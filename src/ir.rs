//! The checked program: resolved names, types, and every decision the
//! checker made (slots, promotions, which statistic) baked in.

use std::rc::Rc;

use crate::units::{Dim, Units};
use crate::value::Value;

pub type Hint = u32;
/// No display unit: shown in the dimension's canonical unit.
pub const NO_HINT: Hint = u32::MAX;

#[derive(Clone, Debug)]
pub enum Ty {
    Unit,
    Bool,
    Int,
    /// A float or quantity: its dimension, and the unit to show it in.
    Num(Dim, Hint),
    Str,
    Enum(u32),
    Rec(u32),
    Arr(Box<Ty>),
}

impl PartialEq for Ty {
    fn eq(&self, other: &Ty) -> bool {
        match (self, other) {
            (Ty::Unit, Ty::Unit)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Int, Ty::Int)
            | (Ty::Str, Ty::Str) => true,
            (Ty::Num(a, _), Ty::Num(b, _)) => a == b,
            (Ty::Enum(a), Ty::Enum(b)) | (Ty::Rec(a), Ty::Rec(b)) => a == b,
            (Ty::Arr(a), Ty::Arr(b)) => a == b,
            _ => false,
        }
    }
}

impl Ty {
    pub const FLOAT: Ty = Ty::Num(Dim::NONE, NO_HINT);

    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Num(..))
    }
}

#[derive(Clone, Debug)]
pub struct RecordDef {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
    pub action: bool,
}

#[derive(Clone, Debug)]
pub struct HintDef {
    pub name: String,
    pub scale: f64,
    pub atoms: Vec<(u32, i32)>,
}

/// An expression evaluated outside any function, with its own frame.
#[derive(Clone, Debug)]
pub struct Code {
    pub ex: Ex,
    pub nslots: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalKind {
    Const,
    Input,
    Derived,
}

#[derive(Clone, Debug)]
pub struct InputSpec {
    pub between: Option<(Code, Code)>,
    pub default: Option<Code>,
    /// Lengths `[T; n]` requires, outermost first (`None` for `[T]`).
    pub lens: Vec<Option<Code>>,
}

#[derive(Clone, Debug)]
pub struct GlobalDef {
    pub name: String,
    pub kind: GlobalKind,
    pub ty: Ty,
    pub init: Option<Code>,
    pub input: Option<InputSpec>,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<Ty>,
    pub nslots: u32,
    pub ret: Ty,
    pub body: Ex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dist {
    UniformInt,
    Uniform,
    Bernoulli,
    Categorical,
    Normal,
    LogNormal,
    Triangular,
    Empirical,
}

#[derive(Clone, Debug)]
pub enum FactBody {
    Draw(Dist, Vec<Ex>),
    Derived(Ex),
}

#[derive(Clone, Debug)]
pub struct FactDef {
    pub world: String,
    pub name: String,
    pub kind: crate::ast::FactKind,
    pub params: Vec<Ty>,
    /// For enum parameters, the enum (its variants are keyed by name).
    pub param_enums: Vec<Option<u32>>,
    pub nslots: u32,
    pub body: FactBody,
    pub ty: Ty,
    /// Hash of the world and fact names: the stable part of the key.
    pub prefix: u64,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub enum ModelDef {
    Decision {
        name: String,
        action: Ty,
        outcome: Ty,
        func: u32,
    },
    Sequential(Box<SeqModel>),
}

#[derive(Clone, Debug)]
pub struct SeqModel {
    pub name: String,
    pub horizon: Code,
    pub state: Ty,
    pub action: Ty,
    pub obs: Ty,
    pub outcome: Ty,
    pub init: u32,
    pub step: u32,
    pub stop: Option<u32>,
    pub observe: u32,
    pub belief: Option<u32>,
    pub outcome_fn: Option<u32>,
    pub invariants: Vec<(u32, u32)>,
}

impl ModelDef {
    pub fn name(&self) -> &str {
        match self {
            ModelDef::Decision { name, .. } => name,
            ModelDef::Sequential(m) => &m.name,
        }
    }

    pub fn outcome(&self) -> &Ty {
        match self {
            ModelDef::Decision { outcome, .. } => outcome,
            ModelDef::Sequential(m) => &m.outcome,
        }
    }

    pub fn action(&self) -> &Ty {
        match self {
            ModelDef::Decision { action, .. } => action,
            ModelDef::Sequential(m) => &m.action,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PolicyDef {
    pub name: String,
    pub oracle: bool,
    pub model: u32,
    /// The values a family ranges over, as an array.
    pub family: Option<(Code, Ty)>,
    pub func: u32,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct Metric {
    pub label: String,
    pub code: Code,
    pub ty: Ty,
    /// A bare `mean(x)` or `probability(x)`, with no filter: it gets a
    /// confidence interval, and paired differences between policies.
    pub simple: Option<(StatKind, Box<Ex>)>,
}

#[derive(Clone, Debug)]
pub struct Constraint {
    pub label: String,
    pub metric: Metric,
    pub op: Option<(CmpOp, Metric)>,
}

#[derive(Clone, Debug)]
pub struct StudyDef {
    pub name: String,
    pub model: u32,
    pub worlds: Option<Code>,
    pub seed: Option<Code>,
    pub with: Vec<(u32, Code)>,
    pub compare: Vec<u32>,
    pub constraints: Vec<Constraint>,
    pub objectives: Vec<(bool, Metric)>,
    pub reports: Vec<Metric>,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub units: Units,
    pub hints: Vec<HintDef>,
    pub records: Vec<RecordDef>,
    pub enums: Vec<EnumDef>,
    /// FNV hash of each variant name, by enum: fact keys use names.
    pub variant_keys: Vec<Vec<u64>>,
    pub globals: Vec<GlobalDef>,
    /// Globals in an order where each comes after everything it reads.
    pub global_order: Vec<u32>,
    pub fns: Vec<FnDef>,
    pub facts: Vec<FactDef>,
    pub worlds: Vec<String>,
    pub models: Vec<ModelDef>,
    pub policies: Vec<PolicyDef>,
    pub studies: Vec<StudyDef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Pow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatKind {
    Mean,
    Median,
    Quantile,
    Variance,
    StdDev,
    Probability,
    Cvar,
    Min,
    Max,
    Count,
}

impl StatKind {
    /// Whether computing it sorts the values.
    pub fn sorts(self) -> bool {
        matches!(self, StatKind::Median | StatKind::Quantile | StatKind::Cvar)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bi {
    Min2,
    Max2,
    Abs,
    Sqrt,
    Exp,
    Ln,
    Sin,
    Cos,
    Pow,
    Floor,
    Ceil,
    Round,
    Float,
    Clamp,
    Mod,
    Len,
    Fill,
    Sum,
    Any,
    All,
    ArgMin,
    ArgMax,
    /// A statistic over an array's elements.
    Stat(StatKind),
}

#[derive(Clone, Debug)]
pub enum Ex {
    Const(Value),
    Local(u32),
    Global(u32),
    Fact {
        fact: u32,
        args: Vec<Ex>,
        line: u32,
    },
    Neg(Box<Ex>, u32),
    Not(Box<Ex>),
    ToNum(Box<Ex>),
    Arith(ArOp, Box<Ex>, Box<Ex>, u32),
    Cmp(CmpOp, Box<Ex>, Box<Ex>),
    And(Box<Ex>, Box<Ex>),
    Or(Box<Ex>, Box<Ex>),
    Call(u32, Vec<Ex>),
    Builtin(Bi, Vec<Ex>, u32),
    Record(Vec<Ex>),
    Field(Box<Ex>, u32),
    Array(Vec<Ex>),
    Index(Box<Ex>, Box<Ex>, u32),
    Comp {
        slot: u32,
        iter: Box<Iter>,
        filter: Option<Box<Ex>>,
        body: Box<Ex>,
        line: u32,
    },
    If(Box<Ex>, Box<Ex>, Box<Ex>),
    Match(Box<Ex>, Vec<Arm>, u32),
    Variant(u32, Vec<Ex>),
    Block(Vec<St>, Box<Ex>),
    Forecast {
        model: u32,
        action: Box<Ex>,
        horizon: Option<Box<Ex>>,
        worlds: Box<Ex>,
        /// Imagine worlds `skip..skip + worlds`, to extend an earlier forecast.
        skip: Option<Box<Ex>>,
        then: Option<Box<Ex>>,
        line: u32,
    },
    Rollout {
        model: u32,
        action: Box<Ex>,
        horizon: Option<Box<Ex>>,
        then: Option<Box<Ex>>,
        line: u32,
    },
    /// In study metrics only: a statistic over the worlds' outcomes. The
    /// outcome is slot 0 while `arg` and `filter` are evaluated.
    Stat {
        kind: StatKind,
        arg: Box<Ex>,
        q: Option<Box<Ex>>,
        filter: Option<Box<Ex>>,
        line: u32,
    },
}

#[derive(Clone, Debug)]
pub struct Arm {
    /// `None` for `_`.
    pub tag: Option<u32>,
    pub binds: Vec<u32>,
    pub body: Ex,
}

#[derive(Clone, Debug)]
pub enum Iter {
    Range {
        lo: Ex,
        hi: Ex,
        inclusive: bool,
        step: Option<Ex>,
        /// Over floats or quantities rather than integers.
        num: bool,
    },
    Over(Ex),
}

#[derive(Clone, Debug)]
pub enum PathSeg {
    Field(u32),
    Index(Ex, u32),
}

#[derive(Clone, Debug)]
pub enum St {
    Let(u32, Ex),
    Set(u32, Vec<PathSeg>, Ex),
    For {
        slot: u32,
        iter: Iter,
        cond: Option<Ex>,
        body: Vec<St>,
        line: u32,
    },
    Assert(Ex, String, u32),
    /// A policy's reason, recorded when a decision is served.
    Note(Rc<str>, Ty, Ex),
    Expr(Ex),
}
