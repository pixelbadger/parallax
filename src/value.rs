//! Runtime values, scopes and timeline copying.
//!
//! Collapsed values (integers, strings, `none`) are immutable and shared
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

use crate::ast::{FuncDef, Op, Sym};

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
}

#[derive(Clone, Debug)]
pub enum Value {
    None,
    Int(i64),
    Str(Rc<str>),
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

/// An Open value or a future: collapses in place to an integer.
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
    Done(i64),
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

    pub fn done(&self) -> Option<i64> {
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
            match std::mem::replace(state, LazyState::Done(0)) {
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
    pub def: Rc<FuncDef>,
    pub env: Rc<Scope>,
}

#[derive(Debug)]
pub enum Slot {
    Value(Value),
    /// A function closing over the scope that holds this slot.
    Fn(Rc<FuncDef>),
}

#[derive(Debug)]
pub struct Var {
    pub name: Sym,
    /// Bound or assigned here since this scope was created or copied.
    pub written: bool,
    pub slot: Slot,
}

#[derive(Debug, Default)]
pub struct Scope {
    pub vars: RefCell<Vec<Var>>,
    pub parent: Option<Rc<Scope>>,
}

impl Scope {
    pub fn new(parent: Option<Rc<Scope>>) -> Rc<Scope> {
        Scope::with_capacity(parent, 0)
    }

    pub fn with_capacity(parent: Option<Rc<Scope>>, n: usize) -> Rc<Scope> {
        Rc::new(Scope {
            vars: RefCell::new(Vec::with_capacity(n)),
            parent,
        })
    }

    /// Reads `name` from the nearest scope that defines it.
    pub fn lookup(self: &Rc<Scope>, name: Sym) -> Option<Value> {
        let mut scope = self;
        loop {
            if let Some(var) = scope.vars.borrow().iter().find(|v| v.name == name) {
                return Some(match &var.slot {
                    Slot::Value(v) => v.clone(),
                    Slot::Fn(def) => Value::Closure(Rc::new(Closure {
                        def: def.clone(),
                        env: scope.clone(),
                    })),
                });
            }
            scope = scope.parent.as_ref()?;
        }
    }

    /// Like [`lookup`](Self::lookup), but does not allocate a closure for a function.
    pub fn lookup_callee(self: &Rc<Scope>, name: Sym) -> Option<Callee> {
        let mut scope = self;
        loop {
            if let Some(var) = scope.vars.borrow().iter().find(|v| v.name == name) {
                return Some(match &var.slot {
                    Slot::Fn(def) => Callee::Fn(def.clone(), scope.clone()),
                    Slot::Value(Value::Closure(c)) => Callee::Fn(c.def.clone(), c.env.clone()),
                    Slot::Value(Value::Builtin(b)) => Callee::Builtin(*b),
                    Slot::Value(_) => Callee::NotAFunction,
                });
            }
            scope = scope.parent.as_ref()?;
        }
    }

    /// Binds `name` in this scope.
    pub fn set(self: &Rc<Scope>, name: Sym, value: Value) {
        let slot = match value {
            Value::Closure(c) if Rc::ptr_eq(&c.env, self) => Slot::Fn(c.def.clone()),
            v => Slot::Value(v),
        };
        self.set_slot(name, slot);
    }

    /// Takes the value of `name` out of the nearest scope that defines it,
    /// leaving `none`, so it can be modified in place and put back with
    /// [`assign`](Self::assign). A function binding is returned as a closure.
    pub fn take(self: &Rc<Scope>, name: Sym) -> Option<Value> {
        let mut scope = self;
        loop {
            if let Some(var) = scope.vars.borrow_mut().iter_mut().find(|v| v.name == name) {
                return Some(
                    match std::mem::replace(&mut var.slot, Slot::Value(Value::None)) {
                        Slot::Value(v) => v,
                        Slot::Fn(def) => Value::Closure(Rc::new(Closure {
                            def,
                            env: scope.clone(),
                        })),
                    },
                );
            }
            scope = scope.parent.as_ref()?;
        }
    }

    pub fn set_slot(&self, name: Sym, slot: Slot) {
        let mut vars = self.vars.borrow_mut();
        match vars.iter_mut().find(|v| v.name == name) {
            Some(var) => {
                var.slot = slot;
                var.written = true;
            }
            None => vars.push(Var {
                name,
                written: true,
                slot,
            }),
        }
    }

    /// Rebinds `name` in the nearest scope that defines it.
    pub fn assign(self: &Rc<Scope>, name: Sym, value: Value) -> bool {
        let mut scope = self;
        loop {
            if scope.vars.borrow().iter().any(|v| v.name == name) {
                scope.set(name, value);
                return true;
            }
            match &scope.parent {
                Some(p) => scope = p,
                None => return false,
            }
        }
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
                let written: Vec<(Sym, Value)> = t
                    .vars
                    .borrow()
                    .iter()
                    .filter(|v| v.written)
                    .map(|v| {
                        let value = match &v.slot {
                            Slot::Value(v) => v.clone(),
                            // Still closes over the forked scope, not ours
                            Slot::Fn(def) => Value::Closure(Rc::new(Closure {
                                def: def.clone(),
                                env: t.clone(),
                            })),
                        };
                        (v.name, value)
                    })
                    .collect();
                for (name, value) in written {
                    ours.set(name, value);
                }
            }
            theirs = t.parent.as_ref();
        }
    }
}

pub enum Callee {
    Fn(Rc<FuncDef>, Rc<Scope>),
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
            Slot::Fn(def) => Slot::Fn(def.clone()),
            Slot::Value(v) => Slot::Value(self.value(v)),
        };
        Var {
            name: var.name,
            written,
            slot,
        }
    }

    pub fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::None | Value::Int(_) | Value::Str(_) | Value::Builtin(_) => v.clone(),
            Value::Lazy(l) => {
                if l.is_done() {
                    return v.clone();
                }
                if let Some(copy) = self.lazies.get(&Rc::as_ptr(l)) {
                    return Value::Lazy(copy.clone());
                }
                let copy = Lazy::new(LazyState::Done(0));
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
