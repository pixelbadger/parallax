//! The runtime: the state a program runs against, and the operations its
//! compiled code (see [`compile`](crate::compile)) calls into.

use rustc_hash::FxHashMap as HashMap;
use std::any::Any;
use std::fmt::Write as _;
use std::io::Write;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::ast::{Ann, Interner, Loc, Op, Ref, Sym, well_known as wk};
use crate::compile::{Code, Func, Program};
use crate::error::Error;
use crate::resolve::Globals;
use crate::rng::{Rng, universe_seed};
use crate::stats::{Sample, aggregate};
use crate::value::{Branch, Builtin, Callee, Copier, Lazy, LazyState, Scope, Slot, Struct, Value};

/// Stack the interpreter should run with: SPL recursion maps onto Rust
/// recursion. See [`Interpreter::run`].
pub const STACK_SIZE: usize = 512 << 20;

/// Stack kept in reserve below the recursion limit, for the evaluation
/// between one call and the next (bounded by the parser's nesting limit).
const STACK_MARGIN: usize = 32 << 20;

/// Longest array `array(n, v)` will make.
const MAX_ARRAY_LEN: usize = 1 << 28;

/// Why evaluation stopped early.
#[derive(Debug)]
pub enum Unwind {
    /// A failed `given`: throw away the current universe.
    Rejected,
    /// Boxed to keep `R<Value>` small: every evaluation returns one.
    Error(Box<Error>),
}

impl From<Error> for Unwind {
    fn from(e: Error) -> Self {
        Unwind::Error(Box::new(e))
    }
}

impl From<std::io::Error> for Unwind {
    fn from(e: std::io::Error) -> Self {
        Unwind::Error(Box::new(e.into()))
    }
}

pub type R<T> = Result<T, Unwind>;

// Small enough to return in registers
const _: () = assert!(std::mem::size_of::<R<Value>>() == 16);

pub fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(Error::runtime(msg).into())
}

/// Where `print` writes. The runtime isn't generic over the writer, so
/// that the compiled code isn't either.
trait Output: Write {
    fn as_any(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<W: Write + 'static> Output for W {
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

pub struct Interpreter<W: Write + 'static> {
    m: Machine,
    out: PhantomData<fn() -> W>,
}

impl<W: Write + 'static> Interpreter<W> {
    pub fn new(seed: u64, out: W) -> Self {
        let global = Scope::new(None);
        let mut globals = Globals::default();
        for (name, b) in [
            (wk::PRINT, Builtin::Print),
            (wk::SEED, Builtin::Seed),
            (wk::MIN, Builtin::Min),
            (wk::MAX, Builtin::Max),
            (wk::ABS, Builtin::Abs),
            (wk::LEN, Builtin::Len),
            (wk::ARRAY, Builtin::Array),
            (wk::PUSH, Builtin::Push),
        ] {
            global.set(globals.slot(name), Value::Builtin(b));
        }
        let m = Machine {
            out: Box::new(out),
            names: Interner::default(),
            globals,
            rng: Rng::new(seed),
            seed,
            pins: HashMap::default(),
            types: HashMap::from_iter([(wk::ENSEMBLE, Rc::from(wk::ENSEMBLE_FIELDS))]),
            global,
            universe_depth: 0,
            stack_base: 0,
            stack_budget: STACK_SIZE - STACK_MARGIN,
            force_stack: Vec::new(),
        };
        Interpreter {
            m,
            out: PhantomData,
        }
    }

    pub fn seed(&self) -> u64 {
        self.m.seed
    }

    pub fn out(&mut self) -> &mut W {
        (*self.m.out)
            .as_any()
            .downcast_mut()
            .expect("output is a W")
    }

    pub fn into_output(self) -> W {
        *Output::into_any(self.m.out)
            .downcast()
            .expect("output is a W")
    }

    pub fn parse(&mut self, src: &str) -> Result<Program, Error> {
        let mut program = crate::parser::parse(src, &mut self.m.names)?;
        crate::resolve::resolve(&mut program, &mut self.m.globals);
        Ok(crate::compile::program(program))
    }

    /// Runs every top-level statement in order, then `main()` if defined.
    ///
    /// Deep SPL recursion fails with "Recursion too deep" once it has used
    /// most of [`STACK_SIZE`], so call this on a thread with that much stack
    /// (as [`run_source`](crate::run_source) does).
    pub fn run(&mut self, program: &Program) -> Result<(), Error> {
        let marker = 0u8;
        self.m.stack_base = std::ptr::addr_of!(marker) as usize;
        let result = self.m.run(program);
        self.m.out.flush()?;
        match result {
            Ok(()) => Ok(()),
            Err(Unwind::Error(e)) => Err(*e),
            Err(Unwind::Rejected) => Err(Error::runtime("'given' rejected outside a multiverse")),
        }
    }
}

/// The interpreter's state while a program runs.
pub struct Machine {
    out: Box<dyn Output>,
    names: Interner,
    globals: Globals,
    rng: Rng,
    seed: u64,
    /// Pinned values live as long as the interpreter, surviving re-seeds,
    /// forks and universes.
    pins: HashMap<Sym, Value>,
    types: HashMap<Sym, Rc<[Sym]>>,
    global: Rc<Scope>,
    universe_depth: u32,
    /// Stack address where `run` started, to measure recursion against.
    stack_base: usize,
    stack_budget: usize,
    /// Reused work stack for [`force`](Self::force).
    force_stack: Vec<Rc<Lazy>>,
}

impl Machine {
    fn run(&mut self, program: &Program) -> R<()> {
        let global = self.global.clone();
        global.grow(self.globals.len());
        program.run(self, &global)?;
        let Some(slot) = self.globals.get(wk::MAIN) else {
            return Ok(());
        };
        let callee = {
            let vars = global.vars.borrow();
            match &vars[slot as usize].slot {
                Slot::Fn(f) => Some((f.clone(), global.clone())),
                Slot::Value(Value::Closure(c)) => Some((c.def.clone(), c.env.clone())),
                _ => None,
            }
        };
        if let Some((f, env)) = callee {
            let scope = self.enter(wk::MAIN, &f, env, 0)?;
            (f.body)(self, &scope)?;
        }
        Ok(())
    }

    pub fn name(&self, sym: Sym) -> &str {
        self.names.name(sym)
    }

    // --- Variables ---

    /// The scope and slot holding `var` as seen from `env`.
    #[inline]
    pub fn find<'s>(&self, env: &'s Rc<Scope>, var: &Ref) -> Option<(&'s Rc<Scope>, u32)> {
        if let [Loc { up: 0, slot }] = *var.locs
            && env.is_bound(slot)
        {
            return Some((env, slot));
        }
        self.find_slow(env, var)
    }

    fn find_slow<'s>(&self, env: &'s Rc<Scope>, var: &Ref) -> Option<(&'s Rc<Scope>, u32)> {
        env.find(&var.locs).or_else(|| self.find_late(env, var))
    }

    /// A global bound after `var` was resolved, by a later program.
    #[cold]
    fn find_late<'s>(&self, env: &'s Rc<Scope>, var: &Ref) -> Option<(&'s Rc<Scope>, u32)> {
        let slot = self.globals.get(var.name)?;
        env.find(&[Loc {
            up: var.depth,
            slot,
        }])
    }

    pub fn lookup(&self, env: &Rc<Scope>, var: &Ref) -> R<Value> {
        match self.find(env, var) {
            Some((scope, slot)) => Ok(scope.get(slot)),
            None => self.undefined(var.name),
        }
    }

    #[cold]
    pub fn undefined<T>(&self, name: Sym) -> R<T> {
        fail(format!("Undefined variable '{}'", self.name(name)))
    }

    #[cold]
    pub fn unassignable<T>(&self, name: Sym) -> R<T> {
        fail(format!(
            "Cannot assign to undefined variable '{}'",
            self.name(name)
        ))
    }

    pub fn validate(&mut self, name: Sym, v: &Value, ann: &Ann) -> R<()> {
        let state = state_name(v);
        let (expected, ann) = match ann {
            Ann::Any => return Ok(()),
            Ann::Open(a) if state != "OPEN" => ("Open", a),
            Ann::Future(a) if state != "RESOLVED" => ("Future", a),
            Ann::Collapsed(a) if matches!(state, "OPEN" | "RESOLVED") => ("Collapsed", a),
            _ => return Ok(()),
        };
        writeln!(
            self.out,
            "[WARN] {}: Expected {expected} ({ann}), got {state}",
            self.names.name(name)
        )?;
        Ok(())
    }

    pub fn pin(&mut self, name: Sym, slot: u32, value: &Code, ann: &Ann, env: &Rc<Scope>) -> R<()> {
        if let Some(saved) = self.pins.get(&name).cloned() {
            writeln!(
                self.out,
                "[SYS] Pinned '{}' retrieved.",
                self.names.name(name)
            )?;
            env.set(slot, saved);
            return Ok(());
        }
        let v = value(self, env)?;
        let v = self.collapse(&v)?;
        self.validate(name, &v, ann)?;
        self.pins.insert(name, v.clone());
        env.set(slot, v);
        Ok(())
    }

    pub fn reset(&mut self, name: Sym) {
        self.pins.remove(&name);
    }

    pub fn define_type(&mut self, name: Sym, fields: &[Sym]) -> R<()> {
        if fields
            .iter()
            .enumerate()
            .any(|(i, f)| fields[..i].contains(f))
        {
            return fail(format!("Duplicate field in type '{}'", self.name(name)));
        }
        self.types.insert(name, fields.into());
        Ok(())
    }

    pub fn settle(&self, var: &Ref, env: &Rc<Scope>, action: &str) -> R<Rc<Branch>> {
        let Value::Branch(branch) = self.lookup(env, var)? else {
            return fail(format!(
                "Cannot {action} '{}': not a fork",
                self.name(var.name)
            ));
        };
        if branch.settled.replace(true) {
            return fail(format!(
                "Cannot {action} '{}': fork already settled",
                self.name(var.name)
            ));
        }
        Ok(branch)
    }

    pub fn given(&self, holds: bool) -> R<()> {
        if holds {
            Ok(())
        } else if self.universe_depth == 0 {
            fail("'given' condition failed outside a multiverse")
        } else {
            Err(Unwind::Rejected)
        }
    }

    // --- Operators ---

    /// Applies `op` now if every operand is collapsed, else returns a future.
    pub fn lazy(&self, op: Op, a: &Value, b: &Value) -> R<Value> {
        let (a, b) = (a.resolved(), b.resolved());
        for v in [a, b] {
            if matches!(v, Value::Closure(_) | Value::Builtin(_)) {
                return fail(format!("'{op}' needs integers, got a function"));
            }
        }
        if a.pending().is_some() || b.pending().is_some() {
            return Ok(Value::Lazy(Lazy::new(LazyState::Future(
                op,
                a.clone(),
                b.clone(),
            ))));
        }
        Ok(Value::Int(self.apply(op, a, b)?))
    }

    fn apply(&self, op: Op, a: &Value, b: &Value) -> R<i64> {
        let int = |v: &Value| match v {
            Value::Int(n) => Some(*n),
            Value::Lazy(l) => l.done(),
            _ => None,
        };
        if op == Op::Eq
            && let (Value::Str(x), Value::Str(y)) = (a, b)
        {
            return Ok(i64::from(x == y));
        }
        match (int(a), int(b)) {
            (Some(x), _) if op == Op::Abs => apply_int(op, x, 0),
            (Some(x), Some(y)) => apply_int(op, x, y),
            _ => {
                let got = if op == Op::Abs {
                    self.py_repr(a)
                } else {
                    format!("{}, {}", self.py_repr(a), self.py_repr(b))
                };
                fail(format!("'{op}' needs integers, got {got}"))
            }
        }
    }

    /// Observes a value: collapses it (and everything it depends on) in place.
    pub fn collapse(&mut self, v: &Value) -> R<Value> {
        Ok(match v.resolved() {
            Value::Lazy(l) => Value::Int(self.force(l)?),
            Value::Closure(_) | Value::Builtin(_) => return fail("Cannot observe a function"),
            other => other.clone(),
        })
    }

    #[inline]
    pub fn truthy(&mut self, v: &Value) -> R<bool> {
        match v {
            Value::Int(n) => Ok(*n != 0),
            _ => self.truthy_slow(v),
        }
    }

    fn truthy_slow(&mut self, v: &Value) -> R<bool> {
        Ok(match self.collapse(v)? {
            Value::Int(n) => n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Struct(s) => !s.fields.is_empty(),
            Value::Array(items) => !items.is_empty(),
            _ => false,
        })
    }

    #[inline]
    pub fn int_of(&mut self, v: &Value, what: &str) -> R<i64> {
        match v {
            Value::Int(n) => Ok(*n),
            _ => self.int_of_slow(v, what),
        }
    }

    fn int_of_slow(&mut self, v: &Value, what: &str) -> R<i64> {
        match self.collapse(v)? {
            Value::Int(n) => Ok(n),
            other => fail(format!(
                "{what} needs an integer, got {}",
                self.py_repr(&other)
            )),
        }
    }

    /// Collapses a lazy cell, depth first and left to right, without recursion.
    fn force(&mut self, root: &Rc<Lazy>) -> R<i64> {
        if let Some(n) = root.done() {
            return Ok(n);
        }
        let mut stack = std::mem::take(&mut self.force_stack);
        stack.clear();
        stack.push(root.clone());
        let result = self.force_with(&mut stack);
        self.force_stack = stack;
        result?;
        Ok(root.done().expect("collapsed"))
    }

    fn force_with(&mut self, stack: &mut Vec<Rc<Lazy>>) -> R<()> {
        while let Some(top) = stack.last().cloned() {
            let next = match &*top.state.borrow() {
                LazyState::Done(_) => {
                    stack.pop();
                    continue;
                }
                LazyState::Open(None) => None,
                LazyState::Open(Some((a, b))) | LazyState::Future(_, a, b) => {
                    a.pending().or_else(|| b.pending()).cloned()
                }
            };
            if let Some(child) = next {
                stack.push(child);
                continue;
            }
            let n = self.compute(&top.state.borrow())?;
            *top.state.borrow_mut() = LazyState::Done(n);
            stack.pop();
        }
        Ok(())
    }

    /// Evaluates a lazy cell whose operands have all collapsed.
    fn compute(&mut self, state: &LazyState) -> R<i64> {
        match state {
            LazyState::Done(n) => Ok(*n),
            LazyState::Open(None) => Ok(self.rng.int_in(0, 99)),
            LazyState::Open(Some((lo, hi))) => {
                let lo = self.int_of(lo, "open() bound")?;
                let hi = self.int_of(hi, "open() bound")?;
                if lo > hi {
                    return fail(format!("open({lo}, {hi}): empty range"));
                }
                Ok(self.rng.int_in(lo, hi))
            }
            LazyState::Future(op, a, b) => {
                let (a, b) = (self.collapse(a)?, self.collapse(b)?);
                self.apply(*op, &a, &b)
            }
        }
    }

    // --- Arrays and structs ---

    pub fn member(&self, obj: &Value, member: Sym) -> R<Value> {
        self.member_ref(obj, member).cloned()
    }

    pub fn member_ref<'v>(&self, obj: &'v Value, member: Sym) -> R<&'v Value> {
        if let Value::Struct(s) = obj.resolved()
            && let Some(field) = s.get(member)
        {
            return Ok(field);
        }
        fail(format!(
            "Cannot access '{}' on {}",
            self.name(member),
            self.repr(obj)
        ))
    }

    /// `base[index]`, with the index already observed.
    pub fn element_ref<'v>(&self, base: &'v Value, i: i64) -> R<&'v Value> {
        match base.resolved() {
            Value::Array(items) => element(items, i),
            _ => fail(format!("Cannot index {}", self.repr(base))),
        }
    }

    pub fn struct_init(&mut self, ty: Sym, given: &[(Sym, Code)], env: &Rc<Scope>) -> R<Value> {
        let Some(fields) = self.types.get(&ty).cloned() else {
            return fail(format!("Unknown type '{}'", self.name(ty)));
        };
        let complete = fields.iter().all(|f| given.iter().any(|(k, _)| k == f))
            && given.iter().all(|(k, _)| fields.contains(k));
        if !complete {
            let mut missing: Vec<&str> = fields
                .iter()
                .filter(|f| !given.iter().any(|(k, _)| k == *f))
                .map(|f| self.name(*f))
                .collect();
            let mut extra: Vec<&str> = given
                .iter()
                .filter(|(k, _)| !fields.contains(k))
                .map(|(k, _)| self.name(*k))
                .collect();
            missing.sort_unstable();
            extra.sort_unstable();
            extra.dedup();
            return fail(format!(
                "Bad fields for '{}': missing {}, unknown {}",
                self.name(ty),
                py_list(&missing),
                py_list(&extra)
            ));
        }
        // Fields are evaluated in the type's order; a repeated field's last value wins
        let mut values = Vec::with_capacity(fields.len());
        for &f in fields.iter() {
            let code = &given
                .iter()
                .rev()
                .find(|(k, _)| *k == f)
                .expect("checked above")
                .1;
            values.push((f, code(self, env)?));
        }
        Ok(Value::Struct(Rc::new(Struct {
            ty,
            fields: values.into(),
        })))
    }

    /// Writes `v` into `target` at `steps`, copying shared parts on the way.
    pub fn write_path(&self, target: &mut Value, steps: &[Step], v: Value) -> R<()> {
        let Some((step, rest)) = steps.split_first() else {
            *target = v;
            return Ok(());
        };
        match (step, &mut *target) {
            (Step::Index(i), Value::Array(items)) => {
                element(items, *i)?;
                let slot = &mut Rc::make_mut(items)[*i as usize];
                self.write_path(slot, rest, v)
            }
            (Step::Field(f), Value::Struct(s)) => {
                let ty = s.ty;
                match Rc::make_mut(s).fields.iter_mut().find(|(k, _)| k == f) {
                    Some((_, slot)) => self.write_path(slot, rest, v),
                    None => fail(format!(
                        "'{}' has no field '{}'",
                        self.name(ty),
                        self.name(*f)
                    )),
                }
            }
            (Step::Index(_), other) => fail(format!("Cannot index {}", self.repr(other))),
            (Step::Field(f), other) => fail(format!(
                "Cannot access '{}' on {}",
                self.name(*f),
                self.repr(other)
            )),
        }
    }

    // --- Calls ---

    /// Calls the function bound to `var` with arguments evaluated by `args`.
    pub fn call(&mut self, var: &Ref, args: &[Code], env: &Rc<Scope>) -> R<Value> {
        let Some((scope, slot)) = self.find(env, var) else {
            return self.undefined(var.name);
        };
        match scope.callee(slot) {
            Callee::Fn(f, closure_env) => {
                if args.len() != f.params {
                    // The arguments are evaluated before the count is checked
                    for a in args {
                        a(self, env)?;
                    }
                    return self.arity(var.name, &f, args.len());
                }
                let scope = Scope::with_size(Some(closure_env), f.size);
                for (a, &p) in args.iter().zip(f.param_slots.iter()) {
                    let v = a(self, env)?;
                    scope.set(p, v);
                }
                self.check_stack()?;
                (f.body)(self, &scope)
            }
            Callee::Builtin(b) => {
                let mut argv = Vec::with_capacity(args.len());
                for a in args {
                    argv.push(a(self, env)?);
                }
                self.builtin(b, argv)
            }
            Callee::NotAFunction => {
                for a in args {
                    a(self, env)?;
                }
                fail(format!("'{}' is not a function", self.name(var.name)))
            }
        }
    }

    /// A scope for a call of `f` (as `name`) with `argc` arguments, not yet bound.
    fn enter(&self, name: Sym, f: &Func, env: Rc<Scope>, argc: usize) -> R<Rc<Scope>> {
        if argc != f.params {
            return self.arity(name, f, argc);
        }
        self.check_stack()?;
        Ok(Scope::with_size(Some(env), f.size))
    }

    #[cold]
    fn arity<T>(&self, name: Sym, f: &Func, argc: usize) -> R<T> {
        fail(format!(
            "'{}' expects {} argument(s), got {}",
            self.name(name),
            f.params,
            argc
        ))
    }

    fn check_stack(&self) -> R<()> {
        let marker = 0u8;
        if self
            .stack_base
            .abs_diff(std::ptr::addr_of!(marker) as usize)
            > self.stack_budget
        {
            return fail("Recursion too deep");
        }
        Ok(())
    }

    fn builtin(&mut self, b: Builtin, args: Vec<Value>) -> R<Value> {
        let arity = |name: &str, n: usize| {
            if args.len() == n {
                Ok(())
            } else {
                fail(format!(
                    "{name}() takes {n} argument(s), got {}",
                    args.len()
                ))
            }
        };
        match b {
            Builtin::Print => {
                let mut line = String::new();
                for (i, a) in args.iter().enumerate() {
                    let v = self.collapse(a)?;
                    if i > 0 {
                        line.push(' ');
                    }
                    self.format(&v, &mut line)?;
                }
                writeln!(self.out, "{line}")?;
                Ok(Value::None)
            }
            Builtin::Seed => {
                arity("seed", 1)?;
                let n = self.int_of(&args[0], "seed()")?;
                self.reseed(n as u64);
                writeln!(self.out, "[SYS] Seed: {n}")?;
                Ok(Value::None)
            }
            Builtin::Min | Builtin::Max => {
                let (name, op) = if b == Builtin::Min {
                    ("min", Op::Min)
                } else {
                    ("max", Op::Max)
                };
                arity(name, 2)?;
                self.lazy(op, &args[0], &args[1])
            }
            Builtin::Abs => {
                arity("abs", 1)?;
                self.lazy(Op::Abs, &args[0], &Value::None)
            }
            Builtin::Len => {
                arity("len", 1)?;
                match args[0].resolved() {
                    Value::Array(items) => Ok(Value::Int(items.len() as i64)),
                    other => fail(format!("len() needs an array, got {}", self.repr(other))),
                }
            }
            Builtin::Array => {
                arity("array", 2)?;
                let n = self.int_of(&args[0], "array() length")?;
                match usize::try_from(n) {
                    Ok(n) if n <= MAX_ARRAY_LEN => {
                        Ok(Value::Array(Rc::new(vec![args[1].clone(); n])))
                    }
                    _ => fail(format!("array() length {n} is out of range")),
                }
            }
            Builtin::Push => {
                arity("push", 2)?;
                match args[0].resolved() {
                    Value::Array(items) => {
                        let mut items = Vec::clone(items);
                        items.push(args[1].clone());
                        Ok(Value::Array(Rc::new(items)))
                    }
                    other => fail(format!("push() needs an array, got {}", self.repr(other))),
                }
            }
        }
    }

    fn reseed(&mut self, seed: u64) {
        self.seed = seed;
        self.rng = Rng::new(seed);
    }

    // --- Timelines ---

    pub fn fork(&mut self, body: &Code, env: &Rc<Scope>) -> R<Value> {
        // The fork draws from its own stream, derived from (but not advancing)
        // this one, so this timeline sees the same draws whether or not it forked.
        let fork_rng = Rng::new(self.rng.fork_seed());
        let saved = std::mem::replace(&mut self.rng, fork_rng);
        let branch_env = Copier::timeline(env);
        let result = body(self, &branch_env);
        self.rng = saved;
        Ok(Value::Branch(Rc::new(Branch {
            val: result?,
            env: branch_env,
            origin: env.chain_weak(),
            settled: false.into(),
        })))
    }

    pub fn multiverse(&mut self, count: &Code, body: &Code, env: &Rc<Scope>) -> R<Value> {
        let count = count(self, env)?;
        let count = self.int_of(&count, "multiverse count")?;
        let (base, saved) = (self.seed, self.rng.clone());
        let mut samples = Vec::with_capacity(usize::try_from(count).unwrap_or(0).min(1 << 16));
        let mut rejected = 0;
        self.universe_depth += 1;
        let mut outcome = Ok(());
        for i in 0..count.max(0) {
            self.reseed(universe_seed(base, i as u64));
            let universe = Copier::timeline(env);
            match body(self, &universe).and_then(|v| self.sample(&v)) {
                Ok(s) => samples.push(s),
                Err(Unwind::Rejected) => rejected += 1,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        self.universe_depth -= 1;
        self.seed = base;
        self.rng = saved;
        outcome?;
        aggregate(samples, rejected).map_err(|e| Error::runtime(e).into())
    }

    /// Observes a universe's result fully: an integer, or a struct of samples.
    fn sample(&mut self, v: &Value) -> R<Sample> {
        match self.collapse(v)? {
            Value::Int(n) => Ok(Sample::Int(n)),
            Value::Struct(s) => {
                let mut fields = Vec::with_capacity(s.fields.len());
                for (k, f) in s.fields.iter() {
                    fields.push((*k, self.sample(f)?));
                }
                Ok(Sample::Struct(s.ty, fields.into()))
            }
            Value::Array(items) => {
                let mut samples = Vec::with_capacity(items.len());
                for item in items.iter() {
                    samples.push(self.sample(item)?);
                }
                Ok(Sample::Array(samples.into()))
            }
            other => {
                let mut shown = String::new();
                self.format(&other, &mut shown)?;
                fail(format!(
                    "A universe must produce an integer, a struct or an array, got {shown}"
                ))
            }
        }
    }

    // --- Display ---

    /// Formats a collapsed value for `print`, observing struct fields.
    fn format(&mut self, v: &Value, out: &mut String) -> R<()> {
        match v.resolved() {
            Value::Struct(s) => {
                let s = s.clone();
                write!(out, "{} {{ ", self.name(s.ty)).ok();
                for (i, (k, f)) in s.fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write!(out, "{}: ", self.name(*k)).ok();
                    let f = self.collapse(f)?;
                    self.format(&f, out)?;
                }
                out.push_str(" }");
            }
            Value::Array(items) => {
                let items = items.clone();
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let item = self.collapse(item)?;
                    self.format(&item, out)?;
                }
                out.push(']');
            }
            Value::None => out.push_str("none"),
            Value::Int(n) => write!(out, "{n}").expect("write to String"),
            Value::Str(s) => out.push_str(s),
            other => out.push_str(&self.repr(other)),
        }
        Ok(())
    }

    /// Debug form of a value, used in error messages.
    fn repr(&self, v: &Value) -> String {
        match v {
            Value::None => "<None>".into(),
            Value::Int(n) => format!("<{n}>"),
            Value::Str(s) => format!("<{s}>"),
            Value::Lazy(l) => match &*l.state.borrow() {
                LazyState::Done(n) => format!("<{n}>"),
                LazyState::Open(_) => "<?Int (Open)>".into(),
                LazyState::Future(op, ..) => {
                    format!(
                        "<{} (Future)>",
                        if op.is_boolean() { "~Bool" } else { "~Int" }
                    )
                }
            },
            Value::Struct(s) => format!("Struct<{}>", self.name(s.ty)),
            Value::Array(items) => format!("Array[{}]", items.len()),
            Value::Branch(b) => format!("<Branch: {}>", self.repr(&b.val)),
            Value::Closure(c) => format!("<fn {}>", self.name(c.def.name)),
            Value::Builtin(_) => "<built-in function>".into(),
        }
    }

    /// A collapsed value as it appears inside "needs integers" errors.
    fn py_repr(&self, v: &Value) -> String {
        match v.resolved() {
            Value::None => "None".into(),
            Value::Int(n) => n.to_string(),
            Value::Str(s) => format!("'{s}'"),
            Value::Lazy(l) if l.is_done() => l.done().unwrap_or_default().to_string(),
            Value::Struct(s) => {
                let fields: Vec<String> = s
                    .fields
                    .iter()
                    .map(|(k, f)| format!("'{}': {}", self.name(*k), self.repr(f)))
                    .collect();
                format!("{{{}}}", fields.join(", "))
            }
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(|item| self.py_repr(item)).collect();
                format!("[{}]", items.join(", "))
            }
            other => self.repr(other),
        }
    }
}

/// `op` on two collapsed integers. `Abs` ignores `y`.
#[inline]
pub fn apply_int(op: Op, x: i64, y: i64) -> R<i64> {
    let r = match op {
        Op::Add => x.checked_add(y),
        Op::Sub => x.checked_sub(y),
        Op::Mul => x.checked_mul(y),
        Op::Div => {
            if y == 0 {
                return fail("Division by zero");
            }
            floor_div(x, y)
        }
        Op::Gt => Some(i64::from(x > y)),
        Op::Lt => Some(i64::from(x < y)),
        Op::Eq => Some(i64::from(x == y)),
        Op::And => Some(i64::from(x != 0 && y != 0)),
        Op::Or => Some(i64::from(x != 0 || y != 0)),
        Op::Min => Some(x.min(y)),
        Op::Max => Some(x.max(y)),
        Op::Abs => x.checked_abs(),
    };
    match r {
        Some(n) => Ok(n),
        None => overflow(op),
    }
}

#[cold]
fn overflow<T>(op: Op) -> R<T> {
    fail(format!("Integer overflow in '{op}'"))
}

/// One resolved step of an assignment target.
#[derive(Clone, Copy)]
pub enum Step {
    Index(i64),
    Field(Sym),
}

fn element(items: &[Value], i: i64) -> R<&Value> {
    match usize::try_from(i).ok().and_then(|i| items.get(i)) {
        Some(item) => Ok(item),
        None => fail(format!(
            "Index {i} out of range for an array of length {}",
            items.len()
        )),
    }
}

fn state_name(v: &Value) -> &'static str {
    match v.resolved() {
        Value::Lazy(l) => match &*l.state.borrow() {
            LazyState::Open(_) => "OPEN",
            LazyState::Future(..) => "RESOLVED",
            LazyState::Done(_) => "COLLAPSED",
        },
        Value::Struct(_) => "STRUCT",
        Value::Array(_) => "ARRAY",
        _ => "COLLAPSED",
    }
}

/// Integer division rounding towards negative infinity.
fn floor_div(x: i64, y: i64) -> Option<i64> {
    let q = x.checked_div(y)?;
    Some(if x % y != 0 && ((x < 0) != (y < 0)) {
        q - 1
    } else {
        q
    })
}

fn py_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::floor_div;

    #[test]
    fn division_floors() {
        assert_eq!(floor_div(20, 3), Some(6));
        assert_eq!(floor_div(-7, 2), Some(-4));
        assert_eq!(floor_div(7, -2), Some(-4));
        assert_eq!(floor_div(-6, 3), Some(-2));
        assert_eq!(floor_div(i64::MIN, -1), None);
    }
}
