//! Work bounds, computed before anything runs.
//!
//! An abstract interpreter over the checked program: integers are tracked
//! as intervals and arrays by their length range, which is enough to bound
//! every loop (loops only run over ranges and arrays, and there is no
//! recursion). It charges exactly what `eval.rs` counts in `ops` (one per
//! node, statement, iteration and step), so the bound is a true upper
//! bound on the real count, which the tests check.

use std::collections::HashMap;
use std::ops::{Add, AddAssign};

use crate::error::{Error, ErrorKind};
use crate::ir::*;
use crate::stats;
use crate::value::Value;

const INF: u64 = u64::MAX;

#[derive(Clone, Debug, PartialEq)]
pub enum Abs {
    /// Anything of its type: floats, booleans, enums, unknown integers.
    Top,
    /// An integer in `lo..=hi` (`i64::MIN`/`MAX` for unbounded).
    Int(i64, i64),
    /// An array with between `lo` and `hi` elements (`INF` for unbounded).
    Arr(u64, u64, Box<Abs>),
    Rec(Vec<Abs>),
}

impl Abs {
    pub fn of(v: &Value) -> Abs {
        match v {
            Value::Int(n) => Abs::Int(*n, *n),
            Value::Arr(items) => {
                let mut elem: Option<Abs> = None;
                for x in items.iter() {
                    let a = Abs::of(x);
                    elem = Some(match elem {
                        None => a,
                        Some(e) => join(&e, &a),
                    });
                }
                let n = items.len() as u64;
                Abs::Arr(n, n, Box::new(elem.unwrap_or(Abs::Top)))
            }
            Value::Rec(fs) => Abs::Rec(fs.iter().map(Abs::of).collect()),
            _ => Abs::Top,
        }
    }

    fn int_range(&self) -> (i64, i64) {
        match self {
            Abs::Int(a, b) => (*a, *b),
            _ => (i64::MIN, i64::MAX),
        }
    }
}

pub fn join(a: &Abs, b: &Abs) -> Abs {
    match (a, b) {
        (Abs::Int(a1, b1), Abs::Int(a2, b2)) => Abs::Int(*a1.min(a2), *b1.max(b2)),
        (Abs::Arr(l1, h1, e1), Abs::Arr(l2, h2, e2)) => {
            Abs::Arr(*l1.min(l2), *h1.max(h2), Box::new(join(e1, e2)))
        }
        (Abs::Rec(x), Abs::Rec(y)) if x.len() == y.len() => {
            Abs::Rec(x.iter().zip(y).map(|(a, b)| join(a, b)).collect())
        }
        _ if a == b => a.clone(),
        _ => Abs::Top,
    }
}

/// Like `join`, but anything still growing jumps to unbounded.
fn widen(old: &Abs, new: &Abs) -> Abs {
    match (old, new) {
        (Abs::Int(a1, b1), Abs::Int(a2, b2)) => Abs::Int(
            if a2 < a1 { i64::MIN } else { *a1 },
            if b2 > b1 { i64::MAX } else { *b1 },
        ),
        (Abs::Arr(l1, h1, e1), Abs::Arr(l2, h2, e2)) => Abs::Arr(
            if l2 < l1 { 0 } else { *l1 },
            if h2 > h1 { INF } else { *h1 },
            Box::new(widen(e1, e2)),
        ),
        (Abs::Rec(x), Abs::Rec(y)) if x.len() == y.len() => {
            Abs::Rec(x.iter().zip(y).map(|(a, b)| widen(a, b)).collect())
        }
        _ if old == new => old.clone(),
        _ => Abs::Top,
    }
}

fn join_env(a: &[Abs], b: &[Abs]) -> Vec<Abs> {
    a.iter().zip(b).map(|(x, y)| join(x, y)).collect()
}

fn widen_env(a: &[Abs], b: &[Abs]) -> Vec<Abs> {
    a.iter().zip(b).map(|(x, y)| widen(x, y)).collect()
}

fn clamp(x: i128) -> i64 {
    x.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Leaf values per row, for a statistic over arrays.
fn cells(a: &Abs) -> f64 {
    match a {
        Abs::Arr(_, h, e) => {
            if *h == INF {
                f64::INFINITY
            } else {
                *h as f64 * cells(e)
            }
        }
        _ => 1.0,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Operations, as `eval.rs` counts them.
    pub ops: f64,
    /// Model transitions: sequential steps, decision-model evaluations.
    pub steps: f64,
}

impl Cost {
    const ONE: Cost = Cost {
        ops: 1.0,
        steps: 0.0,
    };

    fn ops(n: f64) -> Cost {
        Cost { ops: n, steps: 0.0 }
    }

    pub fn times(self, n: f64) -> Cost {
        let m = |x: f64| if x == 0.0 || n == 0.0 { 0.0 } else { x * n };
        Cost {
            ops: m(self.ops),
            steps: m(self.steps),
        }
    }

    fn max(self, o: Cost) -> Cost {
        Cost {
            ops: self.ops.max(o.ops),
            steps: self.steps.max(o.steps),
        }
    }
}

impl Add for Cost {
    type Output = Cost;
    fn add(self, o: Cost) -> Cost {
        Cost {
            ops: self.ops + o.ops,
            steps: self.steps + o.steps,
        }
    }
}

impl AddAssign for Cost {
    fn add_assign(&mut self, o: Cost) {
        *self = *self + o;
    }
}

type AR<T> = Result<T, Error>;

fn unbounded(line: u32, what: &str) -> Error {
    Error::line(
        ErrorKind::Budget,
        line,
        format!(
            "can't bound the work: {what} must be known before the study runs (from \
             constants, inputs or the policy's parameter). Loop to a fixed bound and stop \
             early with `for i in 0..n while cond`"
        ),
    )
}

pub struct Analyzer<'p> {
    p: &'p Program,
    globals: Vec<Abs>,
    horizons: Vec<i64>,
    memo: HashMap<u32, Vec<(Vec<Abs>, Abs, Cost)>>,
    fact_memo: HashMap<u32, Vec<(Vec<Abs>, Abs, Cost)>>,
    states: HashMap<u32, Abs>,
    /// What the policy being analysed decides from.
    decision: Option<Abs>,
    /// The most worlds any forecast asks for.
    pub max_forecast_worlds: u64,
}

/// What evaluating one policy in a study costs.
#[derive(Clone, Copy, Debug, Default)]
pub struct PolicyCost {
    /// Once per study (a decision model's policy, which sees no world).
    pub once: Cost,
    pub per_world: Cost,
}

impl<'p> Analyzer<'p> {
    pub fn new(p: &'p Program, globals: &[Value], horizons: Vec<i64>) -> Analyzer<'p> {
        Analyzer {
            p,
            globals: globals.iter().map(Abs::of).collect(),
            horizons,
            memo: HashMap::new(),
            fact_memo: HashMap::new(),
            states: HashMap::new(),
            decision: None,
            max_forecast_worlds: 0,
        }
    }

    pub fn policy(&mut self, pol: &PolicyDef, family: Option<&Value>) -> AR<PolicyCost> {
        let fam: Vec<Abs> = family.map(Abs::of).into_iter().collect();
        let model = &self.p.models[pol.model as usize];
        match model {
            ModelDef::Decision { func, .. } => {
                self.decision = Some(Abs::Top);
                let (action, pc) = self.call(pol.func, fam)?;
                self.decision = None;
                let (_, mc) = self.call(*func, vec![action])?;
                let model = mc
                    + Cost {
                        ops: 0.0,
                        steps: 1.0,
                    };
                Ok(if pol.oracle {
                    PolicyCost {
                        once: Cost::default(),
                        per_world: pc + model,
                    }
                } else {
                    PolicyCost {
                        once: pc,
                        per_world: model,
                    }
                })
            }
            ModelDef::Sequential(m) => {
                let h = self.horizons[pol.model as usize].max(0);
                let t = Abs::Int(0, (h - 1).max(0));
                let (s0, c_init) = self.call(m.init, vec![])?;
                let s = self.state(pol.model, m, s0)?;
                let (seen, c_obs) = if pol.oracle {
                    (s.clone(), Cost::default())
                } else {
                    self.call(m.observe, vec![s.clone(), t.clone()])?
                };
                self.decision = Some(seen.clone());
                let mut args = fam;
                args.push(seen);
                args.push(t.clone());
                let (_, c_pol) = self.call(pol.func, args)?;
                self.decision = None;
                let c_step = self.step_cost(m, &s, &t)?;
                let c_out = match m.outcome_fn {
                    Some(f) => self.call(f, vec![s.clone()])?.1,
                    None => Cost::default(),
                };
                let per_step = Cost::ONE + c_obs + c_pol + c_step;
                Ok(PolicyCost {
                    once: Cost::default(),
                    per_world: c_init + per_step.times(h as f64) + c_out,
                })
            }
        }
    }

    /// One served decision: the policy, deciding from `seen` (what a
    /// sequential policy observes) at step `t`.
    pub fn decision(
        &mut self,
        pol: &PolicyDef,
        family: Option<&Value>,
        seen: Option<(&Value, i64)>,
    ) -> AR<Cost> {
        let mut args: Vec<Abs> = family.map(Abs::of).into_iter().collect();
        let seen = match seen {
            Some((o, t)) => {
                let o = Abs::of(o);
                args.push(o.clone());
                args.push(Abs::Int(t, t));
                o
            }
            None => Abs::Top,
        };
        self.decision = Some(seen);
        let r = self.call(pol.func, args);
        self.decision = None;
        Ok(r?.1)
    }

    /// `stop`, `step` and the invariants, once; and one transition.
    fn step_cost(&mut self, m: &SeqModel, s: &Abs, t: &Abs) -> AR<Cost> {
        let mut c = Cost {
            ops: 0.0,
            steps: 1.0,
        };
        if let Some(stop) = m.stop {
            c += self.call(stop, vec![s.clone()])?.1;
        }
        c += self.call(m.step, vec![s.clone(), Abs::Top, t.clone()])?.1;
        for &(f, _) in &m.invariants {
            c += self.call(f, vec![s.clone()])?.1;
        }
        Ok(c)
    }

    /// Every state the model can reach from `start`, whatever it's told
    /// to do.
    fn state(&mut self, id: u32, m: &SeqModel, start: Abs) -> AR<Abs> {
        if let Some(s) = self.states.get(&id)
            && join(s, &start) == *s
        {
            return Ok(s.clone());
        }
        let h = self.horizons[id as usize].max(1);
        let t = Abs::Int(0, h - 1);
        let mut s = match self.states.get(&id) {
            Some(prev) => join(prev, &start),
            None => start,
        };
        for round in 0.. {
            let (next, _) = self.call(m.step, vec![s.clone(), Abs::Top, t.clone()])?;
            let j = join(&s, &next);
            if j == s {
                break;
            }
            s = if round >= 2 { widen(&s, &j) } else { j };
            if round > 20 {
                s = Abs::Top;
                break;
            }
        }
        self.states.insert(id, s.clone());
        Ok(s)
    }

    fn call(&mut self, f: u32, args: Vec<Abs>) -> AR<(Abs, Cost)> {
        if let Some(list) = self.memo.get(&f)
            && let Some((_, a, c)) = list.iter().find(|(k, _, _)| *k == args)
        {
            return Ok((a.clone(), *c));
        }
        let def = &self.p.fns[f as usize];
        let mut env = args.clone();
        env.resize(def.nslots as usize, Abs::Top);
        let r = self.ex(&def.body, &mut env)?;
        self.memo
            .entry(f)
            .or_default()
            .push((args, r.0.clone(), r.1));
        Ok(r)
    }

    fn fact(&mut self, f: u32, args: Vec<Abs>) -> AR<(Abs, Cost)> {
        if let Some(list) = self.fact_memo.get(&f)
            && let Some((_, a, c)) = list.iter().find(|(k, _, _)| *k == args)
        {
            return Ok((a.clone(), *c));
        }
        let def = &self.p.facts[f as usize];
        let mut env = args.clone();
        env.resize(def.nslots as usize, Abs::Top);
        let r = match &def.body {
            FactBody::Derived(e) => self.ex(e, &mut env)?,
            FactBody::Draw(dist, xs) => {
                let mut c = Cost::ONE;
                let mut vals = Vec::new();
                for x in xs {
                    let (a, xc) = self.ex(x, &mut env)?;
                    c += xc;
                    vals.push(a);
                }
                let a = match (dist, vals.as_slice()) {
                    (Dist::UniformInt, [Abs::Int(lo, _), Abs::Int(_, hi)]) => Abs::Int(*lo, *hi),
                    (Dist::Categorical | Dist::Empirical, [Abs::Arr(_, _, e), ..]) => (**e).clone(),
                    _ => Abs::Top,
                };
                (a, c)
            }
        };
        self.fact_memo
            .entry(f)
            .or_default()
            .push((args, r.0.clone(), r.1));
        Ok(r)
    }

    /// How many times a loop runs (at least, at most), its variable, and
    /// the cost of computing its range.
    fn range(&mut self, it: &Iter, env: &mut Vec<Abs>, line: u32) -> AR<(u64, u64, Abs, Cost)> {
        match it {
            Iter::Over(e) => {
                let (a, c) = self.ex(e, env)?;
                match a {
                    Abs::Arr(l, h, elem) => {
                        if h == INF {
                            return Err(unbounded(
                                line,
                                "the length of the array this loop runs over",
                            ));
                        }
                        Ok((l, h, *elem, c))
                    }
                    _ => Err(unbounded(
                        line,
                        "the length of the array this loop runs over",
                    )),
                }
            }
            Iter::Range {
                lo,
                hi,
                inclusive,
                step,
                num,
            } => {
                let (la, c1) = self.ex(lo, env)?;
                let (ha, c2) = self.ex(hi, env)?;
                let (sa, c3) = match step {
                    Some(s) => self.ex(s, env)?,
                    None => (Abs::Int(1, 1), Cost::default()),
                };
                let c = c1 + c2 + c3;
                if *num {
                    return Err(unbounded(
                        line,
                        "a range of floats in a model or policy (use an integer range)",
                    ));
                }
                let (lo_min, lo_max) = la.int_range();
                let (hi_min, hi_max) = ha.int_range();
                let (step_min, step_max) = sa.int_range();
                if lo_min == i64::MIN || hi_max == i64::MAX {
                    return Err(unbounded(line, "this loop's range"));
                }
                let step_min = step_min.max(1) as i128;
                let last_max = hi_max as i128 - if *inclusive { 0 } else { 1 };
                let n_hi = if last_max < lo_min as i128 {
                    0
                } else {
                    ((last_max - lo_min as i128) / step_min + 1) as u64
                };
                let n_lo = if lo_min == lo_max && hi_min == hi_max && step_max as i128 == step_min {
                    n_hi
                } else {
                    0
                };
                Ok((n_lo, n_hi, Abs::Int(lo_min, clamp(last_max)), c))
            }
        }
    }

    /// A loop body run up to `n` times, with the variables it assigns
    /// grown to a fixpoint. Returns the cost of one iteration (including
    /// its tick) and the join of the values `value` produced.
    fn looped(
        &mut self,
        env: &mut Vec<Abs>,
        slot: u32,
        elem: &Abs,
        guard: Option<&Ex>,
        stmts: &[St],
        value: Option<&Ex>,
    ) -> AR<(Cost, Abs)> {
        let mut entry = env.clone();
        for round in 0.. {
            let mut e2 = entry.clone();
            e2[slot as usize] = elem.clone();
            let mut c = Cost::ONE;
            if let Some(g) = guard {
                c += self.ex(g, &mut e2)?.1;
            }
            for s in stmts {
                c += self.st(s, &mut e2)?;
            }
            let mut out = Abs::Top;
            if let Some(v) = value {
                let (a, vc) = self.ex(v, &mut e2)?;
                c += vc;
                out = a;
            }
            let joined = join_env(&entry, &e2);
            if joined == entry {
                *env = entry;
                return Ok((c, out));
            }
            entry = if round >= 2 {
                widen_env(&entry, &joined)
            } else {
                joined
            };
        }
        unreachable!()
    }

    fn st(&mut self, s: &St, env: &mut Vec<Abs>) -> AR<Cost> {
        let mut c = Cost::ONE;
        match s {
            St::Let(slot, e) => {
                let (a, ec) = self.ex(e, env)?;
                env[*slot as usize] = a;
                c += ec;
            }
            St::Set(slot, path, e) => {
                let mut idx = Vec::new();
                for seg in path {
                    if let PathSeg::Index(ie, _) = seg {
                        c += self.ex(ie, env)?.1;
                        idx.push(());
                    }
                }
                let (v, ec) = self.ex(e, env)?;
                c += ec;
                let old = std::mem::replace(&mut env[*slot as usize], Abs::Top);
                env[*slot as usize] = set_path(old, path, v);
            }
            St::For {
                slot,
                iter,
                cond,
                body,
                line,
            } => {
                let (_, n, elem, rc) = self.range(iter, env, *line)?;
                c += rc;
                if n > 0 {
                    let (per, _) = self.looped(env, *slot, &elem, cond.as_ref(), body, None)?;
                    c += per.times(n as f64);
                }
            }
            St::Assert(e, _, _) | St::Note(_, _, e) | St::Expr(e) => c += self.ex(e, env)?.1,
        }
        Ok(c)
    }

    fn exs(&mut self, xs: &[Ex], env: &mut Vec<Abs>) -> AR<(Vec<Abs>, Cost)> {
        let mut c = Cost::default();
        let mut out = Vec::with_capacity(xs.len());
        for x in xs {
            let (a, xc) = self.ex(x, env)?;
            out.push(a);
            c += xc;
        }
        Ok((out, c))
    }

    fn ex(&mut self, e: &Ex, env: &mut Vec<Abs>) -> AR<(Abs, Cost)> {
        let one = Cost::ONE;
        Ok(match e {
            Ex::Const(v) => (Abs::of(v), one),
            Ex::Local(i) => (env[*i as usize].clone(), one),
            Ex::Global(g) => (self.globals[*g as usize].clone(), one),
            Ex::Fact { fact, args, .. } => {
                let (xs, c) = self.exs(args, env)?;
                let (a, fc) = self.fact(*fact, xs)?;
                (a, one + c + fc)
            }
            Ex::Neg(x, _) => {
                let (a, c) = self.ex(x, env)?;
                let a = match a {
                    Abs::Int(lo, hi) => Abs::Int(clamp(-(hi as i128)), clamp(-(lo as i128))),
                    _ => Abs::Top,
                };
                (a, one + c)
            }
            Ex::Not(x) | Ex::ToNum(x) => (Abs::Top, one + self.ex(x, env)?.1),
            Ex::Arith(op, a, b, _) => {
                let (x, c1) = self.ex(a, env)?;
                let (y, c2) = self.ex(b, env)?;
                (arith(*op, &x, &y), one + c1 + c2)
            }
            Ex::Cmp(_, a, b) => {
                let c1 = self.ex(a, env)?.1;
                let c2 = self.ex(b, env)?.1;
                (Abs::Top, one + c1 + c2)
            }
            Ex::And(a, b) | Ex::Or(a, b) => {
                let c1 = self.ex(a, env)?.1;
                let mut e2 = env.clone();
                let c2 = self.ex(b, &mut e2)?.1;
                *env = join_env(env, &e2);
                (Abs::Top, one + c1 + c2)
            }
            Ex::Call(f, args) => {
                let (xs, c) = self.exs(args, env)?;
                let (a, fc) = self.call(*f, xs)?;
                (a, one + c + fc)
            }
            Ex::Builtin(b, args, line) => {
                let (xs, c) = self.exs(args, env)?;
                let (a, extra) = self.builtin(*b, &xs, *line)?;
                (a, one + c + Cost::ops(extra))
            }
            Ex::Record(xs) => {
                let (fs, c) = self.exs(xs, env)?;
                (Abs::Rec(fs), one + c)
            }
            Ex::Field(x, i) => {
                let (a, c) = self.ex(x, env)?;
                let a = match a {
                    Abs::Rec(fs) => fs.get(*i as usize).cloned().unwrap_or(Abs::Top),
                    _ => Abs::Top,
                };
                (a, one + c)
            }
            Ex::Array(xs) => {
                let (items, c) = self.exs(xs, env)?;
                let n = items.len() as u64;
                let elem = items
                    .iter()
                    .skip(1)
                    .fold(items.first().cloned().unwrap_or(Abs::Top), |acc, x| {
                        join(&acc, x)
                    });
                (Abs::Arr(n, n, Box::new(elem)), one + c)
            }
            Ex::Index(a, i, _) => {
                let (x, c1) = self.ex(a, env)?;
                let c2 = self.ex(i, env)?.1;
                let elem = match x {
                    Abs::Arr(_, _, e) => *e,
                    _ => Abs::Top,
                };
                (elem, one + c1 + c2)
            }
            Ex::Comp {
                slot,
                iter,
                filter,
                body,
                line,
            } => {
                let (n_lo, n, elem, rc) = self.range(iter, env, *line)?;
                let mut c = one + rc;
                let mut out = Abs::Top;
                if n > 0 {
                    let (per, v) =
                        self.looped(env, *slot, &elem, filter.as_deref(), &[], Some(body))?;
                    c += per.times(n as f64);
                    out = v;
                }
                let lo = if filter.is_some() { 0 } else { n_lo };
                (Abs::Arr(lo, n, Box::new(out)), c)
            }
            Ex::If(cnd, t, f) => {
                let c0 = self.ex(cnd, env)?.1;
                let mut et = env.clone();
                let (at, ct) = self.ex(t, &mut et)?;
                let mut ef = env.clone();
                let (af, cf) = self.ex(f, &mut ef)?;
                *env = join_env(&et, &ef);
                (join(&at, &af), one + c0 + ct.max(cf))
            }
            Ex::Match(s, arms, _) => {
                let c0 = self.ex(s, env)?.1;
                let mut worst = Cost::default();
                let mut out: Option<Abs> = None;
                let mut joined: Option<Vec<Abs>> = None;
                for arm in arms {
                    let mut ea = env.clone();
                    for b in &arm.binds {
                        ea[*b as usize] = Abs::Top;
                    }
                    let (a, c) = self.ex(&arm.body, &mut ea)?;
                    worst = worst.max(c);
                    out = Some(match out {
                        None => a,
                        Some(o) => join(&o, &a),
                    });
                    joined = Some(match joined {
                        None => ea,
                        Some(j) => join_env(&j, &ea),
                    });
                }
                if let Some(j) = joined {
                    *env = j;
                }
                (out.unwrap_or(Abs::Top), one + c0 + worst)
            }
            Ex::Variant(_, xs) => (Abs::Top, one + self.exs(xs, env)?.1),
            Ex::Block(stmts, tail) => {
                let mut c = one;
                for s in stmts {
                    c += self.st(s, env)?;
                }
                let (a, tc) = self.ex(tail, env)?;
                (a, c + tc)
            }
            Ex::Forecast {
                model,
                action,
                horizon,
                worlds,
                skip,
                then,
                line,
            } => {
                let (act, mut c) = self.ex(action, env)?;
                c += one;
                let h = match horizon {
                    Some(h) => {
                        let (a, hc) = self.ex(h, env)?;
                        c += hc;
                        a.int_range().1
                    }
                    None => 1,
                };
                let (k, kc) = self.ex(worlds, env)?;
                c += kc;
                if let Some(s) = skip {
                    c += self.ex(s, env)?.1;
                }
                if let Some(t) = then {
                    c += self.ex(t, env)?.1;
                }
                let (k_lo, k) = k.int_range();
                if k == i64::MAX {
                    return Err(unbounded(*line, "the number of forecast worlds"));
                }
                let k = k.max(0) as u64;
                self.max_forecast_worlds = self.max_forecast_worlds.max(k);
                let (out, per) = self.imagine(*model, act, h, true)?;
                c += per.belief + (Cost::ONE + per.run).times(k as f64);
                (Abs::Arr(k_lo.max(0) as u64, k, Box::new(out)), c)
            }
            Ex::Rollout {
                model,
                action,
                horizon,
                then,
                ..
            } => {
                let (act, mut c) = self.ex(action, env)?;
                c += one;
                let h = match horizon {
                    Some(h) => {
                        let (a, hc) = self.ex(h, env)?;
                        c += hc;
                        a.int_range().1
                    }
                    None => 1,
                };
                if let Some(t) = then {
                    c += self.ex(t, env)?.1;
                }
                let (out, per) = self.imagine(*model, act, h, false)?;
                (out, c + per.belief + per.run)
            }
            Ex::Stat { .. } => (Abs::Top, one),
        })
    }

    /// One imagined future: from the policy's belief (a forecast) or the
    /// true state (a rollout), `h` steps.
    fn imagine(&mut self, model: u32, action: Abs, h: i64, belief: bool) -> AR<(Abs, Imagined)> {
        match &self.p.models[model as usize] {
            ModelDef::Decision { func, .. } => {
                let (out, c) = self.call(*func, vec![action])?;
                Ok((
                    out,
                    Imagined {
                        belief: Cost::default(),
                        run: c + Cost {
                            ops: 0.0,
                            steps: 1.0,
                        },
                    },
                ))
            }
            ModelDef::Sequential(m) => {
                let horizon = self.horizons[model as usize].max(0);
                let steps = h.clamp(0, horizon);
                let t = Abs::Int(0, (horizon - 1).max(0));
                let seen = self.decision.clone().unwrap_or(Abs::Top);
                let (start, c_belief) = match (belief, m.belief) {
                    (true, Some(b)) => self.call(b, vec![seen, t.clone()])?,
                    _ => (seen, Cost::default()),
                };
                let s = self.state(model, m, start)?;
                let per = self.step_cost(m, &s, &t)?;
                let (out, c_out) = match m.outcome_fn {
                    Some(f) => self.call(f, vec![s.clone()])?,
                    None => (s, Cost::default()),
                };
                Ok((
                    out,
                    Imagined {
                        belief: c_belief,
                        run: (Cost::ONE + per).times(steps as f64) + c_out,
                    },
                ))
            }
        }
    }

    /// The abstract result of a builtin, and its extra work.
    fn builtin(&mut self, b: Bi, xs: &[Abs], line: u32) -> AR<(Abs, f64)> {
        let len_hi = |a: &Abs| -> f64 {
            match a {
                Abs::Arr(_, h, _) if *h != INF => *h as f64,
                _ => f64::INFINITY,
            }
        };
        let check = |w: f64, what: &str| -> AR<f64> {
            if w.is_finite() {
                Ok(w)
            } else {
                Err(unbounded(line, what))
            }
        };
        Ok(match b {
            Bi::Len => match &xs[0] {
                Abs::Arr(l, h, _) => (
                    Abs::Int(*l as i64, if *h == INF { i64::MAX } else { *h as i64 }),
                    0.0,
                ),
                _ => (Abs::Int(0, i64::MAX), 0.0),
            },
            Bi::Fill => {
                let (lo, hi) = xs[0].int_range();
                if hi == i64::MAX {
                    return Err(unbounded(line, "the length given to fill"));
                }
                let hi = hi.max(0) as u64;
                (
                    Abs::Arr(lo.max(0) as u64, hi, Box::new(xs[1].clone())),
                    hi as f64,
                )
            }
            Bi::Sum | Bi::Any | Bi::All => (Abs::Top, check(len_hi(&xs[0]), "the array's length")?),
            Bi::ArgMin | Bi::ArgMax => {
                let n = check(len_hi(&xs[0]), "the array's length")?;
                (Abs::Int(0, (n as i64 - 1).max(0)), n)
            }
            Bi::Stat(kind) => {
                let (n, elem) = match &xs[0] {
                    Abs::Arr(_, h, e) if *h != INF => (*h, (**e).clone()),
                    _ => return Err(unbounded(line, "the number of values summarised")),
                };
                let work =
                    check(cells(&elem), "the arrays summarised")? * stats::cost(kind, n) as f64;
                (stat_abs(kind, &elem, n), work)
            }
            Bi::Min2 | Bi::Max2 => match (&xs[0], &xs[1]) {
                (Abs::Int(a, x), Abs::Int(c, y)) => (
                    if b == Bi::Min2 {
                        Abs::Int(*a.min(c), *x.min(y))
                    } else {
                        Abs::Int(*a.max(c), *x.max(y))
                    },
                    0.0,
                ),
                _ => (Abs::Top, 0.0),
            },
            Bi::Abs => match &xs[0] {
                Abs::Int(a, b) => (
                    Abs::Int(0, clamp((*a as i128).abs().max((*b as i128).abs()))),
                    0.0,
                ),
                _ => (Abs::Top, 0.0),
            },
            Bi::Clamp => match (&xs[1], &xs[2]) {
                (Abs::Int(lo, _), Abs::Int(_, hi)) => (Abs::Int(*lo, *hi), 0.0),
                _ => (Abs::Top, 0.0),
            },
            Bi::Mod => match &xs[1] {
                Abs::Int(c, d) if *c >= 1 => (Abs::Int(0, d.saturating_sub(1)), 0.0),
                _ => (Abs::Top, 0.0),
            },
            Bi::Floor | Bi::Ceil | Bi::Round => (Abs::Int(i64::MIN, i64::MAX), 0.0),
            _ => (Abs::Top, 0.0),
        })
    }
}

struct Imagined {
    belief: Cost,
    run: Cost,
}

fn stat_abs(kind: StatKind, elem: &Abs, rows: u64) -> Abs {
    match elem {
        Abs::Arr(l, h, e) => Abs::Arr(*l, *h, Box::new(stat_abs(kind, e, rows))),
        Abs::Int(..) if matches!(kind, StatKind::Min | StatKind::Max) => elem.clone(),
        _ if kind == StatKind::Count => Abs::Int(0, rows as i64),
        _ => Abs::Top,
    }
}

fn arith(op: ArOp, x: &Abs, y: &Abs) -> Abs {
    let (Abs::Int(a, b), Abs::Int(c, d)) = (x, y) else {
        return Abs::Top;
    };
    let (a, b, c, d) = (*a as i128, *b as i128, *c as i128, *d as i128);
    let unb = |v: i128| v <= i64::MIN as i128 || v >= i64::MAX as i128;
    if [a, b, c, d].iter().any(|v| unb(*v)) && !matches!(op, ArOp::Add | ArOp::Sub) {
        return Abs::Int(i64::MIN, i64::MAX);
    }
    match op {
        ArOp::Add => Abs::Int(
            if unb(a) || unb(c) {
                i64::MIN
            } else {
                clamp(a + c)
            },
            if unb(b) || unb(d) {
                i64::MAX
            } else {
                clamp(b + d)
            },
        ),
        ArOp::Sub => Abs::Int(
            if unb(a) || unb(d) {
                i64::MIN
            } else {
                clamp(a - d)
            },
            if unb(b) || unb(c) {
                i64::MAX
            } else {
                clamp(b - c)
            },
        ),
        ArOp::Mul => {
            let p = [a * c, a * d, b * c, b * d];
            Abs::Int(
                clamp(*p.iter().min().unwrap()),
                clamp(*p.iter().max().unwrap()),
            )
        }
        ArOp::IDiv => {
            if c <= 0 && d >= 0 {
                return Abs::Int(i64::MIN, i64::MAX);
            }
            let fd = |x: i128, y: i128| {
                x.div_euclid(y) - if y < 0 && x.rem_euclid(y) != 0 { 1 } else { 0 }
            };
            let p = [fd(a, c), fd(a, d), fd(b, c), fd(b, d)];
            Abs::Int(
                clamp(*p.iter().min().unwrap()),
                clamp(*p.iter().max().unwrap()),
            )
        }
        ArOp::Pow => {
            if a >= 0 && c == d && (0..=62).contains(&c) {
                let pw = |x: i128| x.checked_pow(c as u32).unwrap_or(i128::MAX);
                Abs::Int(clamp(pw(a)), clamp(pw(b)))
            } else {
                Abs::Int(i64::MIN, i64::MAX)
            }
        }
        ArOp::Div => Abs::Top,
    }
}

fn set_path(old: Abs, path: &[PathSeg], v: Abs) -> Abs {
    let Some((seg, rest)) = path.split_first() else {
        return v;
    };
    match (seg, old) {
        (PathSeg::Field(f), Abs::Rec(mut fs)) => {
            let i = *f as usize;
            if i < fs.len() {
                let inner = std::mem::replace(&mut fs[i], Abs::Top);
                fs[i] = set_path(inner, rest, v);
            }
            Abs::Rec(fs)
        }
        (PathSeg::Index(..), Abs::Arr(l, h, e)) => {
            // Some element changes: the array keeps its length.
            let updated = set_path((*e).clone(), rest, v);
            Abs::Arr(l, h, Box::new(join(&e, &updated)))
        }
        _ => Abs::Top,
    }
}
