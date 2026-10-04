//! Aggregating multiverse results into ensembles.

use std::rc::Rc;

use crate::ast::{Sym, well_known as wk};
use crate::value::{Struct, Value};

/// One universe's fully observed result.
#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    Int(i64),
    Float(f64),
    Struct(Sym, Box<[(Sym, Sample)]>),
    Array(Box<[Sample]>),
}

/// Numbers aggregate to an `Ensemble`; structs to a struct of the same type
/// whose every field is aggregated; arrays element by element.
pub fn aggregate(samples: Vec<Sample>, rejected: i64) -> Result<Value, String> {
    if let Some(Sample::Array(first)) = samples.first() {
        let len = first.len();
        let mut columns: Vec<Vec<Sample>> = (0..len)
            .map(|_| Vec::with_capacity(samples.len()))
            .collect();
        for s in samples {
            match s {
                Sample::Array(items) if items.len() == len => {
                    for (column, item) in columns.iter_mut().zip(items.into_vec()) {
                        column.push(item);
                    }
                }
                _ => return Err(MIXED.into()),
            }
        }
        let items = columns
            .into_iter()
            .map(|column| aggregate(column, rejected))
            .collect::<Result<Vec<_>, String>>()?;
        return Ok(Value::Array(Rc::new(items)));
    }
    let Some(Sample::Struct(ty, shape)) = samples.first() else {
        if samples.iter().any(|s| matches!(s, Sample::Float(_))) {
            let floats: Option<Vec<f64>> = samples
                .into_iter()
                .map(|s| match s {
                    Sample::Int(n) => Some(n as f64),
                    Sample::Float(x) => Some(x),
                    Sample::Struct(..) | Sample::Array(_) => None,
                })
                .collect();
            return match floats {
                Some(floats) => summarise_floats(floats, rejected),
                None => Err(MIXED.into()),
            };
        }
        let ints: Option<Vec<i64>> = samples
            .into_iter()
            .map(|s| match s {
                Sample::Int(n) => Some(n),
                Sample::Float(_) | Sample::Struct(..) | Sample::Array(_) => None,
            })
            .collect();
        return match ints {
            Some(ints) => summarise(ints, rejected),
            None => Err(MIXED.into()),
        };
    };
    let (ty, names): (Sym, Vec<Sym>) = (*ty, shape.iter().map(|(k, _)| *k).collect());
    let mut columns: Vec<Vec<Sample>> = names
        .iter()
        .map(|_| Vec::with_capacity(samples.len()))
        .collect();
    for s in samples {
        let Sample::Struct(t, fields) = s else {
            return Err(MIXED.into());
        };
        if t != ty {
            return Err(MIXED.into());
        }
        let mut fields = fields.into_vec();
        for (name, column) in names.iter().zip(&mut columns) {
            let i = fields.iter().position(|(k, _)| k == name).ok_or(MIXED)?;
            column.push(fields.swap_remove(i).1);
        }
    }
    let fields = names
        .into_iter()
        .zip(columns)
        .map(|(name, column)| Ok((name, aggregate(column, rejected)?)))
        .collect::<Result<_, String>>()?;
    Ok(Value::Struct(Rc::new(Struct { ty, fields })))
}

const MIXED: &str = "Every universe must produce the same kind of result";

/// Statistics for one stream of integer universe results.
fn summarise(mut samples: Vec<i64>, rejected: i64) -> Result<Value, String> {
    let n = samples.len() as i64;
    let mut stats = vec![Value::Int(n), Value::Int(rejected)];
    if samples.is_empty() {
        stats.resize(wk::ENSEMBLE_FIELDS.len(), Value::None);
    } else {
        let total: i128 = samples.iter().map(|&s| i128::from(s)).sum();
        let hits = samples.iter().filter(|&&s| s != 0).count() as i64;
        let (min, max) = (samples.iter().copied().min(), samples.iter().copied().max());
        let mid = (samples.len() - 1) / 2;
        let median = *samples.select_nth_unstable(mid).1;
        let overflow = || "Integer overflow in multiverse total".to_string();
        stats.extend(
            [
                i64::try_from(total).map_err(|_| overflow())?,
                i64::try_from(round_half_even(total, i128::from(n))).map_err(|_| overflow())?,
                min.unwrap_or_default(),
                max.unwrap_or_default(),
                median,
                hits,
                round_half_even(100 * i128::from(hits), i128::from(n)) as i64,
            ]
            .map(Value::Int),
        );
    }
    let fields = wk::ENSEMBLE_FIELDS.into_iter().zip(stats).collect();
    Ok(Value::Struct(Rc::new(Struct {
        ty: wk::ENSEMBLE,
        fields,
    })))
}

/// Statistics for a stream of numbers where some universe produced a float:
/// as for integers, but `total`, `mean`, `min`, `max` and `median` are
/// floats and `mean` is not rounded. Summed in universe order, so the
/// result doesn't depend on anything but the seed.
fn summarise_floats(mut samples: Vec<f64>, rejected: i64) -> Result<Value, String> {
    let n = samples.len() as i64;
    let mut stats = vec![Value::Int(n), Value::Int(rejected)];
    if samples.is_empty() {
        stats.resize(wk::ENSEMBLE_FIELDS.len(), Value::None);
    } else {
        let total: f64 = samples.iter().sum();
        if !total.is_finite() {
            return Err("Float overflow in multiverse total".into());
        }
        let hits = samples.iter().filter(|&&s| s != 0.0).count() as i64;
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mid = (samples.len() - 1) / 2;
        let median = *samples.select_nth_unstable_by(mid, f64::total_cmp).1;
        stats.extend([total, total / n as f64, min, max, median].map(Value::Float));
        stats.push(Value::Int(hits));
        stats.push(Value::Int(
            round_half_even(100 * i128::from(hits), i128::from(n)) as i64,
        ));
    }
    let fields = wk::ENSEMBLE_FIELDS.into_iter().zip(stats).collect();
    Ok(Value::Struct(Rc::new(Struct {
        ty: wk::ENSEMBLE,
        fields,
    })))
}

/// `num / den` rounded to the nearest integer, ties to even; `den > 0`.
fn round_half_even(num: i128, den: i128) -> i128 {
    let (q, r) = (num.div_euclid(den), num.rem_euclid(den));
    match (2 * r).cmp(&den) {
        std::cmp::Ordering::Less => q,
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Equal => q + (q & 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(v: &Value, name: Sym) -> Value {
        let Value::Struct(s) = v else {
            panic!("not a struct")
        };
        s.get(name).cloned().unwrap()
    }

    fn int(v: Value) -> i64 {
        let Value::Int(n) = v else {
            panic!("not an int: {v:?}")
        };
        n
    }

    #[test]
    fn rounding_ties_to_even() {
        assert_eq!(round_half_even(5, 2), 2);
        assert_eq!(round_half_even(7, 2), 4);
        assert_eq!(round_half_even(-5, 2), -2);
        assert_eq!(round_half_even(-7, 2), -4);
        assert_eq!(round_half_even(2, 3), 1);
    }

    #[test]
    fn summary_statistics() {
        let samples = [3, 0, 7, 1].map(Sample::Int).to_vec();
        let e = aggregate(samples, 2).unwrap();
        let expect = [
            (wk::N, 4),
            (wk::REJECTED, 2),
            (wk::TOTAL, 11),
            (wk::MEAN, 3),
            (wk::MIN, 0),
            (wk::MAX, 7),
        ];
        for (k, v) in expect {
            assert_eq!(int(field(&e, k)), v);
        }
        assert_eq!(int(field(&e, wk::MEDIAN)), 1); // lower median
        assert_eq!(int(field(&e, wk::HITS)), 3);
        assert_eq!(int(field(&e, wk::RATE)), 75);
    }

    #[test]
    fn empty_ensemble_has_no_statistics() {
        let e = aggregate(vec![], 5).unwrap();
        assert_eq!(int(field(&e, wk::REJECTED)), 5);
        assert!(matches!(field(&e, wk::MEAN), Value::None));
    }

    #[test]
    fn mixed_results_are_rejected() {
        let s = Sample::Struct(wk::ENSEMBLE, Box::new([]));
        assert!(aggregate(vec![Sample::Int(1), s.clone()], 0).is_err());
        assert!(aggregate(vec![s, Sample::Int(1)], 0).is_err());
    }
}
