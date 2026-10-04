//! Runtime values, scopes and timeline copying.
//!
//! Collapsed values (numbers, strings, `none`) are immutable and shared
//! freely. Open values and futures are shared mutable cells ([`Lazy`]) that
//! collapse in place, so every holder sees the same outcome. Forks and
//! universes run in a copy of the scope chain made by [`Copier`], which copies
//! the uncollapsed part of the value graph and preserves its aliasing.
//!
//! Memory is reference counted. The cycles common programs would create are
//! avoided by construction: a function defined in a scope is stored there as
//! [`Slot::Fn`] rather than as a closure pointing back at the scope, and a
//! fork holds its origin scopes weakly.

use rustc_hash::FxHashMap as HashMap;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::ast::{Loc, Op, Sym};
use crate::compile::Func;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Seed,
    Min,
    Max,
    Abs,
    Len,
    Array,
    Push,
    Sqrt,
    Float,
    Int,
}

#[derive(Clone, Debug)]
pub enum Value {
    None,
    Int(i64),
    /// Always finite: an operation that would make `inf` or `NaN` fails.
    Float(f64),
    /// A thin pointer, which keeps `Value` at 16 bytes.
    Str(Rc<String>),
    Lazy(Rc<Lazy>),
    Struct(Rc<Struct>),
    /// Arrays have value semantics: writes copy on write, so no two
    /// variables ever observe each other's changes.
    Array(Rc<Vec<Value>>),
    Branch(Rc<Branch>),
    Closure(Rc<Closure>),
    Builtin(Builtin),
}

impl Value {
    /// Follows branches to the value their fork block produced.
    pub fn resolved(&self) -> &Value {
        let mut v = self;
        while let Value::Branch(b) = v {
            v = &b.val;
        }
        v
    }

    /// The pending cell, if this is (or a branch resolves to) an unobserved value.
    pub fn pending(&self) -> Option<&Rc<Lazy>> {
        match self.resolved() {
            Value::Lazy(l) if !l.is_done() => Some(l),
            _ => None,
        }
    }
}

/// A collapsed number, as a lazy cell holds it once observed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl From<Num> for Value {
    fn from(n: Num) -> Value {
        match n {
            Num::Int(n) => Value::Int(n),
            Num::Float(x) => Value::Float(x),
        }
    }
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(n) => n as f64,
            Num::Float(x) => x,
        }
    }
}

impl std::fmt::Display for Num {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::Int(n) => write!(f, "{n}"),
            // Debug keeps the `.0` of a whole number and is the shortest
            // form that reads back exactly, the same on every platform.
            Num::Float(x) => write!(f, "{x:?}"),
        }
    }
}

/// An Open value or a future: collapses in place to a number.
#[derive(Debug)]
pub struct Lazy {
    pub state: RefCell<LazyState>,
}

#[derive(Debug)]
pub enum LazyState {
    /// `open` (bounds `None` means 0..=99) or `open(lo, hi)`
    Open(Option<(Value, Value)>),
    /// A deferred operator; `Abs` ignores its second operand
    Future(Op, Value, Value),
    Done(Num),
}

impl Lazy {
    pub fn new(state: LazyState) -> Rc<Lazy> {
        Rc::new(Lazy {
            state: RefCell::new(state),
        })
    }

    pub fn is_done(&self) -> bool {
        matches!(*self.state.borrow(), LazyState::Done(_))
    }

    pub fn done(&self) -> Option<Num> {
        match *self.state.borrow() {
            LazyState::Done(n) => Some(n),
            _ => None,
        }
    }
}

impl Drop for Lazy {
    // Unobserved futures can form chains millions long (a loop accumulating
    // `x = x + open`); drop them iteratively rather than recursively.
    fn drop(&mut self) {
        fn take_children(state: &mut LazyState, out: &mut Vec<Value>) {
            match std::mem::replace(state, LazyState::Done(Num::Int(0))) {
                LazyState::Open(Some((a, b))) | LazyState::Future(_, a, b) => {
                    out.push(a);
                    out.push(b);
                }
                _ => {}
            }
        }
        let state = self.state.get_mut();
        if matches!(state, LazyState::Done(_) | LazyState::Open(None)) {
            return;
        }
        let mut stack = Vec::new();
        take_children(state, &mut stack);
        while let Some(v) = stack.pop() {
            if let Value::Lazy(rc) = v
                && let Ok(mut inner) = Rc::try_unwrap(rc)
            {
                take_children(inner.state.get_mut(), &mut stack);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Struct {
    pub ty: Sym,
    pub fields: Box<[(Sym, Value)]>,
}

impl Struct {
    pub fn get(&self, name: Sym) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }
}

/// The result of a `fork`: the value its block produced, plus the timeline
/// it ran in so it can be committed back into the scopes it forked from.
#[derive(Debug)]
pub struct Branch {
    pub val: Value,
    /// The forked copy of the scope chain.
    pub env: Rc<Scope>,
    /// The forked-from scope chain, innermost first. Held weakly: a scope no
    /// one else holds can't be observed, so a commit can skip it.
    pub origin: Box<[Weak<Scope>]>,
    pub settled: Cell<bool>,
}

#[derive(Debug)]
pub struct Closure {
    pub def: Rc<Func>,
    pub env: Rc<Scope>,
}

#[derive(Debug)]
pub enum Slot {
    /// Allocated but not bound yet.
    Unbound,
    Value(Value),
    /// A function closing over the scope that holds this slot.
    Fn(Rc<Func>),
}

#[derive(Debug)]
pub struct Var {
    /// Bound or assigned here since this scope was created or copied.
    pub written: bool,
    pub slot: Slot,
}

impl Var {
    const UNBOUND: Var = Var {
        written: false,
        slot: Slot::Unbound,
    };

    fn is_bound(&self) -> bool {
        !matches!(self.slot, Slot::Unbound)
    }
}

/// A scope's variables are slots laid out by [`resolve`](crate::resolve):
/// scopes copied from one another have the same layout.
#[derive(Debug, Default)]
pub struct Scope {
    pub vars: RefCell<Vec<Var>>,
    pub parent: Option<Rc<Scope>>,
}

impl Scope {
    pub fn new(parent: Option<Rc<Scope>>) -> Rc<Scope> {
        Scope::with_size(parent, 0)
    }

    /// A scope with `n` unbound slots.
    pub fn with_size(parent: Option<Rc<Scope>>, n: usize) -> Rc<Scope> {
        let mut vars = Vec::with_capacity(n);
        vars.resize_with(n, || Var::UNBOUND);
        Rc::new(Scope {
            vars: RefCell::new(vars),
            parent,
        })
    }

    /// Adds unbound slots up to `n`.
    pub fn grow(&self, n: usize) {
        let mut vars = self.vars.borrow_mut();
        if vars.len() < n {
            vars.resize_with(n, || Var::UNBOUND);
        }
    }

    /// Unbinds every slot, as if newly created.
    pub fn clear(&self) {
        for var in self.vars.borrow_mut().iter_mut() {
            *var = Var::UNBOUND;
        }
    }

    /// Reads `slot` of this scope, if it is bound.
    #[inline(always)]
    pub fn read(self: &Rc<Scope>, slot: u32) -> Option<Value> {
        Some(match &self.vars.borrow().get(slot as usize)?.slot {
            // Integers are most of what's read: skip the general clone
            Slot::Value(Value::Int(n)) => Value::Int(*n),
            Slot::Value(v) => v.clone(),
            Slot::Fn(def) => self.closure(def),
            Slot::Unbound => return None,
        })
    }

    /// A function bound here, as a value.
    #[cold]
    fn closure(self: &Rc<Scope>, def: &Rc<Func>) -> Value {
        Value::Closure(Rc::new(Closure {
            def: def.clone(),
            env: self.clone(),
        }))
    }

    /// Whether `slot` of this scope is bound. (A copy of the global scope
    /// made before a later program added globals is short.)
    #[inline]
    pub fn is_bound(&self, slot: u32) -> bool {
        self.vars
            .borrow()
            .get(slot as usize)
            .is_some_and(Var::is_bound)
    }

    /// Reads the first of `locs` that is bound.
    #[inline]
    pub fn read_at(self: &Rc<Scope>, locs: &[Loc]) -> Option<Value> {
        let mut scope = self;
        let mut up = 0;
        for loc in locs {
            while up < loc.up {
                scope = scope
                    .parent
                    .as_ref()
                    .expect("scope chain mirrors the source");
                up += 1;
            }
            if let Some(v) = scope.read(loc.slot) {
                return Some(v);
            }
        }
        None
    }

    /// The first of `locs` that is bound: its scope and slot.
    pub fn find(self: &Rc<Scope>, locs: &[Loc]) -> Option<(&Rc<Scope>, u32)> {
        let mut scope = self;
        let mut up = 0;
        for loc in locs {
            while up < loc.up {
                scope = scope
                    .parent
                    .as_ref()
                    .expect("scope chain mirrors the source");
                up += 1;
            }
            if scope.is_bound(loc.slot) {
                return Some((scope, loc.slot));
            }
        }
        None
    }

    /// Reads a bound slot.
    pub fn get(self: &Rc<Scope>, slot: u32) -> Value {
        self.read(slot).expect("read an unbound slot")
    }

    /// Like [`get`](Self::get), but does not allocate a closure for a function.
    pub fn callee(self: &Rc<Scope>, slot: u32) -> Callee {
        match &self.vars.borrow()[slot as usize].slot {
            Slot::Fn(def) => Callee::Fn(def.clone(), self.clone()),
            Slot::Value(Value::Closure(c)) => Callee::Fn(c.def.clone(), c.env.clone()),
            Slot::Value(Value::Builtin(b)) => Callee::Builtin(*b),
            Slot::Value(_) => Callee::NotAFunction,
            Slot::Unbound => unreachable!("read an unbound slot"),
        }
    }

    /// Takes the value out of a bound slot, leaving `none`, so it can be
    /// modified in place and put back with [`set`](Self::set). A function
    /// binding is returned as a closure.
    pub fn take(self: &Rc<Scope>, slot: u32) -> Value {
        let mut vars = self.vars.borrow_mut();
        match std::mem::replace(&mut vars[slot as usize].slot, Slot::Value(Value::None)) {
            Slot::Value(v) => v,
            Slot::Fn(def) => Value::Closure(Rc::new(Closure {
                def,
                env: self.clone(),
            })),
            Slot::Unbound => unreachable!("took an unbound slot"),
        }
    }

    /// Binds `slot` in this scope.
    pub fn set(self: &Rc<Scope>, slot: u32, value: Value) {
        let slot_value = match value {
            Value::Closure(c) if Rc::ptr_eq(&c.env, self) => Slot::Fn(c.def.clone()),
            v => Slot::Value(v),
        };
        self.set_slot(slot, slot_value);
    }

    pub fn set_slot(&self, slot: u32, value: Slot) {
        let slot = slot as usize;
        let mut vars = self.vars.borrow_mut();
        if slot >= vars.len() {
            vars.resize_with(slot + 1, || Var::UNBOUND);
        }
        vars[slot] = Var {
            written: true,
            slot: value,
        };
    }

    /// The chain from this scope outwards, held weakly.
    pub fn chain_weak(self: &Rc<Scope>) -> Box<[Weak<Scope>]> {
        std::iter::successors(Some(self), |s| s.parent.as_ref())
            .map(Rc::downgrade)
            .collect()
    }

    /// Applies what `theirs` (a forked copy of this scope) wrote, at every
    /// level of the chain. Levels of `origin` no longer alive are skipped.
    pub fn merge_chain(origin: &[Weak<Scope>], theirs: &Rc<Scope>) {
        let mut theirs = Some(theirs);
        for level in origin {
            let Some(t) = theirs else { break };
            if let Some(ours) = level.upgrade() {
                let written: Vec<(u32, Value)> = t
                    .vars
                    .borrow()
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.written)
                    .map(|(i, v)| {
                        let value = match &v.slot {
                            Slot::Value(v) => v.clone(),
                            // Still closes over the forked scope, not ours
                            Slot::Fn(def) => Value::Closure(Rc::new(Closure {
                                def: def.clone(),
                                env: t.clone(),
                            })),
                            Slot::Unbound => unreachable!("a written slot is bound"),
                        };
                        (i as u32, value)
                    })
                    .collect();
                for (slot, value) in written {
                    ours.set(slot, value);
                }
            }
            theirs = t.parent.as_ref();
        }
    }
}

pub enum Callee {
    Fn(Rc<Func>, Rc<Scope>),
    Builtin(Builtin),
    NotAFunction,
}

/// Copies part of the value graph for a new timeline, preserving aliasing:
/// a value reachable twice is copied once. Collapsed values are shared.
#[derive(Default)]
pub struct Copier {
    /// Copied scopes, original first. Chains are short, so a scan beats hashing.
    scopes: Vec<(*const Scope, Rc<Scope>)>,
    lazies: HashMap<*const Lazy, Rc<Lazy>>,
    branches: HashMap<*const Branch, Rc<Branch>>,
    /// Lazy copies created but not yet filled in (avoids deep recursion).
    unfilled: Vec<(Rc<Lazy>, Rc<Lazy>)>,
}

impl Copier {
    /// Copies a whole scope chain so the new timeline can't reach back and
    /// change or collapse values that belong to this one.
    pub fn timeline(scope: &Rc<Scope>) -> Rc<Scope> {
        let mut copier = Copier::default();
        let copy = copier.chain(scope);
        copier.finish();
        copy
    }

    /// Outermost scope first. Each scope is registered before its variables
    /// are copied, so closures over it (or any outer scope) follow it into
    /// the copy.
    fn chain(&mut self, scope: &Rc<Scope>) -> Rc<Scope> {
        let parent = scope.parent.as_ref().map(|p| self.chain(p));
        let copy = Scope::new(parent);
        self.register(scope, &copy);
        let vars = scope
            .vars
            .borrow()
            .iter()
            .map(|v| self.var(v, false))
            .collect();
        *copy.vars.borrow_mut() = vars;
        copy
    }

    /// Copies a scope chain held by a branch, reusing any scope already copied.
    fn scope(&mut self, scope: &Rc<Scope>) -> Rc<Scope> {
        let mut todo = Vec::new();
        let mut cur = Some(scope);
        let mut parent = None;
        while let Some(s) = cur {
            if let Some(copy) = self.copied(s) {
                parent = Some(copy.clone());
                break;
            }
            todo.push(s);
            cur = s.parent.as_ref();
        }
        // Register every new scope before copying any variables, so cycles
        // through branches terminate.
        let mut copies = Vec::with_capacity(todo.len());
        for s in todo.iter().rev() {
            let copy = Scope::new(parent.take());
            self.register(s, &copy);
            copies.push(copy.clone());
            parent = Some(copy);
        }
        for (s, copy) in todo.iter().rev().zip(&copies) {
            let vars = s
                .vars
                .borrow()
                .iter()
                .map(|v| self.var(v, v.written))
                .collect();
            *copy.vars.borrow_mut() = vars;
        }
        match copies.last() {
            Some(c) => c.clone(),
            None => parent.expect("scope copied already"),
        }
    }

    fn copied(&self, scope: &Rc<Scope>) -> Option<&Rc<Scope>> {
        // Latest registration wins, as a scope can be re-registered
        self.scopes
            .iter()
            .rev()
            .find(|(orig, _)| *orig == Rc::as_ptr(scope))
            .map(|(_, copy)| copy)
    }

    fn register(&mut self, orig: &Rc<Scope>, copy: &Rc<Scope>) {
        self.scopes.push((Rc::as_ptr(orig), copy.clone()));
    }

    fn var(&mut self, var: &Var, written: bool) -> Var {
        let slot = match &var.slot {
            Slot::Unbound => Slot::Unbound,
            Slot::Fn(def) => Slot::Fn(def.clone()),
            Slot::Value(v) => Slot::Value(self.value(v)),
        };
        Var { written, slot }
    }

    pub fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::None | Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Builtin(_) => {
                v.clone()
            }
            Value::Lazy(l) => {
                if l.is_done() {
                    return v.clone();
                }
                if let Some(copy) = self.lazies.get(&Rc::as_ptr(l)) {
                    return Value::Lazy(copy.clone());
                }
                let copy = Lazy::new(LazyState::Done(Num::Int(0)));
                self.lazies.insert(Rc::as_ptr(l), copy.clone());
                self.unfilled.push((l.clone(), copy.clone()));
                Value::Lazy(copy)
            }
            Value::Struct(s) => {
                let mut changed = false;
                let fields: Box<[(Sym, Value)]> = s
                    .fields
                    .iter()
                    .map(|(k, f)| {
                        let copy = self.value(f);
                        changed |= !same(f, &copy);
                        (*k, copy)
                    })
                    .collect();
                if changed {
                    Value::Struct(Rc::new(Struct { ty: s.ty, fields }))
                } else {
                    v.clone()
                }
            }
            Value::Array(items) => {
                let mut changed = false;
                let copied: Vec<Value> = items
                    .iter()
                    .map(|item| {
                        let copy = self.value(item);
                        changed |= !same(item, &copy);
                        copy
                    })
                    .collect();
                if changed {
                    Value::Array(Rc::new(copied))
                } else {
                    v.clone()
                }
            }
            Value::Branch(b) => {
                if let Some(copy) = self.branches.get(&Rc::as_ptr(b)) {
                    return Value::Branch(copy.clone());
                }
                let val = self.value(&b.val);
                let env = self.scope(&b.env);
                let origin = b
                    .origin
                    .iter()
                    .map(|w| match w.upgrade() {
                        Some(s) => self.copied(&s).map(Rc::downgrade).unwrap_or_default(),
                        None => Weak::new(),
                    })
                    .collect();
                let copy = Rc::new(Branch {
                    val,
                    env,
                    origin,
                    settled: b.settled.clone(),
                });
                self.branches.insert(Rc::as_ptr(b), copy.clone());
                Value::Branch(copy)
            }
            Value::Closure(c) => match self.copied(&c.env) {
                Some(env) => Value::Closure(Rc::new(Closure {
                    def: c.def.clone(),
                    env: env.clone(),
                })),
                None => v.clone(),
            },
        }
    }

    fn finish(&mut self) {
        while let Some((orig, copy)) = self.unfilled.pop() {
            let state = match &*orig.state.borrow() {
                LazyState::Open(None) => LazyState::Open(None),
                LazyState::Open(Some((lo, hi))) => {
                    LazyState::Open(Some((self.value(lo), self.value(hi))))
                }
                LazyState::Future(op, a, b) => LazyState::Future(*op, self.value(a), self.value(b)),
                LazyState::Done(n) => LazyState::Done(*n),
            };
            *copy.state.borrow_mut() = state;
        }
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Lazy(x), Value::Lazy(y)) => Rc::ptr_eq(x, y),
        (Value::Struct(x), Value::Struct(y)) => Rc::ptr_eq(x, y),
        (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
        (Value::Branch(x), Value::Branch(y)) => Rc::ptr_eq(x, y),
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        _ => true, // immutable scalars are always shared
    }
}
