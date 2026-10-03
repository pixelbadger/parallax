//! Tree-walking evaluator.

use rustc_hash::FxHashMap as HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::rc::Rc;

use crate::ast::{
    Ann, Block, Expr, FuncDef, Interner, Op, Place, Program, Stmt, Sym, well_known as wk,
};
use crate::error::Error;
use crate::rng::{Rng, universe_seed};
use crate::stats::{Sample, aggregate};
use crate::value::{Branch, Builtin, Callee, Copier, Lazy, LazyState, Scope, Slot, Struct, Value};

/// Stack the interpreter should run with: SPL recursion maps onto Rust
/// recursion. See [`Interpreter::run`].
pub const STACK_SIZE: usize = 512 << 20;

/// Stack kept in reserve below the recursion limit, for the evaluation
/// between one call and the next (bounded by the parser's nesting limit).
/// Longest array `array(n, v)` will make.
const MAX_ARRAY_LEN: usize = 1 << 28;

const STACK_MARGIN: usize = 32 << 20;

/// Why evaluation stopped early.
#[derive(Debug)]
enum Unwind {
    /// A failed `given`: throw away the current universe.
    Rejected,
    Error(Error),
}

impl From<Error> for Unwind {
    fn from(e: Error) -> Self {
        Unwind::Error(e)
    }
}

impl From<std::io::Error> for Unwind {
    fn from(e: std::io::Error) -> Self {
        Unwind::Error(e.into())
    }
}

type R<T> = Result<T, Unwind>;

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(Unwind::Error(Error::runtime(msg)))
}

pub struct Interpreter<W: Write> {
    out: W,
    names: Interner,
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

impl<W: Write> Interpreter<W> {
    pub fn new(seed: u64, out: W) -> Self {
        let global = Scope::new(None);
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
            global.set(name, Value::Builtin(b));
        }
        Interpreter {
            out,
            names: Interner::default(),
            rng: Rng::new(seed),
            seed,
            pins: HashMap::default(),
            types: HashMap::from_iter([(wk::ENSEMBLE, Rc::from(wk::ENSEMBLE_FIELDS))]),
            global,
            universe_depth: 0,
            stack_base: 0,
            stack_budget: STACK_SIZE - STACK_MARGIN,
            force_stack: Vec::new(),
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn out(&mut self) -> &mut W {
        &mut self.out
    }

    pub fn into_output(self) -> W {
        self.out
    }

    pub fn parse(&mut self, src: &str) -> Result<Program, Error> {
        crate::parser::parse(src, &mut self.names)
    }

    /// Runs every top-level statement in order, then `main()` if defined.
    ///
    /// Deep SPL recursion fails with "Recursion too deep" once it has used
    /// most of [`STACK_SIZE`], so call this on a thread with that much stack
    /// (as [`run_source`](crate::run_source) does).
    pub fn run(&mut self, program: &Program) -> Result<(), Error> {
        let marker = 0u8;
        self.stack_base = std::ptr::addr_of!(marker) as usize;
        let result = self.run_inner(program);
        self.out.flush()?;
        match result {
            Ok(()) => Ok(()),
            Err(Unwind::Error(e)) => Err(e),
            Err(Unwind::Rejected) => Err(Error::runtime("'given' rejected outside a multiverse")),
        }
    }

    fn run_inner(&mut self, program: &Program) -> R<()> {
        let global = self.global.clone();
        for stmt in program.iter() {
            self.exec(stmt, &global)?;
        }
        let has_main = global.vars.borrow().iter().any(|v| {
            v.name == wk::MAIN && matches!(v.slot, Slot::Fn(_) | Slot::Value(Value::Closure(_)))
        });
        if has_main {
            self.call(wk::MAIN, &[], &global)?;
        }
        Ok(())
    }

    fn name(&self, sym: Sym) -> &str {
        self.names.name(sym)
    }

    // --- Statements ---

    fn exec_block(&mut self, block: &Block, env: &Rc<Scope>) -> R<Value> {
        // A block evaluates to its last statement if that is an expression
        let mut result = Value::None;
        for stmt in block.iter() {
            result = match stmt {
                Stmt::Expr(e) => self.eval(e, env)?,
                _ => {
                    self.exec(stmt, env)?;
                    Value::None
                }
            };
        }
        Ok(result)
    }

    fn exec(&mut self, stmt: &Stmt, env: &Rc<Scope>) -> R<()> {
        match stmt {
            Stmt::Expr(e) => {
                self.eval(e, env)?;
            }
            Stmt::Let(b) => {
                let v = self.eval(&b.expr, env)?;
                self.validate(b.name, &v, &b.ann)?;
                env.set(b.name, v);
            }
            Stmt::Assign(name, expr) => {
                let v = self.eval(expr, env)?;
                if !env.assign(*name, v) {
                    return fail(format!(
                        "Cannot assign to undefined variable '{}'",
                        self.name(*name)
                    ));
                }
            }
            Stmt::AssignPath(name, path, expr) => {
                // The value first, then the indices left to right
                let v = self.eval(expr, env)?;
                let mut steps = Vec::with_capacity(path.len());
                for place in path.iter() {
                    steps.push(match place {
                        Place::Index(e) => {
                            let i = self.eval(e, env)?;
                            Step::Index(self.int_of(&i, "array index")?)
                        }
                        Place::Field(f) => Step::Field(*f),
                    });
                }
                // Taking the value out leaves it uniquely owned, so the
                // write happens in place unless someone else shares it.
                let Some(mut target) = env.take(*name) else {
                    return fail(format!(
                        "Cannot assign to undefined variable '{}'",
                        self.name(*name)
                    ));
                };
                let result = self.write_path(&mut target, &steps, v);
                env.assign(*name, target);
                result?;
            }
            Stmt::Pin(b) => {
                if let Some(saved) = self.pins.get(&b.name).cloned() {
                    writeln!(
                        self.out,
                        "[SYS] Pinned '{}' retrieved.",
                        self.names.name(b.name)
                    )?;
                    env.set(b.name, saved);
                    return Ok(());
                }
                let v = self.eval(&b.expr, env)?;
                let v = self.collapse(&v)?;
                self.validate(b.name, &v, &b.ann)?;
                self.pins.insert(b.name, v.clone());
                env.set(b.name, v);
            }
            Stmt::Reset(name) => {
                self.pins.remove(name);
            }
            Stmt::TypeDef(name, fields) => {
                if fields
                    .iter()
                    .enumerate()
                    .any(|(i, f)| fields[..i].contains(f))
                {
                    return fail(format!("Duplicate field in type '{}'", self.name(*name)));
                }
                self.types.insert(*name, fields.iter().copied().collect());
            }
            Stmt::Func(def) => env.set_slot(def.name, Slot::Fn(def.clone())),
            Stmt::Commit(name) => {
                let branch = self.settle(*name, env, "commit")?;
                Scope::merge_chain(&branch.origin, &branch.env);
            }
            Stmt::Discard(name) => {
                self.settle(*name, env, "discard")?;
            }
            Stmt::Given(cond) => {
                let v = self.eval(cond, env)?;
                if !self.truthy(&v)? {
                    if self.universe_depth == 0 {
                        return fail("'given' condition failed outside a multiverse");
                    }
                    return Err(Unwind::Rejected);
                }
            }
        }
        Ok(())
    }

    fn validate(&mut self, name: Sym, v: &Value, ann: &Ann) -> R<()> {
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

    fn lookup(&self, env: &Rc<Scope>, name: Sym) -> R<Value> {
        match env.lookup(name) {
            Some(v) => Ok(v),
            None => fail(format!("Undefined variable '{}'", self.name(name))),
        }
    }

    fn settle(&mut self, name: Sym, env: &Rc<Scope>, action: &str) -> R<Rc<Branch>> {
        let Value::Branch(branch) = self.lookup(env, name)? else {
            return fail(format!("Cannot {action} '{}': not a fork", self.name(name)));
        };
        if branch.settled.replace(true) {
            return fail(format!(
                "Cannot {action} '{}': fork already settled",
                self.name(name)
            ));
        }
        Ok(branch)
    }

    // --- Expressions ---

    fn eval(&mut self, expr: &Expr, env: &Rc<Scope>) -> R<Value> {
        Ok(match expr {
            Expr::Int(n) => Value::Int(*n),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Var(name) => self.lookup(env, *name)?,
            Expr::Open(None) => Value::Lazy(Lazy::new(LazyState::Open(None))),
            Expr::Open(Some(bounds)) => {
                let lo = self.eval(&bounds.0, env)?;
                let hi = self.eval(&bounds.1, env)?;
                Value::Lazy(Lazy::new(LazyState::Open(Some((lo, hi)))))
            }
            Expr::Binary(op, l, r) => {
                let a = self.eval(l, env)?;
                let b = self.eval(r, env)?;
                self.lazy(*op, &a, &b)?
            }
            Expr::Call(name, args) => self.call(*name, args, env)?,
            Expr::StructInit(name, fields) => self.struct_init(*name, fields, env)?,
            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items.iter() {
                    values.push(self.eval(item, env)?);
                }
                Value::Array(Rc::new(values))
            }
            Expr::Index(base, index) => {
                let base = self.eval(base, env)?;
                let index = self.eval(index, env)?;
                let i = self.int_of(&index, "array index")?;
                match base.resolved() {
                    Value::Array(items) => element(items, i)?.clone(),
                    _ => return fail(format!("Cannot index {}", self.repr(&base))),
                }
            }
            Expr::Member(obj, member) => {
                let obj = self.eval(obj, env)?;
                if let Value::Struct(s) = obj.resolved()
                    && let Some(field) = s.get(*member)
                {
                    field.clone()
                } else {
                    return fail(format!(
                        "Cannot access '{}' on {}",
                        self.name(*member),
                        self.repr(&obj)
                    ));
                }
            }
            Expr::Observe(inner) => {
                let v = self.eval(inner, env)?;
                self.collapse(&v)?
            }
            Expr::Fork(block) => self.fork(block, env)?,
            Expr::If(cond, then_b, else_b) => {
                let c = self.eval(cond, env)?;
                if self.truthy(&c)? {
                    self.exec_block(then_b, &Scope::child(env))?
                } else if let Some(else_b) = else_b {
                    self.exec_block(else_b, &Scope::child(env))?
                } else {
                    Value::None
                }
            }
            Expr::Repeat(count, block) => {
                let n = self.eval(count, env)?;
                let n = self.int_of(&n, "repeat count")?;
                let mut result = Value::None;
                for _ in 0..n {
                    result = self.exec_block(block, &Scope::child(env))?;
                }
                result
            }
            Expr::While(cond, block) => {
                let mut result = Value::None;
                loop {
                    let c = self.eval(cond, env)?;
                    if !self.truthy(&c)? {
                        break result;
                    }
                    result = self.exec_block(block, &Scope::child(env))?;
                }
            }
            Expr::Multiverse(count, block) => self.multiverse(count, block, env)?,
        })
    }

    /// Applies `op` now if every operand is collapsed, else returns a future.
    fn lazy(&mut self, op: Op, a: &Value, b: &Value) -> R<Value> {
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
        let (x, y) = match (int(a), int(b)) {
            (Some(x), _) if op == Op::Abs => (x, 0),
            (Some(x), Some(y)) => (x, y),
            _ => {
                let got = if op == Op::Abs {
                    self.py_repr(a)
                } else {
                    format!("{}, {}", self.py_repr(a), self.py_repr(b))
                };
                return fail(format!("'{op}' needs integers, got {got}"));
            }
        };
        let overflow = || Unwind::Error(Error::runtime(format!("Integer overflow in '{op}'")));
        Ok(match op {
            Op::Add => x.checked_add(y).ok_or_else(overflow)?,
            Op::Sub => x.checked_sub(y).ok_or_else(overflow)?,
            Op::Mul => x.checked_mul(y).ok_or_else(overflow)?,
            Op::Div => {
                if y == 0 {
                    return fail("Division by zero");
                }
                floor_div(x, y).ok_or_else(overflow)?
            }
            Op::Gt => i64::from(x > y),
            Op::Lt => i64::from(x < y),
            Op::Eq => i64::from(x == y),
            Op::And => i64::from(x != 0 && y != 0),
            Op::Or => i64::from(x != 0 || y != 0),
            Op::Min => x.min(y),
            Op::Max => x.max(y),
            Op::Abs => x.checked_abs().ok_or_else(overflow)?,
        })
    }

    /// Observes a value: collapses it (and everything it depends on) in place.
    fn collapse(&mut self, v: &Value) -> R<Value> {
        Ok(match v.resolved() {
            Value::Lazy(l) => Value::Int(self.force(l)?),
            Value::Closure(_) | Value::Builtin(_) => return fail("Cannot observe a function"),
            other => other.clone(),
        })
    }

    fn truthy(&mut self, v: &Value) -> R<bool> {
        Ok(match self.collapse(v)? {
            Value::Int(n) => n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Struct(s) => !s.fields.is_empty(),
            Value::Array(items) => !items.is_empty(),
            _ => false,
        })
    }

    fn int_of(&mut self, v: &Value, what: &str) -> R<i64> {
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

    fn call(&mut self, name: Sym, args: &[Expr], env: &Rc<Scope>) -> R<Value> {
        let Some(callee) = env.lookup_callee(name) else {
            return fail(format!("Undefined variable '{}'", self.name(name)));
        };
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval(a, env)?);
        }
        match callee {
            Callee::Builtin(b) => self.builtin(b, argv),
            Callee::Fn(def, closure_env) => self.apply_fn(name, &def, closure_env, argv),
            Callee::NotAFunction => fail(format!("'{}' is not a function", self.name(name))),
        }
    }

    fn apply_fn(&mut self, name: Sym, def: &FuncDef, env: Rc<Scope>, args: Vec<Value>) -> R<Value> {
        if args.len() != def.params.len() {
            return fail(format!(
                "'{}' expects {} argument(s), got {}",
                self.name(name),
                def.params.len(),
                args.len()
            ));
        }
        let marker = 0u8;
        if self
            .stack_base
            .abs_diff(std::ptr::addr_of!(marker) as usize)
            > self.stack_budget
        {
            return fail("Recursion too deep");
        }
        let scope = Scope::with_capacity(Some(env), def.params.len());
        for (&p, a) in def.params.iter().zip(args) {
            scope.set(p, a);
        }
        self.exec_block(&def.body, &scope)
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

    fn struct_init(&mut self, ty: Sym, given: &[(Sym, Expr)], env: &Rc<Scope>) -> R<Value> {
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
            let expr = &given
                .iter()
                .rev()
                .find(|(k, _)| *k == f)
                .expect("checked above")
                .1;
            values.push((f, self.eval(expr, env)?));
        }
        Ok(Value::Struct(Rc::new(Struct {
            ty,
            fields: values.into(),
        })))
    }

    // --- Timelines ---

    fn fork(&mut self, block: &Block, env: &Rc<Scope>) -> R<Value> {
        // The fork draws from its own stream, derived from (but not advancing)
        // this one, so this timeline sees the same draws whether or not it forked.
        let fork_rng = Rng::new(self.rng.fork_seed());
        let saved = std::mem::replace(&mut self.rng, fork_rng);
        let branch_env = Copier::timeline(env);
        let result = self.exec_block(block, &branch_env);
        self.rng = saved;
        Ok(Value::Branch(Rc::new(Branch {
            val: result?,
            env: branch_env,
            origin: env.chain_weak(),
            settled: false.into(),
        })))
    }

    fn multiverse(&mut self, count: &Expr, block: &Block, env: &Rc<Scope>) -> R<Value> {
        let count = self.eval(count, env)?;
        let count = self.int_of(&count, "multiverse count")?;
        let (base, saved) = (self.seed, self.rng.clone());
        let mut samples = Vec::with_capacity(usize::try_from(count).unwrap_or(0).min(1 << 16));
        let mut rejected = 0;
        self.universe_depth += 1;
        let mut outcome = Ok(());
        for i in 0..count.max(0) {
            self.reseed(universe_seed(base, i as u64));
            let universe = Copier::timeline(env);
            match self
                .exec_block(block, &universe)
                .and_then(|v| self.sample(&v))
            {
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
        aggregate(samples, rejected).map_err(|e| Unwind::Error(Error::runtime(e)))
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

    /// Writes `v` into `target` at `steps`, copying shared parts on the way.
    fn write_path(&self, target: &mut Value, steps: &[Step], v: Value) -> R<()> {
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

/// One resolved step of an assignment target.
enum Step {
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
