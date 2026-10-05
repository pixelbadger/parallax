//! Running studies, and the typed results the host gets back.
//!
//! A study evaluates each candidate policy in worlds `0..n`. World `i` has
//! the same facts for every policy (they are keyed, see `world.rs`), so the
//! comparison is fair by construction, and paired differences between
//! policies are reported with their own intervals.

use std::collections::HashMap;
use std::rc::Rc;

use serde::Serialize;
use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};

use crate::cost::{Analyzer, Cost, PolicyCost};
use crate::error::{Error, ErrorKind, Result};
use crate::eval::{Fault, Machine};
use crate::ir::*;
use crate::stats;
use crate::value::Value;

pub const RUNTIME: &str = concat!("parallax ", env!("CARGO_PKG_VERSION"));

/// What the host allows a program to cost. Checked before anything runs.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_worlds: u64,
    pub max_horizon: u64,
    pub max_forecast_worlds: u64,
    pub max_policies: u64,
    pub max_operations: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_worlds: 1_000_000,
            max_horizon: 1_000_000,
            max_forecast_worlds: 100_000,
            max_policies: 10_000,
            max_operations: 1e11,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Input values by name: numbers in the declared unit, or strings with
    /// a unit (`"5 min"`), arrays, objects for records, strings for enums.
    pub inputs: Map<String, Json>,
    /// Override every study's seed.
    pub seed: Option<i64>,
    /// Override every study's world count.
    pub worlds: Option<u64>,
    /// Run only this study.
    pub study: Option<String>,
    pub limits: Limits,
}

// ---------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub runtime: String,
    pub model_sha256: String,
    pub studies: Vec<StudyReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StudyReport {
    pub study: String,
    pub model: String,
    pub provenance: Provenance,
    pub inputs: Map<String, Json>,
    pub objectives: Vec<ObjectiveSpec>,
    pub constraints: Vec<String>,
    pub policies: Vec<PolicyReport>,
    /// Feasible, error-free, non-oracle policies, best first.
    pub ranking: Vec<String>,
    pub recommended: Option<String>,
    /// The best feasible oracle, an upper bound on what foresight buys.
    pub best_oracle: Option<String>,
    /// Model errors (first few per policy), by world and step.
    pub errors: Vec<ModelError>,
    pub work: Work,
}

#[derive(Clone, Debug, Serialize)]
pub struct Provenance {
    pub runtime: String,
    pub model_sha256: String,
    pub inputs_sha256: String,
    pub seed: i64,
    pub worlds: u64,
    /// Every policy saw exactly these worlds.
    pub world_ids: [u64; 2],
}

#[derive(Clone, Debug, Serialize)]
pub struct ObjectiveSpec {
    pub sense: &'static str,
    pub metric: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicyReport {
    pub name: String,
    pub oracle: bool,
    /// "ok", "infeasible" (a constraint fails) or "model_errors".
    pub status: &'static str,
    pub rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter: Option<Json>,
    /// A decision model's policy decides once: what it chose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Json>,
    pub worlds_completed: u64,
    pub model_errors: u64,
    pub objectives: Vec<MetricValue>,
    pub constraints: Vec<ConstraintResult>,
    pub metrics: Vec<MetricValue>,
    /// The primary objective against the recommended policy, world by world.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_recommended: Option<Paired>,
    /// The primary objective as a fraction of the best oracle's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of_oracle: Option<f64>,
    pub work: PolicyWork,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricValue {
    pub metric: String,
    pub value: Json,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci95: Option<[f64; 2]>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConstraintResult {
    pub constraint: String,
    pub value: Json,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub satisfied: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Paired {
    pub difference: f64,
    pub ci95: Option<[f64; 2]>,
    pub worlds: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelError {
    pub policy: String,
    pub world: Option<u64>,
    pub step: Option<i64>,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicyWork {
    pub estimated_operations: u64,
    pub operations: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Work {
    pub estimated_operations: u64,
    pub estimated_transitions: u64,
    /// Operations actually performed (after a run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operations: Option<u64>,
    pub max_forecast_worlds: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    pub runtime: String,
    pub model_sha256: String,
    pub inputs: Vec<InputSchema>,
    pub worlds: Vec<WorldSchema>,
    pub models: Vec<ModelSchema>,
    pub policies: Vec<PolicySchema>,
    pub studies: Vec<StudyCheck>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InputSchema {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub between: Option<[Json; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Json>,
    pub required: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorldSchema {
    pub name: String,
    pub facts: Vec<FactSchema>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FactSchema {
    pub name: String,
    pub kind: &'static str,
    pub params: Vec<String>,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelSchema {
    pub name: String,
    pub kind: &'static str,
    pub action: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicySchema {
    pub name: String,
    pub model: String,
    pub oracle: bool,
    pub family: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct StudyCheck {
    pub study: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worlds: Option<u64>,
    pub policies: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

// ---------------------------------------------------------------------
// Values, types and units for the host
// ---------------------------------------------------------------------

pub fn ty_name(p: &Program, t: &Ty) -> String {
    match t {
        Ty::Unit => "nothing".into(),
        Ty::Bool => "Bool".into(),
        Ty::Int => "Int".into(),
        Ty::Str => "String".into(),
        Ty::Num(d, h) => {
            if *h != NO_HINT {
                p.hints[*h as usize].name.clone()
            } else if d.is_none() {
                "Float".into()
            } else {
                p.units.canonical(*d)
            }
        }
        Ty::Enum(e) => p.enums[*e as usize].name.clone(),
        Ty::Rec(r) => p.records[*r as usize].name.clone(),
        Ty::Arr(t) => format!("[{}]", ty_name(p, t)),
    }
}

fn scale(p: &Program, t: &Ty) -> f64 {
    match t {
        Ty::Num(_, h) if *h != NO_HINT => p.hints[*h as usize].scale,
        _ => 1.0,
    }
}

pub fn unit_label(p: &Program, t: &Ty) -> Option<String> {
    match t {
        Ty::Num(d, h) => {
            if *h != NO_HINT {
                Some(p.hints[*h as usize].name.clone())
            } else if d.is_none() {
                None
            } else {
                Some(p.units.canonical(*d))
            }
        }
        Ty::Arr(t) => unit_label(p, t),
        _ => None,
    }
}

fn num_json(x: f64) -> Json {
    serde_json::Number::from_f64(x).map_or(Json::Null, Json::Number)
}

/// A value as JSON, quantities in their display unit.
pub fn value_json(p: &Program, v: &Value, t: &Ty) -> Json {
    match (v, t) {
        (Value::Unit, _) => Json::Null,
        (Value::Int(n), Ty::Num(..)) => num_json(*n as f64 / scale(p, t)),
        (Value::Int(n), _) => Json::from(*n),
        (Value::Num(x), _) => num_json(x / scale(p, t)),
        (Value::Bool(b), _) => Json::Bool(*b),
        (Value::Str(s), _) => Json::String(s.to_string()),
        (Value::Variant(_) | Value::Data(_), Ty::Enum(e)) => {
            let (tag, fields) = v.as_variant().unwrap();
            let def = &p.enums[*e as usize].variants[tag as usize];
            match fields.is_empty() {
                true => Json::String(def.name.clone()),
                false => {
                    let mut m = Map::new();
                    m.insert("variant".into(), Json::String(def.name.clone()));
                    for ((n, ft), fv) in def.fields.iter().zip(fields.iter()) {
                        m.insert(n.clone(), value_json(p, fv, ft));
                    }
                    Json::Object(m)
                }
            }
        }
        (Value::Rec(fields), Ty::Rec(r)) => {
            let mut m = Map::new();
            for ((n, ft), fv) in p.records[*r as usize].fields.iter().zip(fields.iter()) {
                m.insert(n.clone(), value_json(p, fv, ft));
            }
            Json::Object(m)
        }
        (Value::Arr(items), Ty::Arr(et)) => {
            Json::Array(items.iter().map(|x| value_json(p, x, et)).collect())
        }
        _ => Json::Null,
    }
}

/// A value with its unit, if it has one: `{"value": 15, "unit": "min"}`.
pub fn typed_json(p: &Program, v: &Value, t: &Ty) -> Json {
    let j = value_json(p, v, t);
    match unit_label(p, t) {
        Some(u) => {
            let mut m = Map::new();
            m.insert("value".into(), j);
            m.insert("unit".into(), Json::String(u));
            Json::Object(m)
        }
        None => j,
    }
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

/// A short label for a policy family's parameter: `5 min`, `full`.
pub fn short(p: &Program, v: &Value, t: &Ty) -> String {
    match (v, t) {
        (Value::Num(x), Ty::Num(..)) => match unit_label(p, t) {
            Some(u) => format!("{} {u}", fmt_num(x / scale(p, t))),
            None => fmt_num(*x),
        },
        (Value::Int(n), _) => n.to_string(),
        _ => match value_json(p, v, t) {
            Json::String(s) => s,
            j => j.to_string(),
        },
    }
}

fn parse_quantity(p: &Program, s: &str) -> std::result::Result<(f64, crate::units::Dim), String> {
    let s = s.trim();
    let split = s
        .char_indices()
        .find(|(i, c)| {
            !(c.is_ascii_digit()
                || *c == '.'
                || *c == '_'
                || ((*c == '-' || *c == '+')
                    && (*i == 0 || matches!(s[..*i].chars().last(), Some('e' | 'E'))))
                || ((*c == 'e' || *c == 'E') && *i > 0))
        })
        .map_or(s.len(), |(i, _)| i);
    let (num, unit) = s.split_at(split);
    let x: f64 = num
        .replace('_', "")
        .parse()
        .map_err(|_| format!("can't read `{s}` as a number with a unit"))?;
    let unit = unit.trim();
    if unit.is_empty() {
        return Ok((x, crate::units::Dim::NONE));
    }
    let mut atoms = Vec::new();
    let mut sign = 1;
    let mut rest = unit;
    loop {
        let end = rest.find(['*', '/', '·']).unwrap_or(rest.len());
        let atom = rest[..end].trim();
        let (name, pow) = match atom.split_once('^') {
            Some((n, e)) => (
                n.trim(),
                e.trim()
                    .parse::<i32>()
                    .map_err(|_| format!("bad power in `{unit}`"))?,
            ),
            None => (atom, 1),
        };
        atoms.push((name.to_string(), pow * sign));
        if end == rest.len() {
            break;
        }
        let op = rest[end..].chars().next().unwrap();
        sign = if op == '/' { -1 } else { 1 };
        rest = &rest[end + op.len_utf8()..];
    }
    let (sc, dim) = p.units.eval(&atoms)?;
    Ok((x * sc, dim))
}

/// A host value for an input of type `t`.
pub fn json_value(p: &Program, j: &Json, t: &Ty, what: &str) -> std::result::Result<Value, String> {
    let bad = || format!("{what} should be {}, not {j}", ty_name(p, t));
    match t {
        Ty::Int => match j {
            Json::Number(n) => n
                .as_i64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|x| x.fract() == 0.0 && x.abs() < 9e18)
                        .map(|x| x as i64)
                })
                .map(Value::Int)
                .ok_or_else(bad),
            _ => Err(bad()),
        },
        Ty::Num(d, _) => match j {
            Json::Number(n) => Ok(Value::Num(n.as_f64().ok_or_else(bad)? * scale(p, t))),
            Json::String(s) => {
                let (x, dim) = parse_quantity(p, s).map_err(|e| format!("{what}: {e}"))?;
                if dim != *d {
                    return Err(format!("{what}: `{s}` isn't in units of {}", ty_name(p, t)));
                }
                Ok(Value::Num(x))
            }
            _ => Err(bad()),
        },
        Ty::Bool => j.as_bool().map(Value::Bool).ok_or_else(bad),
        Ty::Str => j
            .as_str()
            .map(|s| Value::Str(Rc::new(s.to_string())))
            .ok_or_else(bad),
        Ty::Enum(e) => {
            let def = &p.enums[*e as usize];
            let (name, obj) = match j {
                Json::String(s) => (s.as_str(), None),
                Json::Object(m) => (
                    m.get("variant").and_then(Json::as_str).ok_or_else(bad)?,
                    Some(m),
                ),
                _ => return Err(bad()),
            };
            let tag = def
                .variants
                .iter()
                .position(|v| v.name == name)
                .ok_or_else(|| format!("{what}: `{name}` isn't a variant of {}", def.name))?;
            let fields = &def.variants[tag].fields;
            if fields.is_empty() {
                return Ok(Value::Variant(tag as u32));
            }
            let obj = obj.ok_or_else(bad)?;
            let mut vals = Vec::new();
            for (n, ft) in fields {
                let fj = obj
                    .get(n)
                    .ok_or_else(|| format!("{what}: missing field `{n}`"))?;
                vals.push(json_value(p, fj, ft, &format!("{what}.{n}"))?);
            }
            Ok(Value::variant(tag as u32, vals))
        }
        Ty::Rec(r) => {
            let Json::Object(m) = j else {
                return Err(bad());
            };
            let def = &p.records[*r as usize];
            for k in m.keys() {
                if !def.fields.iter().any(|(n, _)| n == k) {
                    return Err(format!("{what}: `{}` has no field `{k}`", def.name));
                }
            }
            let mut vals = Vec::new();
            for (n, ft) in &def.fields {
                let fj = m
                    .get(n)
                    .ok_or_else(|| format!("{what}: missing field `{n}`"))?;
                vals.push(json_value(p, fj, ft, &format!("{what}.{n}"))?);
            }
            Ok(Value::Rec(Rc::new(vals)))
        }
        Ty::Arr(et) => {
            let Json::Array(items) = j else {
                return Err(bad());
            };
            let mut vals = Vec::new();
            for (i, x) in items.iter().enumerate() {
                vals.push(json_value(p, x, et, &format!("{what}[{i}]"))?);
            }
            Ok(Value::arr(vals))
        }
        Ty::Unit => Err(bad()),
    }
}

// ---------------------------------------------------------------------
// Setup: inputs, settings, policies, budget
// ---------------------------------------------------------------------

#[allow(clippy::boxed_local)]
pub(crate) fn fault_err(kind: ErrorKind, f: Box<Fault>, ctx: &str) -> Error {
    Error::line(kind, f.line, format!("{ctx}: {}", f.message))
}

pub fn sha256(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

struct Instance {
    policy: u32,
    name: String,
    param: Option<Value>,
}

struct Setup {
    globals: Vec<Value>,
    inputs: Map<String, Json>,
    seed: i64,
    worlds: u64,
    horizons: Vec<i64>,
    instances: Vec<Instance>,
    costs: Vec<PolicyCost>,
    work: Work,
}

pub(crate) fn input_ids(p: &Program, inputs: &Map<String, Json>) -> Result<()> {
    for k in inputs.keys() {
        if !p
            .globals
            .iter()
            .any(|g| g.kind == GlobalKind::Input && g.name == *k)
        {
            let names: Vec<&str> = p
                .globals
                .iter()
                .filter(|g| g.kind == GlobalKind::Input)
                .map(|g| g.name.as_str())
                .collect();
            return Err(Error::new(
                ErrorKind::Input,
                format!(
                    "unknown input `{k}` (this program's inputs: {})",
                    if names.is_empty() {
                        "none".into()
                    } else {
                        names.join(", ")
                    }
                ),
            ));
        }
    }
    Ok(())
}

fn check_len(p: &Program, m: &mut Machine, g: &GlobalDef, v: &Value) -> Result<()> {
    let spec = g.input.as_ref().unwrap();
    let mut level = vec![v.clone()];
    for (depth, len) in spec.lens.iter().enumerate() {
        let Some(len) = len else {
            level = level.iter().flat_map(|x| x.items().to_vec()).collect();
            continue;
        };
        let want = m
            .code(len)
            .map_err(|f| fault_err(ErrorKind::Runtime, f, &g.name))?
            .as_int();
        for x in &level {
            if x.items().len() as i64 != want {
                return Err(Error::line(
                    ErrorKind::Input,
                    g.line,
                    format!(
                        "input `{}` needs {want} elements at depth {}, not {}",
                        g.name,
                        depth + 1,
                        x.items().len()
                    ),
                ));
            }
        }
        level = level.iter().flat_map(|x| x.items().to_vec()).collect();
    }
    let _ = p;
    Ok(())
}

/// Every global's value: constants, then a study's `with` and the host's
/// inputs (or their defaults), then derived values. Also the inputs as shown.
pub(crate) fn globals(
    p: &Program,
    with: &[(u32, Code)],
    inputs: &Map<String, Json>,
) -> Result<(Vec<Value>, Map<String, Json>)> {
    let mut m = Machine::new(p, vec![Value::Unit; p.globals.len()], 0);
    // Constants first: a study's `with` may use them.
    for &g in &p.global_order {
        let def = &p.globals[g as usize];
        if def.kind == GlobalKind::Const {
            let v = m.code(def.init.as_ref().unwrap()).map_err(|f| {
                fault_err(ErrorKind::Runtime, f, &format!("computing `{}`", def.name))
            })?;
            m.globals[g as usize] = v;
        }
    }
    let mut overrides: HashMap<u32, Value> = HashMap::new();
    for (g, code) in with {
        let v = m
            .code(code)
            .map_err(|f| fault_err(ErrorKind::Runtime, f, "evaluating `with`"))?;
        overrides.insert(*g, v);
    }
    let mut shown = Map::new();
    for &g in &p.global_order {
        let def = &p.globals[g as usize];
        match def.kind {
            GlobalKind::Const => {}
            GlobalKind::Derived => {
                let v = m.code(def.init.as_ref().unwrap()).map_err(|f| {
                    fault_err(ErrorKind::Runtime, f, &format!("computing `{}`", def.name))
                })?;
                m.globals[g as usize] = v;
            }
            GlobalKind::Input => {
                let spec = def.input.as_ref().unwrap();
                let v = if let Some(v) = overrides.remove(&g) {
                    v
                } else if let Some(j) = inputs.get(&def.name) {
                    json_value(p, j, &def.ty, &format!("input `{}`", def.name))
                        .map_err(|msg| Error::new(ErrorKind::Input, msg))?
                } else if let Some(d) = &spec.default {
                    m.code(d)
                        .map_err(|f| fault_err(ErrorKind::Runtime, f, &def.name))?
                } else {
                    return Err(Error::line(
                        ErrorKind::Input,
                        def.line,
                        format!("input `{}` ({}) is required", def.name, ty_name(p, &def.ty)),
                    ));
                };
                if let Some((lo, hi)) = &spec.between {
                    let lo = m
                        .code(lo)
                        .map_err(|f| fault_err(ErrorKind::Runtime, f, &def.name))?;
                    let hi = m
                        .code(hi)
                        .map_err(|f| fault_err(ErrorKind::Runtime, f, &def.name))?;
                    let x = v.as_num();
                    if !(x >= lo.as_num() && x <= hi.as_num()) {
                        return Err(Error::line(
                            ErrorKind::Input,
                            def.line,
                            format!(
                                "input `{}` is {}, outside {}..{}",
                                def.name,
                                short(p, &v, &def.ty),
                                short(p, &lo, &def.ty),
                                short(p, &hi, &def.ty)
                            ),
                        ));
                    }
                }
                check_len(p, &mut m, def, &v)?;
                shown.insert(def.name.clone(), typed_json(p, &v, &def.ty));
                m.globals[g as usize] = v;
            }
        }
    }
    // Shown in declaration order.
    let mut ordered = Map::new();
    for def in &p.globals {
        if let Some(v) = shown.remove(&def.name) {
            ordered.insert(def.name.clone(), v);
        }
    }
    Ok((m.globals, ordered))
}

/// Each sequential model's horizon, within the host's limit.
pub(crate) fn horizons(
    p: &Program,
    m: &mut Machine,
    limits: &Limits,
    line: u32,
) -> Result<Vec<i64>> {
    let mut horizons = vec![0; p.models.len()];
    for (i, model) in p.models.iter().enumerate() {
        if let ModelDef::Sequential(s) = model {
            let h = m
                .code(&s.horizon)
                .map_err(|f| fault_err(ErrorKind::Runtime, f, "the horizon"))?
                .as_int();
            if h < 0 || h as u64 > limits.max_horizon {
                return Err(Error::line(
                    ErrorKind::Budget,
                    line,
                    format!(
                        "model `{}` has horizon {h}; the limit is {}",
                        s.name, limits.max_horizon
                    ),
                ));
            }
            horizons[i] = h;
        }
    }
    Ok(horizons)
}

fn setup(p: &Program, sd: &StudyDef, opts: &Options) -> Result<Setup> {
    let (globals, inputs) = globals(p, &sd.with, &opts.inputs)?;
    let mut m = Machine::new(p, globals.clone(), 0);
    let int = |m: &mut Machine, c: &Option<Code>, what: &str| -> Result<Option<i64>> {
        match c {
            Some(c) => Ok(Some(
                m.code(c)
                    .map_err(|f| fault_err(ErrorKind::Runtime, f, what))?
                    .as_int(),
            )),
            None => Ok(None),
        }
    };
    let seed = match opts.seed {
        Some(s) => s,
        None => int(&mut m, &sd.seed, "the study's seed")?.unwrap_or(0),
    };
    let worlds = match opts.worlds {
        Some(w) => w as i64,
        None => int(&mut m, &sd.worlds, "the study's worlds")?.unwrap_or(1000),
    };
    let budget = |msg: String| Error::line(ErrorKind::Budget, sd.line, msg);
    if worlds < 1 {
        return Err(budget(format!(
            "study `{}` needs at least one world",
            sd.name
        )));
    }
    let worlds = worlds as u64;
    if worlds > opts.limits.max_worlds {
        return Err(budget(format!(
            "study `{}` asks for {worlds} worlds; the limit is {}",
            sd.name, opts.limits.max_worlds
        )));
    }
    let horizons = horizons(p, &mut m, &opts.limits, sd.line)?;
    let mut instances = Vec::new();
    for &pid in &sd.compare {
        let pol = &p.policies[pid as usize];
        match &pol.family {
            None => instances.push(Instance {
                policy: pid,
                name: pol.name.clone(),
                param: None,
            }),
            Some((code, ty)) => {
                let vals = m.code(code).map_err(|f| {
                    fault_err(
                        ErrorKind::Runtime,
                        f,
                        &format!("policy family `{}`", pol.name),
                    )
                })?;
                for v in vals.items() {
                    instances.push(Instance {
                        policy: pid,
                        name: format!("{}[{}]", pol.name, short(p, v, ty)),
                        param: Some(v.clone()),
                    });
                }
            }
        }
    }
    if instances.is_empty() {
        return Err(budget(format!(
            "study `{}` has no policies to compare",
            sd.name
        )));
    }
    if instances.len() as u64 > opts.limits.max_policies {
        return Err(budget(format!(
            "study `{}` compares {} policies; the limit is {}",
            sd.name,
            instances.len(),
            opts.limits.max_policies
        )));
    }
    let mut an = Analyzer::new(p, &globals, horizons.clone());
    let mut costs = Vec::new();
    let mut total = Cost::default();
    for inst in &instances {
        let pc = an.policy(&p.policies[inst.policy as usize], inst.param.as_ref())?;
        total += pc.once + pc.per_world.times(worlds as f64);
        costs.push(pc);
    }
    if an.max_forecast_worlds > opts.limits.max_forecast_worlds {
        return Err(budget(format!(
            "a forecast asks for {} worlds; the limit is {}",
            an.max_forecast_worlds, opts.limits.max_forecast_worlds
        )));
    }
    if !total.ops.is_finite() || total.ops > opts.limits.max_operations {
        return Err(budget(format!(
            "study `{}` could take up to {:.3e} operations ({:.3e} model transitions); the \
             limit is {:.3e}",
            sd.name, total.ops, total.steps, opts.limits.max_operations
        )));
    }
    Ok(Setup {
        globals,
        inputs,
        seed,
        worlds,
        horizons,
        instances,
        costs,
        work: Work {
            estimated_operations: total.ops.ceil() as u64,
            estimated_transitions: total.steps.ceil() as u64,
            operations: None,
            max_forecast_worlds: an.max_forecast_worlds,
        },
    })
}

// ---------------------------------------------------------------------
// Check and run
// ---------------------------------------------------------------------

pub fn check(src: &str, p: &Program, opts: &Options) -> Result<CheckReport> {
    input_ids(p, &opts.inputs)?;
    let mut m = Machine::new(p, vec![Value::Unit; p.globals.len()], 0);
    for &g in &p.global_order {
        let def = &p.globals[g as usize];
        if def.kind == GlobalKind::Const
            && let Ok(v) = m.code(def.init.as_ref().unwrap())
        {
            m.globals[g as usize] = v;
        }
    }
    let inputs = p
        .globals
        .iter()
        .filter(|g| g.kind == GlobalKind::Input)
        .map(|g| {
            let spec = g.input.as_ref().unwrap();
            let mut show = |c: &Code| {
                m.code(c)
                    .map(|v| value_json(p, &v, &g.ty))
                    .unwrap_or(Json::Null)
            };
            InputSchema {
                name: g.name.clone(),
                ty: ty_name(p, &g.ty),
                unit: unit_label(p, &g.ty),
                between: spec.between.as_ref().map(|(lo, hi)| [show(lo), show(hi)]),
                default: spec.default.as_ref().map(&mut show),
                required: spec.default.is_none(),
            }
        })
        .collect();
    let worlds = p
        .worlds
        .iter()
        .map(|w| WorldSchema {
            name: w.clone(),
            facts: p
                .facts
                .iter()
                .filter(|f| f.world == *w)
                .map(|f| FactSchema {
                    name: f.name.clone(),
                    kind: match f.kind {
                        crate::ast::FactKind::Latent => "latent",
                        crate::ast::FactKind::Uncertain => "uncertain",
                        crate::ast::FactKind::Derived => "derived",
                    },
                    params: f.params.iter().map(|t| ty_name(p, t)).collect(),
                    ty: ty_name(p, &f.ty),
                })
                .collect(),
        })
        .collect();
    let models = p
        .models
        .iter()
        .map(|m| match m {
            ModelDef::Decision {
                name,
                action,
                outcome,
                ..
            } => ModelSchema {
                name: name.clone(),
                kind: "decision",
                action: ty_name(p, action),
                outcome: ty_name(p, outcome),
                state: None,
                observation: None,
            },
            ModelDef::Sequential(s) => ModelSchema {
                name: s.name.clone(),
                kind: "sequential",
                action: ty_name(p, &s.action),
                outcome: ty_name(p, &s.outcome),
                state: Some(ty_name(p, &s.state)),
                observation: Some(ty_name(p, &s.obs)),
            },
        })
        .collect();
    let policies = p
        .policies
        .iter()
        .map(|pol| PolicySchema {
            name: pol.name.clone(),
            model: p.models[pol.model as usize].name().to_string(),
            oracle: pol.oracle,
            family: pol.family.is_some(),
        })
        .collect();
    let mut studies = Vec::new();
    for sd in &p.studies {
        if opts.study.as_ref().is_some_and(|s| *s != sd.name) {
            continue;
        }
        let model = p.models[sd.model as usize].name().to_string();
        studies.push(match setup(p, sd, opts) {
            Ok(s) => StudyCheck {
                study: sd.name.clone(),
                model,
                seed: Some(s.seed),
                worlds: Some(s.worlds),
                policies: s.instances.iter().map(|i| i.name.clone()).collect(),
                work: Some(s.work),
                error: None,
            },
            Err(e) => StudyCheck {
                study: sd.name.clone(),
                model,
                seed: None,
                worlds: None,
                policies: sd
                    .compare
                    .iter()
                    .map(|&i| p.policies[i as usize].name.clone())
                    .collect(),
                work: None,
                error: Some(e),
            },
        });
    }
    Ok(CheckReport {
        runtime: RUNTIME.into(),
        model_sha256: sha256(src.as_bytes()),
        inputs,
        worlds,
        models,
        policies,
        studies,
    })
}

pub fn run(src: &str, p: &Program, opts: &Options) -> Result<Report> {
    input_ids(p, &opts.inputs)?;
    if let Some(name) = &opts.study
        && !p.studies.iter().any(|s| s.name == *name)
    {
        return Err(Error::new(
            ErrorKind::Input,
            format!("no study named `{name}`"),
        ));
    }
    if p.studies.is_empty() {
        return Err(Error::new(
            ErrorKind::Check,
            "this program has no study: add `study name { ... }` to say what to compare",
        ));
    }
    let model_sha256 = sha256(src.as_bytes());
    let mut studies = Vec::new();
    for sd in &p.studies {
        if opts.study.as_ref().is_some_and(|s| *s != sd.name) {
            continue;
        }
        studies.push(run_study(p, sd, opts, &model_sha256)?);
    }
    Ok(Report {
        runtime: RUNTIME.into(),
        model_sha256,
        studies,
    })
}

struct Evaluated {
    outcomes: Vec<Option<Value>>,
    action: Option<Value>,
    errors: u64,
    samples: Vec<ModelError>,
    ops: u64,
}

const ERROR_SAMPLES: usize = 5;

fn evaluate(p: &Program, s: &Setup, inst: &Instance) -> Evaluated {
    let pol = &p.policies[inst.policy as usize];
    let mut m = Machine::new(p, s.globals.clone(), s.seed as u64);
    m.horizons = s.horizons.clone();
    let mut ev = Evaluated {
        outcomes: Vec::with_capacity(s.worlds as usize),
        action: None,
        errors: 0,
        samples: Vec::new(),
        ops: 0,
    };
    let fail = |ev: &mut Evaluated, f: Box<Fault>, world: Option<u64>, step: Option<i64>| {
        ev.errors += 1;
        if ev.samples.len() < ERROR_SAMPLES {
            ev.samples.push(ModelError {
                policy: inst.name.clone(),
                world,
                step,
                line: (f.line > 0).then_some(f.line),
                message: f.message,
            });
        }
    };
    let param = inst.param.as_ref();
    match &p.models[pol.model as usize] {
        ModelDef::Decision { func, .. } => {
            let action = if pol.oracle {
                None
            } else {
                match m.decide_once(pol, param) {
                    Ok(a) => Some(a),
                    Err(f) => {
                        fail(&mut ev, f, None, None);
                        ev.errors = s.worlds;
                        ev.outcomes = vec![None; s.worlds as usize];
                        ev.ops = m.ops;
                        return ev;
                    }
                }
            };
            ev.action = action.clone();
            for w in 0..s.worlds {
                match m.run_decision(*func, pol, param, action.as_ref(), w) {
                    Ok(v) => ev.outcomes.push(Some(v)),
                    Err(f) => {
                        fail(&mut ev, f, Some(w), None);
                        ev.outcomes.push(None);
                    }
                }
            }
        }
        ModelDef::Sequential(sm) => {
            let h = s.horizons[pol.model as usize];
            for w in 0..s.worlds {
                match m.run_sequential(sm, h, pol, param, w) {
                    Ok(v) => ev.outcomes.push(Some(v)),
                    Err(f) => {
                        let step = m.step;
                        fail(&mut ev, f, Some(w), step);
                        ev.outcomes.push(None);
                    }
                }
            }
        }
    }
    ev.ops = m.ops;
    ev
}

fn metric_value(
    m: &mut Machine,
    p: &Program,
    metric: &Metric,
    rows: &Rc<Vec<Value>>,
) -> Result<Value> {
    m.rows = Some(rows.clone());
    let r = m.code(&metric.code);
    m.rows = None;
    let _ = p;
    r.map_err(|f| fault_err(ErrorKind::Runtime, f, &format!("metric `{}`", metric.label)))
}

/// The per-world numbers behind a `mean(x)` or `probability(x)`.
fn per_world(
    m: &mut Machine,
    metric: &Metric,
    outcomes: &[Option<Value>],
) -> Result<Vec<Option<f64>>> {
    let Some((_, arg)) = &metric.simple else {
        return Ok(Vec::new());
    };
    let code = Code {
        ex: (**arg).clone(),
        nslots: metric.code.nslots,
    };
    let mut out = Vec::with_capacity(outcomes.len());
    for o in outcomes {
        out.push(match o {
            None => None,
            Some(row) => {
                let v = m.code_with(&code, row.clone()).map_err(|f| {
                    fault_err(ErrorKind::Runtime, f, &format!("metric `{}`", metric.label))
                })?;
                match v {
                    Value::Unit => None,
                    v => Some(v.as_num()),
                }
            }
        });
    }
    Ok(out)
}

fn metric_out(p: &Program, metric: &Metric, v: &Value, ci: Option<[f64; 2]>) -> MetricValue {
    let s = scale(p, &metric.ty);
    MetricValue {
        metric: metric.label.clone(),
        value: value_json(p, v, &metric.ty),
        unit: unit_label(p, &metric.ty),
        ci95: ci.map(|[a, b]| [a / s, b / s]),
    }
}

fn better(maximize: bool, a: f64, b: f64) -> std::cmp::Ordering {
    if maximize {
        b.total_cmp(&a)
    } else {
        a.total_cmp(&b)
    }
}

fn run_study(
    p: &Program,
    sd: &StudyDef,
    opts: &Options,
    model_sha256: &str,
) -> Result<StudyReport> {
    let s = setup(p, sd, opts)?;
    let mut m = Machine::new(p, s.globals.clone(), s.seed as u64);
    m.horizons = s.horizons.clone();

    let mut reports = Vec::new();
    let mut errors = Vec::new();
    let mut objective_values: Vec<Vec<Option<f64>>> = Vec::new();
    let mut primary_rows: Vec<Option<Vec<Option<f64>>>> = Vec::new();
    let mut eligible = Vec::new();
    let mut total_ops = 0;

    for (inst, cost) in s.instances.iter().zip(&s.costs) {
        let pol = &p.policies[inst.policy as usize];
        let ev = evaluate(p, &s, inst);
        total_ops += ev.ops;
        errors.extend(ev.samples.iter().cloned());
        let rows: Rc<Vec<Value>> = Rc::new(ev.outcomes.iter().flatten().cloned().collect());
        let completed = rows.len() as u64;

        let ci_of = |m: &mut Machine, metric: &Metric| -> Result<Option<[f64; 2]>> {
            Ok(match &metric.simple {
                Some((kind, _)) => {
                    let xs: Vec<f64> = per_world(m, metric, &ev.outcomes)?
                        .into_iter()
                        .flatten()
                        .collect();
                    match kind {
                        StatKind::Mean => stats::mean_ci(&xs),
                        _ => stats::wilson(xs.iter().sum(), xs.len() as f64),
                    }
                }
                None => None,
            })
        };

        let mut objectives = Vec::new();
        let mut ovals = Vec::new();
        for (_, metric) in &sd.objectives {
            let v = metric_value(&mut m, p, metric, &rows)?;
            let ci = if rows.is_empty() {
                None
            } else {
                ci_of(&mut m, metric)?
            };
            ovals.push(match v {
                Value::Unit => None,
                ref v => Some(v.as_num()),
            });
            objectives.push(metric_out(p, metric, &v, ci));
        }
        let primary = match sd.objectives.first() {
            Some((_, metric)) if metric.simple.is_some() => {
                Some(per_world(&mut m, metric, &ev.outcomes)?)
            }
            _ => None,
        };

        let mut constraints = Vec::new();
        let mut feasible = true;
        for c in &sd.constraints {
            let v = metric_value(&mut m, p, &c.metric, &rows)?;
            let (bound, satisfied) = match &c.op {
                Some((op, bm)) => {
                    let b = metric_value(&mut m, p, bm, &rows)?;
                    let ok = match (&v, &b) {
                        (Value::Unit, _) | (_, Value::Unit) => None,
                        _ => {
                            let (x, y) = (v.as_num(), b.as_num());
                            Some(match op {
                                CmpOp::Lt => x < y,
                                CmpOp::Gt => x > y,
                                CmpOp::Le => x <= y,
                                CmpOp::Ge => x >= y,
                                CmpOp::Eq => x == y,
                                CmpOp::Ne => x != y,
                            })
                        }
                    };
                    (Some(value_json(p, &b, &c.metric.ty)), ok)
                }
                None => (
                    None,
                    match v {
                        Value::Unit => None,
                        ref v => Some(v.as_bool()),
                    },
                ),
            };
            feasible &= satisfied == Some(true);
            constraints.push(ConstraintResult {
                constraint: c.label.clone(),
                value: value_json(p, &v, &c.metric.ty),
                bound,
                unit: unit_label(p, &c.metric.ty),
                satisfied,
            });
        }

        let mut metrics = Vec::new();
        for metric in &sd.reports {
            let v = metric_value(&mut m, p, metric, &rows)?;
            let ci = if rows.is_empty() {
                None
            } else {
                ci_of(&mut m, metric)?
            };
            metrics.push(metric_out(p, metric, &v, ci));
        }

        let status = if ev.errors > 0 {
            "model_errors"
        } else if !feasible {
            "infeasible"
        } else {
            "ok"
        };
        let ok = status == "ok" && ovals.iter().all(Option::is_some);
        eligible.push(ok);
        objective_values.push(ovals);
        primary_rows.push(primary);
        let model = &p.models[pol.model as usize];
        reports.push(PolicyReport {
            name: inst.name.clone(),
            oracle: pol.oracle,
            status,
            rank: None,
            parameter: inst
                .param
                .as_ref()
                .map(|v| typed_json(p, v, &pol.family.as_ref().unwrap().1)),
            action: ev.action.as_ref().map(|a| typed_json(p, a, model.action())),
            worlds_completed: completed,
            model_errors: ev.errors,
            objectives,
            constraints,
            metrics,
            vs_recommended: None,
            of_oracle: None,
            work: PolicyWork {
                estimated_operations: (cost.once + cost.per_world.times(s.worlds as f64))
                    .ops
                    .ceil() as u64,
                operations: ev.ops,
            },
        });
    }

    // Rank feasible policies by the objectives, in order.
    let order = |idx: &mut Vec<usize>| {
        idx.sort_by(|&a, &b| {
            for (k, (maximize, _)) in sd.objectives.iter().enumerate() {
                let (x, y) = (
                    objective_values[a][k].unwrap(),
                    objective_values[b][k].unwrap(),
                );
                let o = better(*maximize, x, y);
                if o.is_ne() {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        });
    };
    let mut ranked: Vec<usize> = (0..reports.len())
        .filter(|&i| eligible[i] && !reports[i].oracle)
        .collect();
    order(&mut ranked);
    let mut oracles: Vec<usize> = (0..reports.len())
        .filter(|&i| eligible[i] && reports[i].oracle)
        .collect();
    order(&mut oracles);
    for (r, &i) in ranked.iter().enumerate() {
        reports[i].rank = Some(r + 1);
    }
    let recommended = ranked.first().copied();
    let best_oracle = oracles.first().copied();

    if let Some(best) = recommended
        && let Some(base) = primary_rows[best].clone()
    {
        for i in 0..reports.len() {
            if i == best {
                continue;
            }
            if let Some(rows) = &primary_rows[i] {
                let diffs: Vec<f64> = rows
                    .iter()
                    .zip(&base)
                    .filter_map(|(a, b)| Some((*a)? - (*b)?))
                    .collect();
                if diffs.is_empty() {
                    continue;
                }
                let sc = scale(p, &sd.objectives[0].1.ty);
                reports[i].vs_recommended = Some(Paired {
                    difference: stats::mean(&diffs) / sc,
                    ci95: stats::mean_ci(&diffs).map(|[a, b]| [a / sc, b / sc]),
                    worlds: diffs.len() as u64,
                });
            }
        }
    }
    if let (Some(o), Some((maximize, _))) = (best_oracle, sd.objectives.first()) {
        let ov = objective_values[o][0].unwrap();
        for i in 0..reports.len() {
            if reports[i].oracle {
                continue;
            }
            if let Some(v) = objective_values[i].first().copied().flatten()
                && v > 0.0
                && ov > 0.0
            {
                reports[i].of_oracle = Some(if *maximize { v / ov } else { ov / v });
            }
        }
    }

    let inputs_sha256 = sha256(
        serde_json::to_string(&s.inputs)
            .unwrap_or_default()
            .as_bytes(),
    );
    Ok(StudyReport {
        study: sd.name.clone(),
        model: p.models[sd.model as usize].name().to_string(),
        provenance: Provenance {
            runtime: RUNTIME.into(),
            model_sha256: model_sha256.to_string(),
            inputs_sha256,
            seed: s.seed,
            worlds: s.worlds,
            world_ids: [0, s.worlds - 1],
        },
        inputs: s.inputs.clone(),
        objectives: sd
            .objectives
            .iter()
            .map(|(maximize, metric)| ObjectiveSpec {
                sense: if *maximize { "maximize" } else { "minimize" },
                metric: metric.label.clone(),
            })
            .collect(),
        constraints: sd.constraints.iter().map(|c| c.label.clone()).collect(),
        ranking: ranked.iter().map(|&i| reports[i].name.clone()).collect(),
        recommended: recommended.map(|i| reports[i].name.clone()),
        best_oracle: best_oracle.map(|i| reports[i].name.clone()),
        policies: reports,
        errors,
        work: Work {
            operations: Some(total_ops),
            ..s.work
        },
    })
}
