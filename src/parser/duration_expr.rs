// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Duration expressions: Prometheus's experimental `promql-duration-expr`
//! feature, where a range, a subquery step or an offset is arithmetic over
//! number and duration literals, `step()`, `min(a, b)` and `max(a, b)`:
//! `foo[5m + 1m]`, `foo offset (2 * 1h)`, `foo[step() * 2]`.
//!
//! They are parsed only when [`crate::parser::ParseOptions::duration_expr`]
//! is set; otherwise they are refused like Prometheus refuses them without
//! `--enable-feature=promql-duration-expr`.

use std::fmt;
use std::time::Duration;

use crate::parser::token::{token_display, TokenId, T_ADD, T_DIV, T_MOD, T_MUL, T_POW, T_SUB};
use crate::util::display_duration;

/// A duration expression. Values are in seconds.
#[derive(Debug, Clone, PartialEq)]
pub enum DurationExpr {
    /// A number literal, in seconds (`30`, `1.5`).
    Number(f64),
    /// A duration literal (`5m`, `1h30m`).
    Duration(Duration),
    /// `step()`: the query's resolution step (0 for an instant query).
    Step,
    /// Unary minus.
    Neg(Box<DurationExpr>),
    /// `lhs op rhs`, op one of `+ - * / % ^`.
    Binary {
        op: TokenId,
        lhs: Box<DurationExpr>,
        rhs: Box<DurationExpr>,
    },
    /// `min(a, b)`.
    Min(Box<DurationExpr>, Box<DurationExpr>),
    /// `max(a, b)`.
    Max(Box<DurationExpr>, Box<DurationExpr>),
    /// A parenthesized expression.
    Paren(Box<DurationExpr>),
}

impl DurationExpr {
    pub(crate) fn binary(op: TokenId, lhs: DurationExpr, rhs: DurationExpr) -> Self {
        DurationExpr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// A plain literal: what the grammar accepted before duration
    /// expressions (`5m`, `30`).
    pub fn is_literal(&self) -> bool {
        matches!(self, DurationExpr::Number(_) | DurationExpr::Duration(_))
    }

    /// A literal with an optional sign (`-5m`), as an offset accepts it
    /// without duration expressions.
    pub fn is_signed_literal(&self) -> bool {
        match self {
            DurationExpr::Neg(e) => e.is_literal(),
            e => e.is_literal(),
        }
    }

    /// Whether `step()` appears anywhere in the expression, so its value is
    /// only known once the query's step is.
    pub fn uses_step(&self) -> bool {
        match self {
            DurationExpr::Number(_) | DurationExpr::Duration(_) => false,
            DurationExpr::Step => true,
            DurationExpr::Neg(e) | DurationExpr::Paren(e) => e.uses_step(),
            DurationExpr::Binary { lhs, rhs, .. }
            | DurationExpr::Min(lhs, rhs)
            | DurationExpr::Max(lhs, rhs) => lhs.uses_step() || rhs.uses_step(),
        }
    }

    /// The value in seconds, with `step()` = `step`.
    pub fn eval(&self, step: Duration) -> Result<f64, String> {
        Ok(match self {
            DurationExpr::Number(n) => *n,
            DurationExpr::Duration(d) => d.as_secs_f64(),
            DurationExpr::Step => step.as_secs_f64(),
            DurationExpr::Neg(e) => -e.eval(step)?,
            DurationExpr::Paren(e) => e.eval(step)?,
            DurationExpr::Min(a, b) => a.eval(step)?.min(b.eval(step)?),
            DurationExpr::Max(a, b) => a.eval(step)?.max(b.eval(step)?),
            DurationExpr::Binary { op, lhs, rhs } => {
                let (l, r) = (lhs.eval(step)?, rhs.eval(step)?);
                match *op {
                    T_ADD => l + r,
                    T_SUB => l - r,
                    T_MUL => l * r,
                    T_DIV if r == 0.0 => return Err("division by zero".into()),
                    T_DIV => l / r,
                    T_MOD if r == 0.0 => return Err("modulo by zero".into()),
                    T_MOD => l % r,
                    T_POW => l.powf(r),
                    op => {
                        return Err(format!(
                            "unexpected duration expression operator '{}'",
                            token_display(op)
                        ))
                    }
                }
            }
        })
    }

    /// The value as signed milliseconds (truncated, as Prometheus does),
    /// refusing a value out of range, and one that is not positive unless
    /// `allow_negative`.
    pub fn eval_millis(&self, step: Duration, allow_negative: bool) -> Result<i128, String> {
        let secs = self.eval(step)?;
        if secs.is_nan() || (secs <= 0.0 && !allow_negative) {
            return Err("duration must be greater than 0".into());
        }
        if !(-9_223_372_036.854_775..=9_223_372_036.854_775).contains(&secs) {
            return Err("duration is out of range".into());
        }
        Ok((secs * 1000.0) as i128)
    }

    /// The value as a positive duration (a range or a subquery step).
    pub fn eval_duration(&self, step: Duration) -> Result<Duration, String> {
        let ms = self.eval_millis(step, false)?;
        Ok(Duration::from_millis(ms as u64))
    }
}

impl fmt::Display for DurationExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationExpr::Number(n) => write!(f, "{n}"),
            DurationExpr::Duration(d) => write!(f, "{}", display_duration(d)),
            DurationExpr::Step => write!(f, "step()"),
            DurationExpr::Neg(e) => write!(f, "-{e}"),
            DurationExpr::Paren(e) => write!(f, "({e})"),
            DurationExpr::Min(a, b) => write!(f, "min({a}, {b})"),
            DurationExpr::Max(a, b) => write!(f, "max({a}, {b})"),
            DurationExpr::Binary { op, lhs, rhs } => {
                write!(f, "{lhs} {} {rhs}", token_display(*op))
            }
        }
    }
}

#[cfg(feature = "ser")]
impl serde::Serialize for DurationExpr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: f64) -> DurationExpr {
        DurationExpr::Number(v)
    }

    #[test]
    fn eval_arithmetic() {
        let step = Duration::from_secs(10);
        let e = DurationExpr::binary(T_ADD, DurationExpr::Duration(Duration::from_secs(60)), n(1.0));
        assert_eq!(e.eval(step), Ok(61.0));
        assert_eq!(DurationExpr::binary(T_MOD, n(5400.0), n(3600.0)).eval(step), Ok(1800.0));
        assert_eq!(DurationExpr::binary(T_POW, n(2.0), n(3.0)).eval(step), Ok(8.0));
        assert_eq!(
            DurationExpr::Min(Box::new(DurationExpr::Step), Box::new(n(1.0))).eval(step),
            Ok(1.0)
        );
        assert_eq!(
            DurationExpr::Max(Box::new(DurationExpr::Step), Box::new(n(1.0))).eval(step),
            Ok(10.0)
        );
        assert!(DurationExpr::binary(T_DIV, n(1.0), n(0.0)).eval(step).is_err());
        assert!(DurationExpr::binary(T_MOD, n(1.0), n(0.0)).eval(step).is_err());
    }

    #[test]
    fn eval_limits() {
        let z = Duration::ZERO;
        assert!(DurationExpr::Neg(Box::new(n(1.0))).eval_duration(z).is_err());
        assert_eq!(DurationExpr::Neg(Box::new(n(1.5))).eval_millis(z, true), Ok(-1500));
        assert!(n(1e300).eval_millis(z, true).is_err());
        assert!(DurationExpr::Step.eval_duration(z).is_err());
    }

    #[test]
    fn display() {
        let e = DurationExpr::binary(
            T_MUL,
            DurationExpr::Paren(Box::new(DurationExpr::binary(
                T_ADD,
                DurationExpr::Duration(Duration::from_secs(570)),
                DurationExpr::Duration(Duration::from_secs(30)),
            ))),
            n(3.0),
        );
        assert_eq!(e.to_string(), "(9m30s + 30s) * 3");
        assert!(!e.uses_step());
        assert!(DurationExpr::Neg(Box::new(DurationExpr::Step)).uses_step());
    }
}
