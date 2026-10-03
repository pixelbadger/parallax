//! Syntax tree and name interning.

use rustc_hash::FxHashMap as HashMap;
use std::fmt;
use std::rc::Rc;

/// An interned identifier. Names are resolved to symbols at parse time so
/// the interpreter never hashes or compares strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sym(u32);

#[derive(Debug)]
pub struct Interner {
    map: HashMap<Box<str>, Sym>,
    names: Vec<Box<str>>,
}

/// Names the interpreter itself needs, interned up front in this order.
pub mod well_known {
    use super::Sym;
    pub const MAIN: Sym = Sym(0);
    pub const ENSEMBLE: Sym = Sym(1);
    pub const PRINT: Sym = Sym(2);
    pub const SEED: Sym = Sym(3);
    pub const MIN: Sym = Sym(4);
    pub const MAX: Sym = Sym(5);
    pub const ABS: Sym = Sym(6);
    pub const N: Sym = Sym(7);
    pub const REJECTED: Sym = Sym(8);
    pub const TOTAL: Sym = Sym(9);
    pub const MEAN: Sym = Sym(10);
    pub const MEDIAN: Sym = Sym(11);
    pub const HITS: Sym = Sym(12);
    pub const RATE: Sym = Sym(13);
    pub const LEN: Sym = Sym(14);
    pub const ARRAY: Sym = Sym(15);
    pub const PUSH: Sym = Sym(16);
    pub(super) const NAMES: [&str; 17] = [
        "main", "Ensemble", "print", "seed", "min", "max", "abs", "n", "rejected", "total", "mean",
        "median", "hits", "rate", "len", "array", "push",
    ];
    /// Fields of the `Ensemble` struct a multiverse produces, in order.
    pub const ENSEMBLE_FIELDS: [Sym; 9] = [N, REJECTED, TOTAL, MEAN, MIN, MAX, MEDIAN, HITS, RATE];
}

impl Default for Interner {
    fn default() -> Self {
        let mut interner = Interner {
            map: HashMap::default(),
            names: Vec::new(),
        };
        for name in well_known::NAMES {
            interner.intern(name);
        }
        interner
    }
}

impl Interner {
    pub fn intern(&mut self, name: &str) -> Sym {
        if let Some(&sym) = self.map.get(name) {
            return sym;
        }
        let sym = Sym(u32::try_from(self.names.len()).expect("too many names"));
        self.names.push(name.into());
        self.map.insert(name.into(), sym);
        sym
    }

    pub fn name(&self, sym: Sym) -> &str {
        &self.names[sym.0 as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Or,
    And,
    Eq,
    Gt,
    Lt,
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
    Abs,
}

impl Op {
    /// Comparisons and logic produce booleans (as 0/1 integers).
    pub fn is_boolean(self) -> bool {
        matches!(self, Op::Or | Op::And | Op::Eq | Op::Gt | Op::Lt)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Or => "||",
            Op::And => "&&",
            Op::Eq => "==",
            Op::Gt => ">",
            Op::Lt => "<",
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Min => "min",
            Op::Max => "max",
            Op::Abs => "abs",
        })
    }
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Box<[Stmt]>,
    /// How many statements bind a name in the block's own scope. A block
    /// that binds nothing runs in its parent's scope (one of its own would
    /// be unobservable), and other scopes are allocated at the right size.
    pub binds: usize,
}

impl Block {
    pub fn new(stmts: Vec<Stmt>) -> Self {
        let binds = stmts
            .iter()
            .filter(|s| matches!(s, Stmt::Let(_) | Stmt::Pin(_) | Stmt::Func(_)))
            .count();
        Block {
            stmts: stmts.into(),
            binds,
        }
    }
}

#[derive(Debug)]
pub enum Expr {
    Int(i64),
    Str(Rc<str>),
    Var(Sym),
    /// `open` (0..=99) or `open(lo, hi)`.
    Open(Option<Box<(Expr, Expr)>>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Call(Sym, Box<[Expr]>),
    StructInit(Sym, Box<[(Sym, Expr)]>),
    Member(Box<Expr>, Sym),
    /// `[a, b, c]`
    Array(Box<[Expr]>),
    /// `array[index]`
    Index(Box<Expr>, Box<Expr>),
    Observe(Box<Expr>),
    Fork(Block),
    If(Box<Expr>, Block, Option<Block>),
    Repeat(Box<Expr>, Block),
    While(Box<Expr>, Block),
    Multiverse(Box<Expr>, Block),
}

impl Expr {
    /// Block expressions need no `;` when used as a statement.
    pub fn is_block_expr(&self) -> bool {
        matches!(
            self,
            Expr::Fork(_)
                | Expr::If(..)
                | Expr::Repeat(..)
                | Expr::While(..)
                | Expr::Multiverse(..)
        )
    }
}

/// A type annotation; only its state prefix is checked.
#[derive(Debug)]
pub enum Ann {
    Any,
    Open(Box<str>),
    Future(Box<str>),
    Collapsed(Box<str>),
}

#[derive(Debug)]
pub struct Binding {
    pub name: Sym,
    pub ann: Ann,
    pub expr: Expr,
}

#[derive(Debug)]
pub struct FuncDef {
    pub name: Sym,
    pub params: Box<[Sym]>,
    pub body: Block,
}

#[derive(Debug)]
pub enum Stmt {
    Let(Binding),
    Pin(Binding),
    Assign(Sym, Expr),
    /// `name[i].field[j] = expr;`: writes into part of a variable's value.
    AssignPath(Sym, Box<[Place]>, Expr),
    Reset(Sym),
    TypeDef(Sym, Box<[Sym]>),
    Commit(Sym),
    Discard(Sym),
    Given(Expr),
    Func(Rc<FuncDef>),
    Expr(Expr),
}

/// One step of an assignment target below its variable.
#[derive(Debug)]
pub enum Place {
    Index(Expr),
    Field(Sym),
}

pub type Program = Box<[Stmt]>;
