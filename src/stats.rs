//! Statistics over samples: the same code summarises a forecast's array of
//! outcomes inside a policy and the worlds' outcomes in a study.

use crate::ir::StatKind;
use crate::value::Value;

/// Work charged for one statistic over `n` scalar values.
pub fn cost(kind: StatKind, n: u64) -> u64 {
    let log = if kind.sorts() {
        64 - n.max(1).leading_zeros() as u64
    } else {
        0
    };
    n * (1 + log)
}

/// A statistic over rows of the same shape: numbers, conditions, or arrays
/// of them (summarised element by element).
pub fn stat(kind: StatKind, rows: &[&Value], q: f64, ops: &mut u64) -> Result<Value, String> {
    let Some(first) = rows.first() else {
        return Err("no values to summarise".into());
    };
    if let Value::Arr(a) = first {
        let len = a.len();
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let mut col = Vec::with_capacity(rows.len());
            for r in rows {
                let items = r.items();
                if items.len() != len {
                    return Err(format!(
                        "can't summarise arrays of different lengths ({len} and {})",
                        items.len()
                    ));
                }
                col.push(&items[i]);
            }
            out.push(stat(kind, &col, q, ops)?);
        }
        return Ok(Value::arr(out));
    }
    *ops += cost(kind, rows.len() as u64);
    let n = rows.len() as f64;
    match first {
        Value::Bool(_) => {
            let hits = rows.iter().filter(|v| v.as_bool()).count();
            Ok(match kind {
                StatKind::Count => Value::Int(hits as i64),
                _ => Value::Num(hits as f64 / n),
            })
        }
        Value::Int(_) if matches!(kind, StatKind::Min | StatKind::Max) => {
            let it = rows.iter().map(|v| v.as_int());
            Ok(Value::Int(if kind == StatKind::Min {
                it.min().unwrap()
            } else {
                it.max().unwrap()
            }))
        }
        _ => {
            let xs: Vec<f64> = rows.iter().map(|v| v.as_num()).collect();
            let x = numbers(kind, xs, q)?;
            if !x.is_finite() {
                return Err("a statistic overflowed".into());
            }
            Ok(Value::Num(x))
        }
    }
}

fn check_level(q: f64, cvar: bool) -> Result<(), String> {
    let ok = if cvar {
        (0.0..1.0).contains(&q)
    } else {
        (0.0..=1.0).contains(&q)
    };
    if ok {
        Ok(())
    } else {
        Err(format!("level {q} is outside 0..1"))
    }
}

pub fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

pub fn variance(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64
}

/// Linear interpolation between order statistics (R's type 7).
pub fn quantile_sorted(xs: &[f64], q: f64) -> f64 {
    let h = (xs.len() - 1) as f64 * q;
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(xs.len() - 1);
    xs[lo] + (h - lo as f64) * (xs[hi] - xs[lo])
}

fn numbers(kind: StatKind, mut xs: Vec<f64>, q: f64) -> Result<f64, String> {
    Ok(match kind {
        StatKind::Mean | StatKind::Probability => mean(&xs),
        StatKind::Variance => variance(&xs),
        StatKind::StdDev => variance(&xs).sqrt(),
        StatKind::Min => xs.iter().copied().fold(f64::INFINITY, f64::min),
        StatKind::Max => xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        StatKind::Count => xs.len() as f64,
        StatKind::Median | StatKind::Quantile => {
            let q = if kind == StatKind::Median { 0.5 } else { q };
            check_level(q, false)?;
            xs.sort_by(f64::total_cmp);
            quantile_sorted(&xs, q)
        }
        StatKind::Cvar => {
            check_level(q, true)?;
            xs.sort_by(f64::total_cmp);
            let k = (((1.0 - q) * xs.len() as f64).ceil() as usize).clamp(1, xs.len());
            mean(&xs[xs.len() - k..])
        }
    })
}

/// 95% interval for a mean (normal approximation).
pub fn mean_ci(xs: &[f64]) -> Option<[f64; 2]> {
    if xs.len() < 2 {
        return None;
    }
    let m = mean(xs);
    let h = 1.959_963_984_540_054 * (variance(xs) / xs.len() as f64).sqrt();
    Some([m - h, m + h])
}

/// 95% Wilson interval for a proportion.
pub fn wilson(hits: f64, n: f64) -> Option<[f64; 2]> {
    if n < 1.0 {
        return None;
    }
    let z = 1.959_963_984_540_054_f64;
    let p = hits / n;
    let denom = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denom;
    let h = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    Some([(centre - h).max(0.0), (centre + h).min(1.0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(xs: &[f64]) -> Vec<Value> {
        xs.iter().map(|x| Value::Num(*x)).collect()
    }

    fn run(kind: StatKind, xs: &[f64], q: f64) -> f64 {
        let vals = nums(xs);
        let refs: Vec<&Value> = vals.iter().collect();
        stat(kind, &refs, q, &mut 0).unwrap().as_num()
    }

    #[test]
    fn quantiles_and_tails() {
        let xs = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert_eq!(run(StatKind::Median, &xs, 0.0), 3.0);
        assert_eq!(run(StatKind::Quantile, &xs, 0.25), 2.0);
        assert_eq!(run(StatKind::Cvar, &xs, 0.8), 100.0);
        assert_eq!(run(StatKind::Cvar, &xs, 0.6), 52.0);
        assert_eq!(run(StatKind::Mean, &xs, 0.0), 22.0);
    }

    #[test]
    fn arrays_are_summarised_elementwise() {
        let rows = [
            Value::arr(vec![Value::Int(1), Value::Int(10)]),
            Value::arr(vec![Value::Int(3), Value::Int(30)]),
        ];
        let refs: Vec<&Value> = rows.iter().collect();
        let m = stat(StatKind::Mean, &refs, 0.0, &mut 0).unwrap();
        assert_eq!(m, Value::arr(vec![Value::Num(2.0), Value::Num(20.0)]));
    }
}
