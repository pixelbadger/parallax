//! Resolves variables to scope slots, after parsing.
//!
//! Every scope at run time belongs to a block, and its parent is the scope of
//! the enclosing block that owns one, so the chain mirrors the source. This
//! gives each scope a fixed layout (one slot per name bound in it), and each
//! use of a name a short list of the slots it can be in (see [`Ref`]).
//!
//! The blocks that own a scope are function bodies and the bodies of `if`,
//! `repeat` and `while` that bind something (see [`Block::binds`]). Every
//! other block runs in the enclosing scope: `fork` and `multiverse` blocks in
//! a copy of it, so a `let` in one is a slot of the enclosing scope, and a
//! `commit` that brings it back out fills that slot.

use rustc_hash::FxHashMap as HashMap;
use std::rc::Rc;

use crate::ast::{Block, Expr, Loc, Place, Ref, Stmt, Sym};

/// The global scope's layout. It outlives one program, so a later program
/// run by the same interpreter sees the globals an earlier one bound.
#[derive(Debug, Default)]
pub struct Globals {
    slots: HashMap<Sym, u32>,
}

impl Globals {
    /// The slot for `name`, allocating one if it has none.
    pub fn slot(&mut self, name: Sym) -> u32 {
        let next = u32::try_from(self.slots.len()).expect("too many globals");
        *self.slots.entry(name).or_insert(next)
    }

    pub fn get(&self, name: Sym) -> Option<u32> {
        self.slots.get(&name).copied()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

pub fn resolve(program: &mut [Stmt], globals: &mut Globals) {
    let mut r = Resolver {
        globals,
        scopes: Vec::new(),
    };
    r.collect(program);
    r.stmts(program);
}

struct Resolver<'g> {
    globals: &'g mut Globals,
    /// Layouts of the scopes enclosing the current point, outermost first,
    /// not counting the global scope.
    scopes: Vec<Vec<Sym>>,
}

impl Resolver<'_> {
    /// The slot of `name` in the current scope, allocating one if needed.
    fn bind(&mut self, name: Sym) -> u32 {
        let Some(layout) = self.scopes.last_mut() else {
            return self.globals.slot(name);
        };
        let i = match layout.iter().position(|&n| n == name) {
            Some(i) => i,
            None => {
                layout.push(name);
                layout.len() - 1
            }
        };
        u32::try_from(i).expect("too many names in a scope")
    }

    fn reference(&mut self, r: &mut Ref) {
        let mut locs = Vec::new();
        for (up, layout) in self.scopes.iter().rev().enumerate() {
            if let Some(slot) = layout.iter().position(|&n| n == r.name) {
                locs.push(Loc {
                    up: up as u32,
                    slot: slot as u32,
                });
            }
        }
        r.depth = self.scopes.len() as u32;
        if let Some(slot) = self.globals.get(r.name) {
            locs.push(Loc { up: r.depth, slot });
        }
        r.locs = locs.into();
    }

    /// Resolves a block that owns its scope, which starts with `params`.
    fn scope(&mut self, block: &mut Block, params: &[Sym]) -> Box<[u32]> {
        self.scopes.push(Vec::new());
        let param_slots = params.iter().map(|&p| self.bind(p)).collect();
        // Every slot is known before any use is resolved, so a function can
        // use a name bound after it.
        self.collect(&block.stmts);
        self.stmts(&mut block.stmts);
        block.size = self.scopes.pop().expect("pushed above").len();
        param_slots
    }

    /// Resolves a nested block, which owns a scope only if it binds something.
    fn nested(&mut self, block: &mut Block) {
        if block.binds == 0 {
            self.stmts(&mut block.stmts);
        } else {
            self.scope(block, &[]);
        }
    }

    // --- Allocating the current scope's slots ---

    fn collect(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Let(b) | Stmt::Pin(b) => {
                    self.bind(b.name);
                    self.collect_expr(&b.expr);
                }
                Stmt::Func(def) => {
                    self.bind(def.name);
                }
                Stmt::Assign(_, e) | Stmt::Given(e) | Stmt::Expr(e) => self.collect_expr(e),
                Stmt::AssignPath(_, path, e) => {
                    for place in path.iter() {
                        if let Place::Index(i) = place {
                            self.collect_expr(i);
                        }
                    }
                    self.collect_expr(e);
                }
                Stmt::Reset(_) | Stmt::TypeDef(..) | Stmt::Commit(_) | Stmt::Discard(_) => {}
            }
        }
    }

    /// Finds the blocks in `expr` that run in the current scope.
    fn collect_expr(&mut self, expr: &Expr) {
        let mut visit = |e: &Expr| self.collect_expr(e);
        match expr {
            Expr::Int(_) | Expr::Str(_) | Expr::Var(_) | Expr::Open(None) => {}
            Expr::Open(Some(b)) => {
                visit(&b.0);
                visit(&b.1);
            }
            Expr::Binary(_, a, b) | Expr::Index(a, b) => {
                visit(a);
                visit(b);
            }
            Expr::Call(_, items) | Expr::Array(items) => items.iter().for_each(visit),
            Expr::StructInit(_, fields) => fields.iter().for_each(|(_, e)| visit(e)),
            Expr::Member(e, _) | Expr::Observe(e) => visit(e),
            Expr::Fork(b) => self.collect(&b.stmts),
            Expr::Multiverse(c, b) => {
                visit(c);
                self.collect(&b.stmts);
            }
            Expr::If(c, t, e) => {
                visit(c);
                self.collect_nested(t);
                if let Some(e) = e {
                    self.collect_nested(e);
                }
            }
            Expr::Repeat(c, b) | Expr::While(c, b) => {
                visit(c);
                self.collect_nested(b);
            }
        }
    }

    fn collect_nested(&mut self, block: &Block) {
        if block.binds == 0 {
            self.collect(&block.stmts);
        }
    }

    // --- Resolving uses ---

    fn stmts(&mut self, stmts: &mut [Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &mut Stmt) {
        match stmt {
            Stmt::Let(b) | Stmt::Pin(b) => {
                self.expr(&mut b.expr);
                b.slot = self.bind(b.name);
            }
            Stmt::Assign(r, e) => {
                self.expr(e);
                self.reference(r);
            }
            Stmt::AssignPath(r, path, e) => {
                self.expr(e);
                for place in path.iter_mut() {
                    if let Place::Index(i) = place {
                        self.expr(i);
                    }
                }
                self.reference(r);
            }
            Stmt::Commit(r) | Stmt::Discard(r) => self.reference(r),
            Stmt::Func(def) => {
                let slot = self.bind(def.name);
                let def = Rc::get_mut(def).expect("a new function is not shared");
                def.slot = slot;
                def.param_slots = self.scope(&mut def.body, &def.params);
            }
            Stmt::Given(e) | Stmt::Expr(e) => self.expr(e),
            Stmt::Reset(_) | Stmt::TypeDef(..) => {}
        }
    }

    fn expr(&mut self, expr: &mut Expr) {
        match expr {
            Expr::Int(_) | Expr::Str(_) | Expr::Open(None) => {}
            Expr::Var(r) => self.reference(r),
            Expr::Open(Some(b)) => {
                self.expr(&mut b.0);
                self.expr(&mut b.1);
            }
            Expr::Binary(_, a, b) | Expr::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::Call(r, args) => {
                self.reference(r);
                args.iter_mut().for_each(|a| self.expr(a));
            }
            Expr::Array(items) => items.iter_mut().for_each(|e| self.expr(e)),
            Expr::StructInit(_, fields) => fields.iter_mut().for_each(|(_, e)| self.expr(e)),
            Expr::Member(e, _) | Expr::Observe(e) => self.expr(e),
            Expr::Fork(b) => self.stmts(&mut b.stmts),
            Expr::Multiverse(c, b) => {
                self.expr(c);
                self.stmts(&mut b.stmts);
            }
            Expr::If(c, t, e) => {
                self.expr(c);
                self.nested(t);
                if let Some(e) = e {
                    self.nested(e);
                }
            }
            Expr::Repeat(c, b) | Expr::While(c, b) => {
                self.expr(c);
                self.nested(b);
            }
        }
    }
}
