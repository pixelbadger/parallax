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
    pub const SQRT: Sym = Sym(17);
    pub const FLOAT: Sym = Sym(18);
    pub const INT: Sym = Sym(19);
    pub(super) const NAMES: [&str; 20] = [
        "main", "Ensemble", "print", "seed", "min", "max", "abs", "n", "rejected", "total", "mean",
        "median", "hits", "rate", "len", "array", "push", "sqrt", "float", "int",
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
    /// The unary built-ins below, like `Abs`, ignore their second operand.
    Sqrt,
    /// `float(x)`
    ToFloat,
    /// `int(x)`, rounding down
    ToInt,
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
            Op::Sqrt => "sqrt",
            Op::ToFloat => "float",
            Op::ToInt => "int",
        })
    }
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Box<[Stmt]>,
    /// How many statements bind a name in the block's own scope. A block
    /// that binds nothing runs in its parent's scope. That shows only when a
    /// `let` in a fork inside it is committed: it lands in the parent.
    pub binds: usize,
    /// Slots in the block's scope, if it has one: every name bound in it,
    /// including by `fork` and `multiverse` blocks that run in it. Set by
    /// [`resolve`](crate::resolve).
    pub size: usize,
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
            size: 0,
        }
    }
}

/// Where a variable may live: `up` scopes out from the current one, at
/// index `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub up: u32,
    pub slot: u32,
}

/// A use of a variable, resolved to the scopes that may bind it.
///
/// A scope's slots are allocated when it is created but filled as its
/// bindings run, so a slot can still be unbound when it is read (a use
/// before the `let`, or a function called before a name it uses is bound).
/// Lookup then falls back to the next candidate out, as a search by name
/// would. The global scope is a candidate if the name was a global when
/// this was resolved; a global bound later, by another program run by the
/// same interpreter, is found from `depth`.
#[derive(Debug)]
pub struct Ref {
    pub name: Sym,
    /// Innermost first. Empty until resolved.
    pub locs: Box<[Loc]>,
    /// How many scopes out the global scope is.
    pub depth: u32,
}

impl Ref {
    pub fn new(name: Sym) -> Self {
        Ref {
            name,
            locs: Box::default(),
            depth: 0,
        }
    }
}

#[derive(Debug)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(Rc<String>),
    Var(Ref),
    /// `open` (0..=99) or `open(lo, hi)`.
    Open(Option<Box<(Expr, Expr)>>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Call(Ref, Box<[Expr]>),
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
    /// Slot in the current scope.
    pub slot: u32,
    pub ann: Ann,
    pub expr: Expr,
}

#[derive(Debug)]
pub struct FuncDef {
    pub name: Sym,
    /// Slot in the defining scope.
    pub slot: u32,
    pub params: Box<[Sym]>,
    /// Each parameter's slot in the body's scope (a repeated name shares one).
    pub param_slots: Box<[u32]>,
    pub body: Block,
}

#[derive(Debug)]
pub enum Stmt {
    Let(Binding),
    Pin(Binding),
    Assign(Ref, Expr),
    /// `name[i].field[j] = expr;`: writes into part of a variable's value.
    AssignPath(Ref, Box<[Place]>, Expr),
    Reset(Sym),
    TypeDef(Sym, Box<[Sym]>),
    Commit(Ref),
    Discard(Ref),
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
