//! The syntax tree, as parsed. `check` turns it into the typed `ir`.

use crate::error::Span;

/// A unit as written: `p/kWh` is `[("p", 1), ("kWh", -1)]`.
pub type UnitExpr = Vec<(String, i32)>;

#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// A type name, or a single unit (`Int`, `Battery`, `kWh`).
    Name(String, Span),
    /// A compound unit (`p/kWh`, `m/s^2`).
    Unit(UnitExpr, Span),
    /// `[T]`, or `[T; n]` with a length the host must match.
    Array(Box<TypeExpr>, Option<Box<Expr>>, Span),
    /// `{ field: T, ... }`, only on the right of `type Name =`.
    Record(Vec<Field>, Span),
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Name(_, s)
            | TypeExpr::Unit(_, s)
            | TypeExpr::Array(_, _, s)
            | TypeExpr::Record(_, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone)]
pub enum Decl {
    /// `const` (literals and other constants) or `derived` (may use inputs).
    Const {
        name: String,
        ty: Option<TypeExpr>,
        value: Expr,
        derived: bool,
        span: Span,
    },
    Input {
        name: String,
        ty: TypeExpr,
        between: Option<(Expr, Expr)>,
        default: Option<Expr>,
        span: Span,
    },
    Unit {
        name: String,
        def: Option<Expr>,
        span: Span,
    },
    Type {
        name: String,
        ty: TypeExpr,
        span: Span,
    },
    /// `enum` or `action` with variants.
    Enum {
        name: String,
        variants: Vec<Variant>,
        action: bool,
        span: Span,
    },
    /// `action Name = type`.
    ActionType {
        name: String,
        ty: TypeExpr,
        span: Span,
    },
    World {
        name: String,
        items: Vec<WorldItem>,
        span: Span,
    },
    Fn(FnDecl),
    Model(ModelDecl),
    Policy(PolicyDecl),
    Study(StudyDecl),
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    Latent,
    Uncertain,
    Derived,
}

#[derive(Debug, Clone)]
pub struct WorldItem {
    pub kind: FactKind,
    pub name: String,
    pub params: Vec<Param>,
    pub ty: Option<TypeExpr>,
    /// The distribution after `~`, or the expression after `=`.
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ModelDecl {
    /// `model name(action: A) -> Outcome = ...`: one decision, one outcome.
    Decision {
        name: String,
        param: Param,
        ret: Option<TypeExpr>,
        body: Expr,
        span: Span,
    },
    /// `model name { horizon n; init = ...; step(s, a, t) = ...; ... }`.
    Sequential {
        name: String,
        horizon: Option<Expr>,
        clauses: Vec<FnDecl>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct PolicyDecl {
    pub name: String,
    pub oracle: bool,
    pub family: Option<(String, Iter)>,
    pub model: Option<(String, Span)>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StudyDecl {
    pub name: String,
    pub clauses: Vec<StudyClause>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StudyClause {
    Model(String, Span),
    Worlds(Expr),
    Seed(Expr),
    With(Vec<(String, Expr, Span)>),
    /// `None` is `compare all`.
    Compare(Option<Vec<(String, Span)>>, Span),
    Require(Expr),
    Minimize(Expr),
    Maximize(Expr),
    Report(Vec<Expr>),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Pow,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    /// A number with a unit: `5 min`, `29 p/kWh`, `30%`.
    Quantity(f64, UnitExpr),
    Str(String),
    Bool(bool),
    Name(String),
    Field(Box<Expr>, String),
    Call(Box<Expr>, Vec<Arg>),
    Index(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `e in unit`: the number of `unit`s in `e`.
    Convert(Box<Expr>, UnitExpr),
    Record(String, Vec<(String, Expr)>),
    Array(Vec<Expr>),
    Comp {
        body: Box<Expr>,
        var: String,
        iter: Box<Iter>,
        filter: Option<Box<Expr>>,
    },
    If(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    Match(Box<Expr>, Vec<Arm>),
    Block(Block),
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
    /// `mean(cost where pump_failed)`, in study metrics.
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct Iter {
    pub kind: IterKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum IterKind {
    Range {
        lo: Expr,
        hi: Expr,
        inclusive: bool,
        step: Option<Expr>,
    },
    Over(Expr),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Stmt {
    Let {
        name: String,
        mutable: bool,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Assign {
        target: Expr,
        value: Expr,
        span: Span,
    },
    For {
        var: String,
        iter: Iter,
        cond: Option<Expr>,
        body: Block,
        span: Span,
    },
    Iterate {
        count: Expr,
        body: Block,
        span: Span,
    },
    Assert {
        cond: Expr,
        msg: Option<String>,
        span: Span,
    },
    /// `note name = expr` in a policy: a reason reported with a decision.
    Note {
        name: String,
        value: Expr,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct Arm {
    pub pat: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wild,
    Variant {
        qual: Option<String>,
        name: String,
        binds: Vec<String>,
    },
}
