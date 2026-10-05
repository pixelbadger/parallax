//! The evaluator.
//!
//! Functions are pure, so evaluation is a plain walk of the checked tree
//! with one stack of local slots. A world fact is computed from its key
//! whenever it is read. Every node evaluated, loop iteration and model step
//! adds one to `ops`; `cost.rs` bounds that count before anything runs.

use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::*;
use crate::stats;
use crate::value::Value;
use crate::world::{Stream, combine, fnv, forecast_world};

/// A failure inside one world: reported against that world, not fatal.
#[derive(Clone, Debug)]
pub struct Fault {
    pub message: String,
    pub line: u32,
}

pub type R<T> = Result<T, Box<Fault>>;

pub fn fault(line: u32, message: impl Into<String>) -> Box<Fault> {
    Box::new(Fault {
        message: message.into(),
        line,
    })
}

/// What a policy is deciding from: the step, what it sees, and the world
/// it is being evaluated in.
#[derive(Clone)]
struct Decision {
    t: i64,
    /// The observation, or for an oracle the true state.
    value: Value,
    eval: Option<u64>,
}

pub struct Machine<'p> {
    pub p: &'p Program,
    pub globals: Vec<Value>,
    pub seed: u64,
    /// Each sequential model's horizon, for this study.
    pub horizons: Vec<i64>,
    pub ops: u64,
    /// The step a world failed at, for error reports.
    pub step: Option<i64>,
    /// In a study metric: the outcomes a statistic summarises.
    pub rows: Option<Rc<Vec<Value>>>,
    stack: Vec<Value>,
    base: usize,
    /// The world facts are drawn from.
    world: u64,
    /// The evaluation world, while a forecast imagines others.
    eval_world: u64,
    forecasting: bool,
    /// Keys of the facts `observe` has revealed in this run.
    revealed: FxHashSet<u64>,
    observing: bool,
    decision: Option<Decision>,
    /// When serving a decision: the policy's notes, by name, latest value.
    pub notes: Option<Vec<(Rc<str>, Ty, Value)>>,
    /// When serving a decision: facts the observations revealed, by key.
    /// A forecast holds them at these values.
    pub pinned: FxHashMap<u64, Value>,
}

fn arith(op: ArOp, a: Value, b: Value, line: u32) -> R<Value> {
    let overflow = || fault(line, "integer overflow");
    match (&a, &b) {
        (Value::Unit, _) | (_, Value::Unit) => Ok(Value::Unit),
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x, *y);
            let r = match op {
                ArOp::Add => x.checked_add(y),
                ArOp::Sub => x.checked_sub(y),
                ArOp::Mul => x.checked_mul(y),
                ArOp::IDiv => {
                    if y == 0 {
                        return Err(fault(line, "division by zero"));
                    }
                    let q = x.checked_div(y).ok_or_else(overflow)?;
                    Some(if x % y != 0 && ((x < 0) != (y < 0)) {
                        q - 1
                    } else {
                        q
                    })
                }
                ArOp::Pow => {
                    if y < 0 {
                        return Err(fault(line, "negative integer power"));
                    }
                    x.checked_pow(y.min(u32::MAX as i64) as u32)
                }
                ArOp::Div => {
                    if y == 0 {
                        return Err(fault(line, "division by zero"));
                    }
                    return Ok(Value::Num(x as f64 / y as f64));
                }
            };
            r.map(Value::Int).ok_or_else(overflow)
        }
        (Value::Num(x), Value::Int(n)) if op == ArOp::Pow => {
            let r = powi(*x, *n);
            if !r.is_finite() {
                return Err(fault(line, "number overflow"));
            }
            Ok(Value::Num(r))
        }
        _ => {
            let (x, y) = (a.as_num(), b.as_num());
            let r = match op {
                ArOp::Add => x + y,
                ArOp::Sub => x - y,
                ArOp::Mul => x * y,
                ArOp::Div | ArOp::IDiv => {
                    if y == 0.0 {
                        return Err(fault(line, "division by zero"));
                    }
                    x / y
                }
                ArOp::Pow => libm::pow(x, y),
            };
            if !r.is_finite() {
                return Err(fault(line, "number overflow"));
            }
            Ok(Value::Num(r))
        }
    }
}

/// `x^n` by repeated squaring: exactly reproducible, unlike `powi`.
fn powi(x: f64, n: i64) -> f64 {
    let mut r = 1.0;
    let mut base = x;
    let mut e = n.unsigned_abs();
    while e > 0 {
        if e & 1 == 1 {
            r *= base;
        }
        base *= base;
        e >>= 1;
    }
    if n < 0 { 1.0 / r } else { r }
}

fn compare(op: CmpOp, a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Unit) || matches!(b, Value::Unit) {
        return Value::Unit;
    }
    let r = match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        _ => {
            let ord = match (a, b) {
                (Value::Int(x), Value::Int(y)) => x.cmp(y),
                _ => a.as_num().total_cmp(&b.as_num()),
            };
            match op {
                CmpOp::Lt => ord.is_lt(),
                CmpOp::Gt => ord.is_gt(),
                CmpOp::Le => ord.is_le(),
                _ => ord.is_ge(),
            }
        }
    };
    Value::Bool(r)
}

/// The values a loop or comprehension runs over.
enum Seq {
    Ints { lo: i64, step: i64, n: u64 },
    Nums { lo: f64, step: f64, n: u64 },
    Items(Rc<Vec<Value>>),
}

impl Seq {
    fn len(&self) -> u64 {
        match self {
            Seq::Ints { n, .. } | Seq::Nums { n, .. } => *n,
            Seq::Items(v) => v.len() as u64,
        }
    }

    fn get(&self, i: u64) -> Value {
        match self {
            Seq::Ints { lo, step, .. } => Value::Int(lo + step * i as i64),
            Seq::Nums { lo, step, .. } => Value::Num(lo + step * i as f64),
            Seq::Items(v) => v[i as usize].clone(),
        }
    }
}

impl<'p> Machine<'p> {
    pub fn new(p: &'p Program, globals: Vec<Value>, seed: u64) -> Machine<'p> {
        Machine {
            p,
            globals,
            seed,
            horizons: vec![0; p.models.len()],
            ops: 0,
            step: None,
            rows: None,
            stack: Vec::with_capacity(256),
            base: 0,
            world: 0,
            eval_world: 0,
            forecasting: false,
            revealed: FxHashSet::default(),
            observing: false,
            decision: None,
            notes: None,
            pinned: FxHashMap::default(),
        }
    }

    /// Evaluate a top-level expression in a frame of its own.
    pub fn code(&mut self, c: &Code) -> R<Value> {
        let base = self.stack.len();
        self.stack.resize(base + c.nslots as usize, Value::Unit);
        let old = std::mem::replace(&mut self.base, base);
        let r = self.eval(&c.ex);
        self.base = old;
        self.stack.truncate(base);
        r
    }

    /// Evaluate a top-level expression with `slot0` in its first slot.
    pub fn code_with(&mut self, c: &Code, slot0: Value) -> R<Value> {
        let base = self.stack.len();
        self.stack
            .resize(base + (c.nslots as usize).max(1), Value::Unit);
        self.stack[base] = slot0;
        let old = std::mem::replace(&mut self.base, base);
        let r = self.eval(&c.ex);
        self.base = old;
        self.stack.truncate(base);
        r
    }

    pub fn call(&mut self, f: u32, args: Vec<Value>) -> R<Value> {
        let def = &self.p.fns[f as usize];
        let base = self.stack.len();
        self.stack.extend(args);
        self.stack.resize(base + def.nslots as usize, Value::Unit);
        let old = std::mem::replace(&mut self.base, base);
        let r = self.eval(&def.body);
        self.base = old;
        self.stack.truncate(base);
        r
    }

    fn enter_world(&mut self, world: u64) {
        self.world = world;
        self.eval_world = world;
        self.forecasting = false;
        self.revealed.clear();
        self.decision = None;
        self.observing = false;
        self.step = None;
    }

    // ---- running policies through worlds ----

    /// A decision model's policy, which decides once for every world.
    pub fn decide_once(&mut self, policy: &PolicyDef, family: Option<&Value>) -> R<Value> {
        self.enter_world(u64::MAX);
        self.decision = Some(Decision {
            t: 0,
            value: Value::Unit,
            eval: None,
        });
        let r = self.call(policy.func, family.into_iter().cloned().collect());
        self.decision = None;
        r
    }

    /// Serve one decision, outside any study world: a decision model's
    /// policy (`seen` is `None`) or a sequential policy at step `t` given
    /// what it observes. Forecasts imagine the same worlds a decision model's
    /// study does, and for a sequential model the worlds of step `t`.
    pub fn serve(
        &mut self,
        policy: &PolicyDef,
        family: Option<&Value>,
        seen: Option<(Value, i64)>,
    ) -> R<Value> {
        self.enter_world(u64::MAX);
        let mut args: Vec<Value> = family.into_iter().cloned().collect();
        let (value, t) = match seen {
            Some((o, t)) => {
                args.push(o.clone());
                args.push(Value::Int(t));
                (o, t)
            }
            None => (Value::Unit, 0),
        };
        self.decision = Some(Decision {
            t,
            value,
            eval: None,
        });
        let r = self.call(policy.func, args);
        self.decision = None;
        r
    }

    /// The key of fact `f` with these arguments.
    pub fn fact_key(&self, f: u32, args: &[Value]) -> u64 {
        let p = self.p;
        let def = &p.facts[f as usize];
        let mut key = def.prefix;
        for (i, a) in args.iter().enumerate() {
            let w = match a {
                Value::Int(n) => *n as u64,
                Value::Bool(b) => *b as u64,
                Value::Str(s) => fnv(s),
                Value::Variant(tag) => {
                    p.variant_keys[def.param_enums[i].unwrap() as usize][*tag as usize]
                }
                _ => 0,
            };
            key = combine(key, w);
        }
        key
    }

    /// Evaluate `e` in a frame of `nslots` starting with `slots`.
    pub fn eval_in(&mut self, e: &Ex, slots: Vec<Value>, nslots: u32) -> R<Value> {
        let base = self.stack.len();
        let n = (nslots as usize).max(slots.len());
        self.stack.extend(slots);
        self.stack.resize(base + n, Value::Unit);
        let old = std::mem::replace(&mut self.base, base);
        let r = self.eval(e);
        self.base = old;
        self.stack.truncate(base);
        r
    }

    /// One evaluation world of a decision model. `action` is the decision
    /// made once, or `None` for an oracle, which decides in each world.
    pub fn run_decision(
        &mut self,
        func: u32,
        policy: &PolicyDef,
        family: Option<&Value>,
        action: Option<&Value>,
        world: u64,
    ) -> R<Value> {
        self.enter_world(world);
        let a = match action {
            Some(a) => a.clone(),
            None => {
                self.decision = Some(Decision {
                    t: 0,
                    value: Value::Unit,
                    eval: Some(world),
                });
                let r = self.call(policy.func, family.into_iter().cloned().collect());
                self.decision = None;
                r?
            }
        };
        self.call(func, vec![a])
    }

    /// One evaluation world of a sequential model.
    pub fn run_sequential(
        &mut self,
        m: &SeqModel,
        horizon: i64,
        policy: &PolicyDef,
        family: Option<&Value>,
        world: u64,
    ) -> R<Value> {
        self.enter_world(world);
        let mut s = self.call(m.init, vec![])?;
        for t in 0..horizon {
            self.ops += 1;
            self.step = Some(t);
            if let Some(stop) = m.stop
                && self.call(stop, vec![s.clone()])?.as_bool()
            {
                break;
            }
            let mut args: Vec<Value> = family.into_iter().cloned().collect();
            let seen = if policy.oracle {
                s.clone()
            } else {
                self.observing = true;
                let o = self.call(m.observe, vec![s.clone(), Value::Int(t)]);
                self.observing = false;
                o?
            };
            self.decision = Some(Decision {
                t,
                value: seen.clone(),
                eval: Some(world),
            });
            args.push(seen);
            args.push(Value::Int(t));
            let a = self.call(policy.func, args);
            self.decision = None;
            s = self.call(m.step, vec![s, a?, Value::Int(t)])?;
            self.invariants(m, &s, t)?;
        }
        self.step = None;
        self.outcome(m, s)
    }

    fn invariants(&mut self, m: &SeqModel, s: &Value, t: i64) -> R<()> {
        for &(f, line) in &m.invariants {
            if !self.call(f, vec![s.clone()])?.as_bool() {
                return Err(fault(line, format!("invariant failed after step {t}")));
            }
        }
        Ok(())
    }

    fn outcome(&mut self, m: &SeqModel, s: Value) -> R<Value> {
        match m.outcome_fn {
            Some(f) => self.call(f, vec![s]),
            None => Ok(s),
        }
    }

    /// Steps `t0..t0 + h` (within the horizon) from state `s`, applying
    /// `first` and then `then`.
    #[allow(clippy::too_many_arguments)]
    fn advance(
        &mut self,
        m: &SeqModel,
        horizon: i64,
        mut s: Value,
        t0: i64,
        h: i64,
        first: &Value,
        then: Option<&Value>,
    ) -> R<Value> {
        let end = t0.saturating_add(h.max(0)).min(horizon);
        let mut t = t0;
        while t < end {
            self.ops += 1;
            if let Some(stop) = m.stop
                && self.call(stop, vec![s.clone()])?.as_bool()
            {
                break;
            }
            let a = if t == t0 {
                first
            } else {
                then.unwrap_or(first)
            };
            s = self.call(m.step, vec![s, a.clone(), Value::Int(t)])?;
            self.invariants(m, &s, t)?;
            t += 1;
        }
        Ok(s)
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn forecast(
        &mut self,
        model: u32,
        action: &Ex,
        horizon: Option<&Ex>,
        worlds: &Ex,
        skip: Option<&Ex>,
        then: Option<&Ex>,
        line: u32,
    ) -> R<Value> {
        let a = self.eval(action)?;
        let h = match horizon {
            Some(h) => self.eval(h)?.as_int(),
            None => 1,
        };
        let k = self.eval(worlds)?.as_int();
        let skip = match skip {
            Some(s) => self.eval(s)?.as_int(),
            None => 0,
        };
        let then = match then {
            Some(t) => Some(self.eval(t)?),
            None => None,
        };
        if k < 0 || h < 0 || skip < 0 {
            return Err(fault(
                line,
                "forecast needs non-negative `worlds`, `horizon` and `skip`",
            ));
        }
        let skip = skip as u64;
        let d = self.decision.clone().expect("forecast outside a decision");
        let saved = (self.world, self.eval_world, self.forecasting);
        let mut out = Vec::with_capacity(k as usize);
        let r: R<()> = (|| {
            match &self.p.models[model as usize] {
                ModelDef::Decision { func, .. } => {
                    for j in skip..skip.saturating_add(k as u64) {
                        self.ops += 1;
                        self.world = forecast_world(None, 0, j);
                        self.forecasting = true;
                        out.push(self.call(*func, vec![a.clone()])?);
                    }
                }
                ModelDef::Sequential(m) => {
                    let start = match m.belief {
                        Some(b) => self.call(b, vec![d.value.clone(), Value::Int(d.t)])?,
                        None => d.value.clone(),
                    };
                    let horizon = self.horizons[model as usize];
                    for j in skip..skip.saturating_add(k as u64) {
                        self.ops += 1;
                        // A served decision has no evaluation world: its
                        // revealed facts are pinned instead.
                        self.world = forecast_world(d.eval, d.t, j);
                        if let Some(eval) = d.eval {
                            self.eval_world = eval;
                        }
                        self.forecasting = true;
                        let s =
                            self.advance(m, horizon, start.clone(), d.t, h, &a, then.as_ref())?;
                        out.push(self.outcome(m, s)?);
                    }
                }
            }
            Ok(())
        })();
        (self.world, self.eval_world, self.forecasting) = saved;
        r?;
        Ok(Value::arr(out))
    }

    #[inline(never)]
    fn rollout(
        &mut self,
        model: u32,
        action: &Ex,
        horizon: Option<&Ex>,
        then: Option<&Ex>,
    ) -> R<Value> {
        let a = self.eval(action)?;
        let h = match horizon {
            Some(h) => self.eval(h)?.as_int(),
            None => 1,
        };
        let then = match then {
            Some(t) => Some(self.eval(t)?),
            None => None,
        };
        let d = self.decision.clone().expect("rollout outside a decision");
        match &self.p.models[model as usize] {
            ModelDef::Decision { func, .. } => self.call(*func, vec![a]),
            ModelDef::Sequential(m) => {
                let horizon = self.horizons[model as usize];
                let s = self.advance(m, horizon, d.value, d.t, h, &a, then.as_ref())?;
                self.outcome(m, s)
            }
        }
    }

    // ---- world facts ----

    #[inline(never)]
    fn fact(&mut self, f: u32, args: Vec<Value>, line: u32) -> R<Value> {
        let def = &self.p.facts[f as usize];
        let key = self.fact_key(f, &args);
        // A served decision's forecasts hold revealed facts at their
        // observed values.
        if self.forecasting
            && let Some(v) = self.pinned.get(&key)
        {
            return Ok(v.clone());
        }
        if self.observing {
            self.revealed.insert(key);
        }
        // A forecast imagines its own world, except for what was observed.
        let saved = (self.world, self.forecasting, self.observing);
        if self.forecasting && self.revealed.contains(&key) {
            self.world = self.eval_world;
            self.forecasting = false;
        }
        self.observing = false;
        let base = self.stack.len();
        self.stack.extend(args);
        self.stack.resize(base + def.nslots as usize, Value::Unit);
        let old = std::mem::replace(&mut self.base, base);
        let r = match &def.body {
            FactBody::Derived(e) => self.eval(e),
            FactBody::Draw(dist, xs) => {
                let mut vals = Vec::with_capacity(xs.len());
                let mut res = Ok(());
                for x in xs {
                    match self.eval(x) {
                        Ok(v) => vals.push(v),
                        Err(e) => {
                            res = Err(e);
                            break;
                        }
                    }
                }
                res.and_then(|()| {
                    self.ops += 1;
                    sample(*dist, &vals, Stream::new(self.seed, self.world, key), line)
                })
            }
        };
        self.base = old;
        self.stack.truncate(base);
        (self.world, self.forecasting, self.observing) = saved;
        r
    }

    // ---- expressions ----

    fn seq(&mut self, it: &Iter, line: u32) -> R<Seq> {
        match it {
            Iter::Over(e) => match self.eval(e)? {
                Value::Arr(a) => Ok(Seq::Items(a)),
                _ => Ok(Seq::Items(Rc::new(Vec::new()))),
            },
            Iter::Range {
                lo,
                hi,
                inclusive,
                step,
                num,
            } => {
                let lo = self.eval(lo)?;
                let hi = self.eval(hi)?;
                let step = match step {
                    Some(s) => Some(self.eval(s)?),
                    None => None,
                };
                if *num {
                    let (lo, hi) = (lo.as_num(), hi.as_num());
                    let step = step.map_or(1.0, |s| s.as_num());
                    if step <= 0.0 {
                        return Err(fault(line, "a range's step must be positive"));
                    }
                    let span = (hi - lo) / step;
                    let n = if span < -1e-9 {
                        0
                    } else {
                        let whole = (span + 1e-9).floor();
                        let on_grid = (span - whole).abs() <= 1e-9;
                        whole as u64 + if *inclusive || !on_grid { 1 } else { 0 }
                    };
                    Ok(Seq::Nums { lo, step, n })
                } else {
                    let (lo, hi) = (lo.as_int(), hi.as_int());
                    let step = step.map_or(1, |s| s.as_int());
                    if step <= 0 {
                        return Err(fault(line, "a range's step must be positive"));
                    }
                    let last = if *inclusive {
                        hi as i128
                    } else {
                        hi as i128 - 1
                    };
                    let n = if last < lo as i128 {
                        0
                    } else {
                        ((last - lo as i128) / step as i128 + 1) as u64
                    };
                    Ok(Seq::Ints { lo, step, n })
                }
            }
        }
    }

    fn exec(&mut self, st: &St) -> R<()> {
        self.ops += 1;
        match st {
            St::Let(slot, e) => {
                let v = self.eval(e)?;
                self.stack[self.base + *slot as usize] = v;
            }
            St::Set(slot, path, e) => {
                let mut idx = [0i64; 8];
                let mut n = 0;
                for seg in path {
                    if let PathSeg::Index(ie, line) = seg {
                        if n == idx.len() {
                            return Err(fault(*line, "assignment paths are limited to 8 indices"));
                        }
                        idx[n] = self.eval(ie)?.as_int();
                        n += 1;
                    }
                }
                let v = self.eval(e)?;
                let mut cur = &mut self.stack[self.base + *slot as usize];
                let mut k = 0;
                for seg in path {
                    cur = match (seg, cur) {
                        (PathSeg::Field(f), Value::Rec(r)) => &mut Rc::make_mut(r)[*f as usize],
                        (PathSeg::Index(_, line), Value::Arr(a)) => {
                            let i = idx[k];
                            k += 1;
                            let items = Rc::make_mut(a);
                            if i < 0 || i as usize >= items.len() {
                                return Err(fault(
                                    *line,
                                    format!(
                                        "index {i} is out of range for an array of length {}",
                                        items.len()
                                    ),
                                ));
                            }
                            &mut items[i as usize]
                        }
                        _ => unreachable!("checked path"),
                    };
                }
                *cur = v;
            }
            St::For {
                slot,
                iter,
                cond,
                body,
                line,
            } => {
                let seq = self.seq(iter, *line)?;
                for i in 0..seq.len() {
                    self.ops += 1;
                    self.stack[self.base + *slot as usize] = seq.get(i);
                    if let Some(c) = cond
                        && !self.eval(c)?.as_bool()
                    {
                        break;
                    }
                    for s in body {
                        self.exec(s)?;
                    }
                }
            }
            St::Assert(c, msg, line) => {
                if !self.eval(c)?.as_bool() {
                    return Err(fault(*line, msg.clone()));
                }
            }
            St::Note(name, ty, e) => {
                let v = self.eval(e)?;
                if let Some(notes) = &mut self.notes {
                    match notes.iter_mut().find(|n| n.0 == *name) {
                        Some(n) => n.2 = v,
                        None => notes.push((name.clone(), ty.clone(), v)),
                    }
                }
            }
            St::Expr(e) => {
                self.eval(e)?;
            }
        }
        Ok(())
    }

    pub fn eval(&mut self, e: &Ex) -> R<Value> {
        self.ops += 1;
        match e {
            Ex::Const(v) => Ok(v.clone()),
            Ex::Local(i) => Ok(self.stack[self.base + *i as usize].clone()),
            Ex::Global(g) => Ok(self.globals[*g as usize].clone()),
            Ex::Fact { fact, args, line } => {
                let vals = self.evals(args)?;
                self.fact(*fact, vals, *line)
            }
            Ex::Neg(x, line) => match self.eval(x)? {
                Value::Int(n) => n
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| fault(*line, "integer overflow")),
                Value::Num(x) => Ok(Value::Num(-x)),
                v => Ok(v),
            },
            Ex::Not(x) => Ok(match self.eval(x)? {
                Value::Unit => Value::Unit,
                v => Value::Bool(!v.as_bool()),
            }),
            Ex::ToNum(x) => Ok(match self.eval(x)? {
                Value::Unit => Value::Unit,
                v => Value::Num(v.as_num()),
            }),
            Ex::Arith(op, a, b, line) => {
                let a = self.eval(a)?;
                let b = self.eval(b)?;
                if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
                    let r = match op {
                        ArOp::Add => x + y,
                        ArOp::Sub => x - y,
                        ArOp::Mul => x * y,
                        ArOp::Div if *y != 0.0 => x / y,
                        _ => return arith(*op, a, b, *line),
                    };
                    if r.is_finite() {
                        return Ok(Value::Num(r));
                    }
                    return Err(fault(*line, "number overflow"));
                }
                if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                    let r = match op {
                        ArOp::Add => x.checked_add(*y),
                        ArOp::Sub => x.checked_sub(*y),
                        ArOp::Mul => x.checked_mul(*y),
                        _ => return arith(*op, a, b, *line),
                    };
                    return r
                        .map(Value::Int)
                        .ok_or_else(|| fault(*line, "integer overflow"));
                }
                arith(*op, a, b, *line)
            }
            Ex::Cmp(op, a, b) => {
                let a = self.eval(a)?;
                let b = self.eval(b)?;
                if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
                    return Ok(Value::Bool(match op {
                        CmpOp::Lt => x < y,
                        CmpOp::Gt => x > y,
                        CmpOp::Le => x <= y,
                        CmpOp::Ge => x >= y,
                        CmpOp::Eq => x == y,
                        CmpOp::Ne => x != y,
                    }));
                }
                if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                    return Ok(Value::Bool(match op {
                        CmpOp::Lt => x < y,
                        CmpOp::Gt => x > y,
                        CmpOp::Le => x <= y,
                        CmpOp::Ge => x >= y,
                        CmpOp::Eq => x == y,
                        CmpOp::Ne => x != y,
                    }));
                }
                Ok(compare(*op, &a, &b))
            }
            Ex::And(a, b) => {
                if self.eval(a)?.as_bool() {
                    self.eval(b)
                } else {
                    Ok(Value::Bool(false))
                }
            }
            Ex::Or(a, b) => {
                if self.eval(a)?.as_bool() {
                    Ok(Value::Bool(true))
                } else {
                    self.eval(b)
                }
            }
            Ex::Call(f, args) => {
                // Arguments go straight into the callee's frame: calls made
                // while evaluating them sit above it and pop themselves.
                let def = &self.p.fns[*f as usize];
                let base = self.stack.len();
                for a in args {
                    match self.eval(a) {
                        Ok(v) => self.stack.push(v),
                        Err(e) => {
                            self.stack.truncate(base);
                            return Err(e);
                        }
                    }
                }
                self.stack.resize(base + def.nslots as usize, Value::Unit);
                let old = std::mem::replace(&mut self.base, base);
                let r = self.eval(&def.body);
                self.base = old;
                self.stack.truncate(base);
                r
            }
            Ex::Builtin(b, args, line) => self.builtin(*b, args, *line),
            Ex::Record(fs) => Ok(Value::Rec(Rc::new(self.evals(fs)?))),
            Ex::Field(x, i) => {
                // `v.f` and `v.f.g` read in place, without copying `v`.
                if let Some(Value::Rec(r)) = self.root(x) {
                    let v = r[*i as usize].clone();
                    self.ops += 1;
                    return Ok(v);
                }
                if let Ex::Field(y, j) = &**x
                    && let Some(Value::Rec(r)) = self.root(y)
                    && let Value::Rec(r2) = &r[*j as usize]
                {
                    let v = r2[*i as usize].clone();
                    self.ops += 2;
                    return Ok(v);
                }
                match self.eval(x)? {
                    Value::Rec(r) => Ok(r[*i as usize].clone()),
                    _ => Ok(Value::Unit),
                }
            }
            Ex::Array(xs) => Ok(Value::arr(self.evals(xs)?)),
            Ex::Index(a, i, line) => {
                // `v[i]` and `v.f[i]` read in place, without copying `v`.
                let field = match &**a {
                    Ex::Local(_) | Ex::Global(_) => Some((&**a, None)),
                    Ex::Field(y, j) if matches!(**y, Ex::Local(_) | Ex::Global(_)) => {
                        Some((&**y, Some(*j)))
                    }
                    _ => None,
                };
                if let Some((root, f)) = field {
                    let i = self.eval(i)?.as_int();
                    self.ops += 1 + f.is_some() as u64;
                    let mut v = self.root(root).unwrap();
                    if let (Some(j), Value::Rec(r)) = (f, v) {
                        v = &r[j as usize];
                    }
                    let items = v.items();
                    if i < 0 || i as usize >= items.len() {
                        return Err(fault(
                            *line,
                            format!(
                                "index {i} is out of range for an array of length {}",
                                items.len()
                            ),
                        ));
                    }
                    return Ok(items[i as usize].clone());
                }
                let a = self.eval(a)?;
                let i = self.eval(i)?.as_int();
                let items = a.items();
                if i < 0 || i as usize >= items.len() {
                    return Err(fault(
                        *line,
                        format!(
                            "index {i} is out of range for an array of length {}",
                            items.len()
                        ),
                    ));
                }
                Ok(items[i as usize].clone())
            }
            Ex::Comp {
                slot,
                iter,
                filter,
                body,
                line,
            } => self.comp(*slot, iter, filter.as_deref(), body, *line),
            Ex::If(c, t, f) => {
                if self.eval(c)?.as_bool() {
                    self.eval(t)
                } else {
                    self.eval(f)
                }
            }
            Ex::Match(s, arms, _) => self.matching(s, arms),
            Ex::Variant(tag, xs) => Ok(Value::variant(*tag, self.evals(xs)?)),
            Ex::Block(stmts, tail) => {
                for s in stmts {
                    self.exec(s)?;
                }
                self.eval(tail)
            }
            Ex::Forecast {
                model,
                action,
                horizon,
                worlds,
                skip,
                then,
                line,
            } => self.forecast(
                *model,
                action,
                horizon.as_deref(),
                worlds,
                skip.as_deref(),
                then.as_deref(),
                *line,
            ),
            Ex::Rollout {
                model,
                action,
                horizon,
                then,
                ..
            } => self.rollout(*model, action, horizon.as_deref(), then.as_deref()),
            Ex::Stat {
                kind,
                arg,
                q,
                filter,
                line,
            } => self.world_stat(*kind, arg, q.as_deref(), filter.as_deref(), *line),
        }
    }

    #[inline(never)]
    fn evals(&mut self, xs: &[Ex]) -> R<Vec<Value>> {
        let mut vals = Vec::with_capacity(xs.len());
        for x in xs {
            vals.push(self.eval(x)?);
        }
        Ok(vals)
    }

    #[inline(never)]
    fn comp(
        &mut self,
        slot: u32,
        iter: &Iter,
        filter: Option<&Ex>,
        body: &Ex,
        line: u32,
    ) -> R<Value> {
        let seq = self.seq(iter, line)?;
        let mut out = Vec::with_capacity(seq.len() as usize);
        for i in 0..seq.len() {
            self.ops += 1;
            self.stack[self.base + slot as usize] = seq.get(i);
            if let Some(f) = filter
                && !self.eval(f)?.as_bool()
            {
                continue;
            }
            out.push(self.eval(body)?);
        }
        Ok(Value::arr(out))
    }

    #[inline(never)]
    fn matching(&mut self, s: &Ex, arms: &[Arm]) -> R<Value> {
        let v = self.eval(s)?;
        let (tag, fields) = v.as_variant().expect("checked match");
        for arm in arms {
            if arm.tag.is_none_or(|t| t == tag) {
                for (slot, f) in arm.binds.iter().zip(fields) {
                    self.stack[self.base + *slot as usize] = f.clone();
                }
                return self.eval(&arm.body);
            }
        }
        unreachable!("checked exhaustive match")
    }

    #[inline(never)]
    fn world_stat(
        &mut self,
        kind: StatKind,
        arg: &Ex,
        q: Option<&Ex>,
        filter: Option<&Ex>,
        line: u32,
    ) -> R<Value> {
        let rows = self.rows.clone().expect("statistic outside a study metric");
        let q = match q {
            Some(q) => self.eval(q)?.as_num(),
            None => 0.0,
        };
        let mut vals = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            self.stack[self.base] = row.clone();
            if let Some(f) = filter
                && !self.eval(f)?.as_bool()
            {
                continue;
            }
            vals.push(self.eval(arg)?);
        }
        if vals.is_empty() {
            return Ok(Value::Unit);
        }
        let refs: Vec<&Value> = vals.iter().collect();
        stats::stat(kind, &refs, q, &mut self.ops).map_err(|m| fault(line, m))
    }

    fn root(&self, e: &Ex) -> Option<&Value> {
        match e {
            Ex::Local(i) => Some(&self.stack[self.base + *i as usize]),
            Ex::Global(g) => Some(&self.globals[*g as usize]),
            _ => None,
        }
    }

    #[inline(never)]
    fn builtin(&mut self, b: Bi, args: &[Ex], line: u32) -> R<Value> {
        let mut v = Vec::with_capacity(args.len());
        for a in args {
            v.push(self.eval(a)?);
        }
        // In a study metric, a statistic over no worlds is missing, and so
        // is anything computed from it.
        if !matches!(b, Bi::Fill | Bi::Len | Bi::Stat(_))
            && v.iter().any(|x| matches!(x, Value::Unit))
        {
            return Ok(Value::Unit);
        }
        let num = |x: f64| -> R<Value> {
            if x.is_finite() {
                Ok(Value::Num(x))
            } else {
                Err(fault(line, "number overflow"))
            }
        };
        let to_int = |x: f64| -> R<Value> {
            if (-9.2e18..=9.2e18).contains(&x) {
                Ok(Value::Int(x as i64))
            } else {
                Err(fault(line, "number too large for an integer"))
            }
        };
        match b {
            Bi::Min2 | Bi::Max2 => Ok(match (&v[0], &v[1]) {
                (Value::Int(x), Value::Int(y)) => {
                    Value::Int(if b == Bi::Min2 { *x.min(y) } else { *x.max(y) })
                }
                (x, y) => {
                    let (x, y) = (x.as_num(), y.as_num());
                    Value::Num(if b == Bi::Min2 { x.min(y) } else { x.max(y) })
                }
            }),
            Bi::Abs => match &v[0] {
                Value::Int(n) => n
                    .checked_abs()
                    .map(Value::Int)
                    .ok_or_else(|| fault(line, "integer overflow")),
                x => Ok(Value::Num(x.as_num().abs())),
            },
            Bi::Sqrt => {
                let x = v[0].as_num();
                if x < 0.0 {
                    return Err(fault(
                        line,
                        format!("square root of a negative number ({x})"),
                    ));
                }
                Ok(Value::Num(x.sqrt()))
            }
            Bi::Exp => num(libm::exp(v[0].as_num())),
            Bi::Ln => {
                let x = v[0].as_num();
                if x <= 0.0 {
                    return Err(fault(
                        line,
                        format!("logarithm of a non-positive number ({x})"),
                    ));
                }
                Ok(Value::Num(libm::log(x)))
            }
            Bi::Sin => num(libm::sin(v[0].as_num())),
            Bi::Cos => num(libm::cos(v[0].as_num())),
            Bi::Pow => num(libm::pow(v[0].as_num(), v[1].as_num())),
            Bi::Floor => to_int(v[0].as_num().floor()),
            Bi::Ceil => to_int(v[0].as_num().ceil()),
            Bi::Round => to_int(v[0].as_num().round()),
            Bi::Float => Ok(Value::Num(v[0].as_num())),
            Bi::Clamp => {
                if let (Value::Int(x), Value::Int(lo), Value::Int(hi)) = (&v[0], &v[1], &v[2]) {
                    if lo > hi {
                        return Err(fault(line, "clamp(x, lo, hi) needs lo <= hi"));
                    }
                    return Ok(Value::Int(*x.clamp(lo, hi)));
                }
                let (x, lo, hi) = (v[0].as_num(), v[1].as_num(), v[2].as_num());
                if lo > hi {
                    return Err(fault(line, "clamp(x, lo, hi) needs lo <= hi"));
                }
                Ok(Value::Num(x.clamp(lo, hi)))
            }
            Bi::Mod => match (&v[0], &v[1]) {
                (Value::Int(a), Value::Int(b)) => {
                    if *b == 0 {
                        return Err(fault(line, "mod by zero"));
                    }
                    Ok(Value::Int(a.rem_euclid(*b)))
                }
                (a, b) => {
                    let (x, y) = (a.as_num(), b.as_num());
                    if y == 0.0 {
                        return Err(fault(line, "mod by zero"));
                    }
                    num(x.rem_euclid(y))
                }
            },
            Bi::Len => Ok(Value::Int(v[0].items().len() as i64)),
            Bi::Fill => {
                let n = v[0].as_int();
                if n < 0 {
                    return Err(fault(line, "fill needs a non-negative length"));
                }
                self.ops += n as u64;
                Ok(Value::arr(vec![v[1].clone(); n as usize]))
            }
            Bi::Sum => {
                let items = v[0].items();
                self.ops += items.len() as u64;
                if items.iter().all(|x| matches!(x, Value::Int(_))) {
                    let mut s: i64 = 0;
                    for x in items {
                        s = s
                            .checked_add(x.as_int())
                            .ok_or_else(|| fault(line, "integer overflow"))?;
                    }
                    Ok(Value::Int(s))
                } else {
                    num(items.iter().map(Value::as_num).sum())
                }
            }
            Bi::Any | Bi::All => {
                let items = v[0].items();
                self.ops += items.len() as u64;
                Ok(Value::Bool(if b == Bi::Any {
                    items.iter().any(Value::as_bool)
                } else {
                    items.iter().all(Value::as_bool)
                }))
            }
            Bi::ArgMin | Bi::ArgMax => {
                let items = v[0].items();
                self.ops += items.len() as u64;
                if items.is_empty() {
                    return Err(fault(line, "argmin/argmax of an empty array"));
                }
                let mut best = 0;
                for (i, x) in items.iter().enumerate().skip(1) {
                    let better = if b == Bi::ArgMin {
                        x.as_num() < items[best].as_num()
                    } else {
                        x.as_num() > items[best].as_num()
                    };
                    if better {
                        best = i;
                    }
                }
                Ok(Value::Int(best as i64))
            }
            Bi::Stat(kind) => {
                let q = v.get(1).map_or(0.0, Value::as_num);
                let refs: Vec<&Value> = v[0].items().iter().collect();
                stats::stat(kind, &refs, q, &mut self.ops).map_err(|m| fault(line, m))
            }
        }
    }
}

/// Draw from a distribution.
fn sample(dist: Dist, v: &[Value], mut s: Stream, line: u32) -> R<Value> {
    let bad = |m: &str| Err(fault(line, m.to_string()));
    match dist {
        Dist::UniformInt => {
            let (lo, hi) = (v[0].as_int(), v[1].as_int());
            if hi < lo {
                return bad("uniform(lo, hi) needs lo <= hi");
            }
            Ok(Value::Int(s.int_in(lo, hi)))
        }
        Dist::Uniform => {
            let (lo, hi) = (v[0].as_num(), v[1].as_num());
            if hi < lo {
                return bad("uniform(lo, hi) needs lo <= hi");
            }
            Ok(Value::Num(lo + (hi - lo) * s.next_f64()))
        }
        Dist::Bernoulli => {
            let p = v[0].as_num();
            if !(0.0..=1.0).contains(&p) {
                return bad("bernoulli(p) needs p between 0 and 1");
            }
            Ok(Value::Bool(s.next_f64() < p))
        }
        Dist::Categorical => {
            let (vals, ws) = (v[0].items(), v[1].items());
            if vals.is_empty() || vals.len() != ws.len() {
                return bad("categorical(values, weights) needs one weight per value");
            }
            let mut total = 0.0;
            for w in ws {
                let w = w.as_num();
                if w < 0.0 {
                    return bad("categorical weights can't be negative");
                }
                total += w;
            }
            if total <= 0.0 || !total.is_finite() {
                return bad("categorical weights must add up to more than 0");
            }
            let target = s.next_f64() * total;
            let mut acc = 0.0;
            let mut last = 0;
            for (i, w) in ws.iter().enumerate() {
                let w = w.as_num();
                if w > 0.0 {
                    last = i;
                    acc += w;
                    if target < acc {
                        return Ok(vals[i].clone());
                    }
                }
            }
            Ok(vals[last].clone())
        }
        Dist::Normal => {
            let (m, sd) = (v[0].as_num(), v[1].as_num());
            if sd < 0.0 {
                return bad("normal(mean, sd) needs sd >= 0");
            }
            let x = m + sd * s.normal();
            if !x.is_finite() {
                return bad("number overflow");
            }
            Ok(Value::Num(x))
        }
        Dist::LogNormal => {
            let (median, sigma) = (v[0].as_num(), v[1].as_num());
            if median <= 0.0 || sigma < 0.0 {
                return bad("lognormal(median, sigma) needs median > 0 and sigma >= 0");
            }
            let x = median * libm::exp(sigma * s.normal());
            if !x.is_finite() {
                return bad("number overflow");
            }
            Ok(Value::Num(x))
        }
        Dist::Triangular => {
            let (lo, mode, hi) = (v[0].as_num(), v[1].as_num(), v[2].as_num());
            if !(lo <= mode && mode <= hi) {
                return bad("triangular(lo, mode, hi) needs lo <= mode <= hi");
            }
            if hi == lo {
                return Ok(Value::Num(lo));
            }
            let u = s.next_f64();
            let f = (mode - lo) / (hi - lo);
            let x = if u < f {
                lo + (u * (hi - lo) * (mode - lo)).sqrt()
            } else {
                hi - ((1.0 - u) * (hi - lo) * (hi - mode)).sqrt()
            };
            Ok(Value::Num(x))
        }
        Dist::Empirical => {
            let items = v[0].items();
            if items.is_empty() {
                return bad("empirical(samples) needs at least one sample");
            }
            Ok(items[s.int_in(0, items.len() as i64 - 1) as usize].clone())
        }
    }
}
