//! Physical units and dimensions.
//!
//! A quantity is stored as an `f64` in its dimension's base unit (seconds,
//! metres, kilograms, °C, amperes, pounds, dollars, euros, or a program's
//! own base units). Types carry the dimension, which the checker enforces,
//! and a display hint: the unit the program wrote, used for output.
//!
//! °C is treated as a linear unit. There is no kelvin or °F, because
//! converting absolute temperatures between them isn't a scaling.

use std::collections::HashMap;

pub const NDIM: usize = 16;
/// Base dimensions before the program's own (`unit rabbit`).
const BUILTIN_BASES: [&str; 8] = ["s", "m", "kg", "°C", "A", "£", "$", "€"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Dim(pub [i8; NDIM]);

impl Dim {
    pub const NONE: Dim = Dim([0; NDIM]);

    pub fn base(i: usize) -> Dim {
        let mut d = [0; NDIM];
        d[i] = 1;
        Dim(d)
    }

    pub fn is_none(&self) -> bool {
        *self == Dim::NONE
    }

    pub fn times(self, o: Dim) -> Dim {
        let mut d = self.0;
        for (a, b) in d.iter_mut().zip(o.0) {
            *a += b;
        }
        Dim(d)
    }

    pub fn per(self, o: Dim) -> Dim {
        let mut d = self.0;
        for (a, b) in d.iter_mut().zip(o.0) {
            *a -= b;
        }
        Dim(d)
    }

    pub fn pow(self, n: i32) -> Dim {
        let mut d = self.0;
        for a in d.iter_mut() {
            *a = (*a as i32 * n) as i8;
        }
        Dim(d)
    }

    /// Half every exponent, if they are all even.
    pub fn sqrt(self) -> Option<Dim> {
        let mut d = self.0;
        for a in d.iter_mut() {
            if *a % 2 != 0 {
                return None;
            }
            *a /= 2;
        }
        Some(Dim(d))
    }
}

#[derive(Clone, Debug)]
pub struct UnitDef {
    pub name: String,
    pub scale: f64,
    pub dim: Dim,
}

#[derive(Clone, Debug)]
pub struct Units {
    pub defs: Vec<UnitDef>,
    map: HashMap<String, usize>,
    pub base_names: Vec<String>,
}

fn d(exps: &[(usize, i8)]) -> Dim {
    let mut out = [0; NDIM];
    for &(i, e) in exps {
        out[i] = e;
    }
    Dim(out)
}

/// Every built-in unit: name, scale to the base unit, dimension.
fn builtins() -> Vec<(&'static str, f64, Dim)> {
    let (s, m, kg, c, a, gbp, usd, eur) = (0, 1, 2, 3, 4, 5, 6, 7);
    let time = d(&[(s, 1)]);
    let length = d(&[(m, 1)]);
    let mass = d(&[(kg, 1)]);
    let energy = d(&[(kg, 1), (m, 2), (s, -2)]);
    let power = d(&[(kg, 1), (m, 2), (s, -3)]);
    let force = d(&[(kg, 1), (m, 1), (s, -2)]);
    let current = d(&[(a, 1)]);
    let charge = d(&[(a, 1), (s, 1)]);
    let volt = d(&[(kg, 1), (m, 2), (s, -3), (a, -1)]);
    vec![
        ("s", 1.0, time),
        ("ms", 1e-3, time),
        ("min", 60.0, time),
        ("h", 3600.0, time),
        ("day", 86400.0, time),
        ("week", 604800.0, time),
        ("m", 1.0, length),
        ("km", 1e3, length),
        ("cm", 1e-2, length),
        ("mm", 1e-3, length),
        ("kg", 1.0, mass),
        ("g", 1e-3, mass),
        ("°C", 1.0, d(&[(c, 1)])),
        ("degC", 1.0, d(&[(c, 1)])),
        ("A", 1.0, current),
        ("Ah", 3600.0, charge),
        ("V", 1.0, volt),
        ("J", 1.0, energy),
        ("kJ", 1e3, energy),
        ("MJ", 1e6, energy),
        ("Wh", 3600.0, energy),
        ("kWh", 3.6e6, energy),
        ("MWh", 3.6e9, energy),
        ("W", 1.0, power),
        ("kW", 1e3, power),
        ("MW", 1e6, power),
        ("N", 1.0, force),
        ("Hz", 1.0, d(&[(s, -1)])),
        ("£", 1.0, d(&[(gbp, 1)])),
        ("p", 0.01, d(&[(gbp, 1)])),
        ("$", 1.0, d(&[(usd, 1)])),
        ("¢", 0.01, d(&[(usd, 1)])),
        ("€", 1.0, d(&[(eur, 1)])),
        ("%", 0.01, Dim::NONE),
    ]
}

/// Units named for a whole dimension, preferred when displaying one.
const NAMED: [&str; 5] = ["W", "J", "N", "V", "Hz"];

pub fn is_builtin_unit(name: &str) -> bool {
    builtins().iter().any(|(n, _, _)| *n == name)
}

impl Default for Units {
    fn default() -> Self {
        Self::new()
    }
}

impl Units {
    pub fn new() -> Units {
        let mut u = Units {
            defs: Vec::new(),
            map: HashMap::new(),
            base_names: BUILTIN_BASES.iter().map(|s| s.to_string()).collect(),
        };
        for (name, scale, dim) in builtins() {
            u.add(name, scale, dim);
        }
        u
    }

    fn add(&mut self, name: &str, scale: f64, dim: Dim) -> usize {
        let i = self.defs.len();
        self.defs.push(UnitDef {
            name: name.to_string(),
            scale,
            dim,
        });
        self.map.insert(name.to_string(), i);
        i
    }

    pub fn lookup(&self, name: &str) -> Option<usize> {
        self.map.get(name).copied()
    }

    /// `unit rabbit`: a new base dimension.
    pub fn add_base(&mut self, name: &str) -> Result<usize, String> {
        if self.map.contains_key(name) {
            return Err(format!("unit `{name}` is already defined"));
        }
        if self.base_names.len() >= NDIM {
            return Err(format!("too many base units (at most {NDIM})"));
        }
        let dim = Dim::base(self.base_names.len());
        self.base_names.push(name.to_string());
        Ok(self.add(name, 1.0, dim))
    }

    /// `unit AU = 1.496e11 m`.
    pub fn add_derived(&mut self, name: &str, scale: f64, dim: Dim) -> Result<usize, String> {
        if self.map.contains_key(name) {
            return Err(format!("unit `{name}` is already defined"));
        }
        Ok(self.add(name, scale, dim))
    }

    /// Scale and dimension of a written unit.
    pub fn eval(&self, ue: &[(String, i32)]) -> Result<(f64, Dim), String> {
        let mut scale = 1.0;
        let mut dim = Dim::NONE;
        for (name, p) in ue {
            let i = self
                .lookup(name)
                .ok_or_else(|| format!("unknown unit `{name}`"))?;
            let def = &self.defs[i];
            scale *= def.scale.powi(*p);
            dim = dim.times(def.dim.pow(*p));
        }
        Ok((scale, dim))
    }

    /// The unit a value of this dimension is shown in when no hint says:
    /// base units, with a named unit factored out if that's simpler
    /// (`£/J` rather than `s^2·£/m^2·kg`).
    pub fn canonical(&self, dim: Dim) -> String {
        if dim.is_none() {
            return String::new();
        }
        let weight = |d: Dim| d.0.iter().map(|e| e.unsigned_abs() as u32).sum::<u32>();
        let mut best: (u32, Option<(&str, i32)>, Dim) = (weight(dim), None, dim);
        for name in NAMED {
            let nd = self.defs[self.map[name]].dim;
            for e in [1, -1] {
                let rest = dim.per(nd.pow(e));
                let w = weight(rest) + 2;
                if w < best.0 {
                    best = (w, Some((name, e)), rest);
                }
            }
        }
        let (_, named, rest) = best;
        let mut atoms: Vec<(String, i32)> = rest
            .0
            .iter()
            .enumerate()
            .filter(|(_, e)| **e != 0)
            .map(|(i, e)| (self.base_names[i].clone(), *e as i32))
            .collect();
        if let Some((n, e)) = named {
            atoms.push((n.to_string(), e));
        }
        unit_name(&atoms)
    }
}

/// `[("p", 1), ("kWh", -1)]` is "p/kWh".
pub fn unit_name(atoms: &[(String, i32)]) -> String {
    let part = |list: Vec<&(String, i32)>| -> String {
        list.iter()
            .map(|(n, e)| {
                let e = e.abs();
                if e == 1 {
                    n.clone()
                } else {
                    format!("{n}^{e}")
                }
            })
            .collect::<Vec<_>>()
            .join("·")
    };
    let pos: Vec<_> = atoms.iter().filter(|(_, e)| *e > 0).collect();
    let neg: Vec<_> = atoms.iter().filter(|(_, e)| *e < 0).collect();
    match (pos.is_empty(), neg.is_empty()) {
        (_, true) => part(pos),
        (true, false) => format!("1/{}", part(neg)),
        (false, false) => format!("{}/{}", part(pos), part(neg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_combine() {
        let u = Units::new();
        let (scale, dim) = u.eval(&[("p".into(), 1), ("kWh".into(), -1)]).unwrap();
        assert!((scale - 0.01 / 3.6e6).abs() < 1e-20);
        let (_, money) = u.eval(&[("£".into(), 1)]).unwrap();
        let (_, energy) = u.eval(&[("J".into(), 1)]).unwrap();
        assert_eq!(dim, money.per(energy));
        assert_eq!(u.canonical(energy), "J");
        assert_eq!(u.canonical(dim), "£/J");
        let (_, accel) = u.eval(&[("m".into(), 1), ("s".into(), -2)]).unwrap();
        assert_eq!(u.canonical(accel), "m/s^2");
    }
}
