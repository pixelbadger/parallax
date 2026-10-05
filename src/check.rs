//! The checker: names, types, units and information boundaries.
//!
//! It turns the syntax tree into `ir::Program`, rejecting a program that
//! would let a policy read the world, recurse, mix up dimensions, or reveal
//! part of a world fact to a policy. Functions, facts and globals are checked
//! lazily on first use, which is also how recursion is caught: a function
//! still being checked when it is called again calls itself.

use std::collections::{HashMap, HashSet};

use crate::ast::{self, BinOp, Decl, Expr, ExprKind, FactKind, IterKind, Stmt, TypeExpr};
use crate::error::{Error, ErrorKind, Result, Span};
use crate::ir::*;
use crate::units::{Dim, Units, unit_name};
use crate::value::Value;
use crate::world::{fnv, key_prefix};

fn err(span: Span, msg: impl Into<String>) -> Error {
    Error::at(ErrorKind::Check, span, msg)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Ctx {
    #[default]
    Fn,
    /// A `const`: literals and other constants.
    Const,
    /// A `derived` value, an input's bounds, or a study's settings.
    Setup,
    Fact,
    Model,
    Observe,
    Belief,
    Policy {
        model: u32,
        oracle: bool,
    },
    /// A study metric; `row` inside a statistic, where outcome fields are.
    Metric {
        model: u32,
        row: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Todo,
    Doing,
    Done,
}

struct Local {
    name: String,
    slot: u32,
    ty: Ty,
    mutable: bool,
}

#[derive(Default)]
struct Frame {
    scopes: Vec<Vec<Local>>,
    nslots: u32,
    ctx: Ctx,
    world: Option<String>,
    globals_read: HashSet<u32>,
    fns_called: HashSet<u32>,
}

enum TypeName {
    Rec(u32),
    Enum(u32),
    Alias(Ty),
}

pub struct Checker<'a> {
    src: &'a str,
    units: Units,
    hints: Vec<HintDef>,
    hint_map: HashMap<Vec<(u32, i32)>, Hint>,
    type_names: HashMap<String, TypeName>,
    action_types: HashSet<String>,
    records: Vec<RecordDef>,
    enums: Vec<EnumDef>,
    variant_names: HashMap<String, Vec<(u32, u32)>>,

    global_decls: Vec<&'a Decl>,
    global_map: HashMap<String, u32>,
    global_status: Vec<Status>,
    globals: Vec<Option<GlobalDef>>,
    global_deps: Vec<HashSet<u32>>,

    fn_decls: Vec<&'a ast::FnDecl>,
    fn_map: HashMap<String, u32>,
    fn_status: Vec<Status>,
    fns: Vec<Option<FnDef>>,
    fn_globals: Vec<HashSet<u32>>,

    fact_decls: Vec<(&'a str, &'a ast::WorldItem)>,
    world_map: HashMap<String, HashMap<String, u32>>,
    fact_status: Vec<Status>,
    facts: Vec<Option<FactDef>>,

    models: Vec<ModelDef>,
    model_map: HashMap<String, u32>,
    policies: Vec<PolicyDef>,
    studies: Vec<StudyDef>,

    frame: Frame,
    /// In `observe`: the expression being checked is a record field's whole
    /// value, so reading a world fact here reveals all of it.
    direct: bool,
}

pub fn check(src: &str, program: &ast::Program) -> Result<Program> {
    let mut c = Checker {
        src,
        units: Units::new(),
        hints: Vec::new(),
        hint_map: HashMap::new(),
        type_names: HashMap::new(),
        action_types: HashSet::new(),
        records: Vec::new(),
        enums: Vec::new(),
        variant_names: HashMap::new(),
        global_decls: Vec::new(),
        global_map: HashMap::new(),
        global_status: Vec::new(),
        globals: Vec::new(),
        global_deps: Vec::new(),
        fn_decls: Vec::new(),
        fn_map: HashMap::new(),
        fn_status: Vec::new(),
        fns: Vec::new(),
        fn_globals: Vec::new(),
        fact_decls: Vec::new(),
        world_map: HashMap::new(),
        fact_status: Vec::new(),
        facts: Vec::new(),
        models: Vec::new(),
        model_map: HashMap::new(),
        policies: Vec::new(),
        studies: Vec::new(),
        frame: Frame::default(),
        direct: false,
    };
    c.program(program)?;
    let global_order = c.global_order()?;
    let variant_keys = c
        .enums
        .iter()
        .map(|e| e.variants.iter().map(|v| fnv(&v.name)).collect())
        .collect();
    let mut worlds: Vec<String> = c.world_map.keys().cloned().collect();
    worlds.sort();
    Ok(Program {
        units: c.units,
        hints: c.hints,
        records: c.records,
        enums: c.enums,
        variant_keys,
        globals: c.globals.into_iter().map(Option::unwrap).collect(),
        global_order,
        fns: c.fns.into_iter().map(Option::unwrap).collect(),
        facts: c.facts.into_iter().map(Option::unwrap).collect(),
        worlds,
        models: c.models,
        policies: c.policies,
        studies: c.studies,
    })
}

fn is_zero(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(0) => true,
        ExprKind::Float(x) => *x == 0.0,
        ExprKind::Neg(inner) => is_zero(inner),
        _ => false,
    }
}

/// A static integer exponent: `2`, `-1`.
fn int_literal(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Neg(inner) => int_literal(inner).map(|n| -n),
        _ => None,
    }
}

fn stat_name(name: &str) -> Option<(StatKind, Option<f64>)> {
    let k = match name {
        "mean" => StatKind::Mean,
        "median" => StatKind::Median,
        "quantile" => StatKind::Quantile,
        "variance" => StatKind::Variance,
        "stddev" => StatKind::StdDev,
        "probability" => StatKind::Probability,
        "cvar" => StatKind::Cvar,
        "min" => StatKind::Min,
        "max" => StatKind::Max,
        "count" => StatKind::Count,
        _ => {
            let pct = |rest: &str| -> Option<f64> {
                if !rest.is_empty() && rest.len() <= 2 && rest.bytes().all(|b| b.is_ascii_digit()) {
                    let n: f64 = rest.parse().ok()?;
                    (n > 0.0).then_some(n / 100.0)
                } else {
                    None
                }
            };
            if let Some(q) = name.strip_prefix("cvar").and_then(pct) {
                return Some((StatKind::Cvar, Some(q)));
            }
            if let Some(q) = name.strip_prefix('p').and_then(pct) {
                return Some((StatKind::Quantile, Some(q)));
            }
            return None;
        }
    };
    Some((k, None))
}

const DISTRIBUTIONS: [&str; 7] = [
    "uniform",
    "bernoulli",
    "categorical",
    "normal",
    "lognormal",
    "triangular",
    "empirical",
];

impl<'a> Checker<'a> {
    // ---------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------

    fn program(&mut self, p: &'a ast::Program) -> Result<()> {
        // Units first: types and literals use them.
        for d in &p.decls {
            if let Decl::Unit { name, def, span } = d {
                let r = match def {
                    None => self.units.add_base(name),
                    Some(e) => {
                        let (v, ue) = match &e.kind {
                            ExprKind::Quantity(v, ue) => (*v, ue.clone()),
                            ExprKind::Int(n) => (*n as f64, vec![]),
                            ExprKind::Float(x) => (*x, vec![]),
                            _ => {
                                return Err(err(
                                    e.span,
                                    "a unit is defined as a number and a unit, like `1.496e11 m`",
                                ));
                            }
                        };
                        let (scale, dim) = self.units.eval(&ue).map_err(|m| err(e.span, m))?;
                        self.units.add_derived(name, v * scale, dim)
                    }
                };
                r.map_err(|m| err(*span, m))?;
            }
        }
        self.declare_types(p)?;
        self.declare_names(p)?;

        for g in 0..self.global_decls.len() {
            self.global(g as u32)?;
        }
        for f in 0..self.fn_decls.len() {
            self.check_fn(f as u32, Span::default())?;
        }
        for f in 0..self.fact_decls.len() {
            self.fact(f as u32, Span::default())?;
        }
        for d in &p.decls {
            if let Decl::Model(m) = d {
                let def = self.model(m)?;
                self.models.push(def);
            }
        }
        for d in &p.decls {
            if let Decl::Policy(pd) = d {
                let def = self.policy(pd)?;
                self.policies.push(def);
            }
        }
        for d in &p.decls {
            if let Decl::Study(sd) = d {
                let def = self.study(sd)?;
                self.studies.push(def);
            }
        }
        Ok(())
    }

    fn declare_types(&mut self, p: &'a ast::Program) -> Result<()> {
        let builtin = ["Int", "Float", "Bool", "String"];
        let mut aliases: Vec<(&'a str, &'a TypeExpr, Span)> = Vec::new();
        let taken = |c: &Self, name: &str, span: Span| -> Result<()> {
            if c.type_names.contains_key(name) || builtin.contains(&name) {
                return Err(err(span, format!("type `{name}` is already defined")));
            }
            Ok(())
        };
        for d in &p.decls {
            match d {
                Decl::Type { name, ty, span } => {
                    taken(self, name, *span)?;
                    if let TypeExpr::Record(..) = ty {
                        let id = self.records.len() as u32;
                        self.records.push(RecordDef {
                            name: name.clone(),
                            fields: Vec::new(),
                        });
                        self.type_names.insert(name.clone(), TypeName::Rec(id));
                    } else {
                        aliases.push((name, ty, *span));
                    }
                }
                Decl::Enum {
                    name,
                    variants,
                    action,
                    span,
                } => {
                    taken(self, name, *span)?;
                    self.declare_enum(name, variants.iter().map(|v| v.name.clone()), *action);
                    if *action {
                        self.action_types.insert(name.clone());
                    }
                }
                Decl::ActionType { name, ty, span } => {
                    taken(self, name, *span)?;
                    self.action_types.insert(name.clone());
                    match ty {
                        TypeExpr::Name(n, _)
                            if !builtin.contains(&n.as_str())
                                && self.units.lookup(n).is_none()
                                && !p.decls.iter().any(|d| match d {
                                    Decl::Type { name, .. }
                                    | Decl::Enum { name, .. }
                                    | Decl::ActionType { name, .. } => name == n,
                                    _ => false,
                                }) =>
                        {
                            // `action Go = go`: a single variant.
                            self.declare_enum(name, std::iter::once(n.clone()), true);
                        }
                        TypeExpr::Record(..) => {
                            let id = self.records.len() as u32;
                            self.records.push(RecordDef {
                                name: name.clone(),
                                fields: Vec::new(),
                            });
                            self.type_names.insert(name.clone(), TypeName::Rec(id));
                        }
                        _ => aliases.push((name, ty, *span)),
                    }
                }
                _ => {}
            }
        }
        // Aliases may refer to each other, in any order.
        let mut pending = aliases;
        while !pending.is_empty() {
            let before = pending.len();
            let mut rest = Vec::new();
            let mut last_err = None;
            for (name, ty, span) in pending {
                match self.ty(ty) {
                    Ok(t) => {
                        self.type_names.insert(name.to_string(), TypeName::Alias(t));
                    }
                    Err(e) => {
                        last_err = Some(e);
                        rest.push((name, ty, span));
                    }
                }
            }
            if rest.len() == before {
                return Err(last_err.unwrap());
            }
            pending = rest;
        }
        // Record fields and variant payloads.
        for d in &p.decls {
            let (name, fields) = match d {
                Decl::Type {
                    name,
                    ty: TypeExpr::Record(fields, _),
                    ..
                }
                | Decl::ActionType {
                    name,
                    ty: TypeExpr::Record(fields, _),
                    ..
                } => (name, fields),
                Decl::Enum { name, variants, .. } => {
                    let Some(TypeName::Enum(id)) = self.type_names.get(name) else {
                        continue;
                    };
                    let id = *id;
                    for (i, v) in variants.iter().enumerate() {
                        let mut fs = Vec::new();
                        for f in &v.fields {
                            if fs.iter().any(|(n, _): &(String, Ty)| *n == f.name) {
                                return Err(err(f.span, format!("duplicate field `{}`", f.name)));
                            }
                            let t = self.ty(&f.ty)?;
                            fs.push((f.name.clone(), t));
                        }
                        self.enums[id as usize].variants[i].fields = fs;
                    }
                    continue;
                }
                _ => continue,
            };
            let Some(TypeName::Rec(id)) = self.type_names.get(name) else {
                continue;
            };
            let id = *id;
            let mut fs = Vec::new();
            for f in fields {
                if fs.iter().any(|(n, _): &(String, Ty)| *n == f.name) {
                    return Err(err(f.span, format!("duplicate field `{}`", f.name)));
                }
                let t = self.ty(&f.ty)?;
                fs.push((f.name.clone(), t));
            }
            self.records[id as usize].fields = fs;
        }
        Ok(())
    }

    fn declare_enum(&mut self, name: &str, variants: impl Iterator<Item = String>, action: bool) {
        let id = self.enums.len() as u32;
        let variants: Vec<VariantDef> = variants
            .map(|n| VariantDef {
                name: n,
                fields: Vec::new(),
            })
            .collect();
        for (i, v) in variants.iter().enumerate() {
            self.variant_names
                .entry(v.name.clone())
                .or_default()
                .push((id, i as u32));
        }
        self.enums.push(EnumDef {
            name: name.to_string(),
            variants,
            action,
        });
        self.type_names.insert(name.to_string(), TypeName::Enum(id));
    }

    fn declare_names(&mut self, p: &'a ast::Program) -> Result<()> {
        let mut seen: HashMap<String, &'static str> = HashMap::new();
        let mut claim = |name: &str, what: &'static str, span: Span| -> Result<()> {
            if let Some(prev) = seen.insert(name.to_string(), what) {
                return Err(err(
                    span,
                    format!("`{name}` is already declared (as a {prev})"),
                ));
            }
            Ok(())
        };
        for d in &p.decls {
            match d {
                Decl::Const { name, span, .. } | Decl::Input { name, span, .. } => {
                    claim(name, "value", *span)?;
                    let id = self.global_decls.len() as u32;
                    self.global_decls.push(d);
                    self.global_map.insert(name.clone(), id);
                    self.global_status.push(Status::Todo);
                    self.globals.push(None);
                    self.global_deps.push(HashSet::new());
                }
                Decl::Fn(f) => {
                    claim(&f.name, "function", f.span)?;
                    let id = self.fn_decls.len() as u32;
                    self.fn_decls.push(f);
                    self.fn_map.insert(f.name.clone(), id);
                    self.fn_status.push(Status::Todo);
                    self.fns.push(None);
                    self.fn_globals.push(HashSet::new());
                }
                Decl::World { name, items, span } => {
                    claim(name, "world", *span)?;
                    let mut map = HashMap::new();
                    for item in items {
                        if map.contains_key(&item.name) {
                            return Err(err(
                                item.span,
                                format!("`{name}.{}` is declared twice", item.name),
                            ));
                        }
                        let id = self.fact_decls.len() as u32;
                        self.fact_decls.push((name, item));
                        self.fact_status.push(Status::Todo);
                        self.facts.push(None);
                        map.insert(item.name.clone(), id);
                    }
                    self.world_map.insert(name.clone(), map);
                }
                Decl::Model(m) => {
                    let (name, span) = match m {
                        ast::ModelDecl::Decision { name, span, .. }
                        | ast::ModelDecl::Sequential { name, span, .. } => (name, span),
                    };
                    claim(name, "model", *span)?;
                    self.model_map
                        .insert(name.clone(), self.model_map.len() as u32);
                }
                Decl::Policy(pd) => claim(&pd.name, "policy", pd.span)?,
                Decl::Study(sd) => claim(&sd.name, "study", sd.span)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn global_order(&self) -> Result<Vec<u32>> {
        let n = self.globals.len();
        let mut state = vec![0u8; n];
        let mut order = Vec::new();
        fn visit(c: &Checker, g: usize, state: &mut [u8], order: &mut Vec<u32>) -> Result<()> {
            match state[g] {
                2 => return Ok(()),
                1 => {
                    let def = c.globals[g].as_ref().unwrap();
                    return Err(Error::line(
                        ErrorKind::Check,
                        def.line,
                        format!("`{}` depends on itself", def.name),
                    ));
                }
                _ => {}
            }
            state[g] = 1;
            let mut deps: Vec<u32> = c.global_deps[g].iter().copied().collect();
            deps.sort();
            for d in deps {
                visit(c, d as usize, state, order)?;
            }
            state[g] = 2;
            order.push(g as u32);
            Ok(())
        }
        for g in 0..n {
            visit(self, g, &mut state, &mut order)?;
        }
        Ok(order)
    }

    fn global(&mut self, id: u32) -> Result<Ty> {
        match self.global_status[id as usize] {
            Status::Done => return Ok(self.globals[id as usize].as_ref().unwrap().ty.clone()),
            Status::Doing => {
                let name = match self.global_decls[id as usize] {
                    Decl::Const { name, .. } | Decl::Input { name, .. } => name,
                    _ => unreachable!(),
                };
                return Err(Error::new(
                    ErrorKind::Check,
                    format!("`{name}` depends on itself"),
                ));
            }
            Status::Todo => {}
        }
        self.global_status[id as usize] = Status::Doing;
        let saved = std::mem::take(&mut self.frame);
        let def = match self.global_decls[id as usize] {
            Decl::Const {
                name,
                ty,
                value,
                derived,
                span,
            } => {
                let ctx = if *derived { Ctx::Setup } else { Ctx::Const };
                let want = ty.as_ref().map(|t| self.ty(t)).transpose()?;
                let code = self.code(ctx, |c| match &want {
                    Some(w) => Ok((c.expr_as(value, w)?, w.clone())),
                    None => c.expr(value),
                })?;
                GlobalDef {
                    name: name.clone(),
                    kind: if *derived {
                        GlobalKind::Derived
                    } else {
                        GlobalKind::Const
                    },
                    ty: code.1,
                    init: Some(code.0),
                    input: None,
                    line: span.line,
                }
            }
            Decl::Input {
                name,
                ty,
                between,
                default,
                span,
            } => {
                let t = self.ty(ty)?;
                let mut lens = Vec::new();
                let mut cur = ty;
                while let TypeExpr::Array(elem, len, _) = cur {
                    lens.push(match len {
                        Some(e) => Some(
                            self.code(Ctx::Const, |c| Ok((c.expr_as(e, &Ty::Int)?, Ty::Int)))?
                                .0,
                        ),
                        None => None,
                    });
                    cur = elem;
                }
                let between = match between {
                    Some((lo, hi)) => {
                        if !t.is_numeric() {
                            return Err(err(*span, "`between` needs a numeric input"));
                        }
                        let lo = self
                            .code(Ctx::Const, |c| Ok((c.expr_as(lo, &t)?, t.clone())))?
                            .0;
                        let hi = self
                            .code(Ctx::Const, |c| Ok((c.expr_as(hi, &t)?, t.clone())))?
                            .0;
                        Some((lo, hi))
                    }
                    None => None,
                };
                let default = match default {
                    Some(d) => Some(
                        self.code(Ctx::Const, |c| Ok((c.expr_as(d, &t)?, t.clone())))?
                            .0,
                    ),
                    None => None,
                };
                GlobalDef {
                    name: name.clone(),
                    kind: GlobalKind::Input,
                    ty: t,
                    init: None,
                    input: Some(InputSpec {
                        between,
                        default,
                        lens,
                    }),
                    line: span.line,
                }
            }
            _ => unreachable!(),
        };
        let mut deps = std::mem::take(&mut self.frame.globals_read);
        for f in std::mem::take(&mut self.frame.fns_called) {
            deps.extend(self.fn_globals[f as usize].iter().copied());
        }
        self.frame = saved;
        let ty = def.ty.clone();
        self.global_deps[id as usize] = deps;
        self.globals[id as usize] = Some(def);
        self.global_status[id as usize] = Status::Done;
        Ok(ty)
    }

    /// Check an expression in a fresh frame of its own.
    fn code(
        &mut self,
        ctx: Ctx,
        f: impl FnOnce(&mut Self) -> Result<(Ex, Ty)>,
    ) -> Result<(Code, Ty)> {
        let reads = std::mem::take(&mut self.frame.globals_read);
        let calls = std::mem::take(&mut self.frame.fns_called);
        let outer_scopes = std::mem::take(&mut self.frame.scopes);
        let outer_slots = self.frame.nslots;
        let outer_ctx = self.frame.ctx;
        self.frame.scopes = vec![Vec::new()];
        self.frame.nslots = 0;
        self.frame.ctx = ctx;
        let r = f(self);
        let nslots = self.frame.nslots;
        self.frame.scopes = outer_scopes;
        self.frame.nslots = outer_slots;
        self.frame.ctx = outer_ctx;
        self.frame.globals_read.extend(reads);
        self.frame.fns_called.extend(calls);
        let (ex, ty) = r?;
        Ok((Code { ex, nslots }, ty))
    }

    fn check_fn(&mut self, id: u32, call: Span) -> Result<()> {
        match self.fn_status[id as usize] {
            Status::Done => return Ok(()),
            Status::Doing => {
                return Err(err(
                    call,
                    format!(
                        "recursion is not allowed: `{}` calls itself. Use a bounded `for` loop \
                         instead",
                        self.fn_decls[id as usize].name
                    ),
                ));
            }
            Status::Todo => {}
        }
        self.fn_status[id as usize] = Status::Doing;
        let decl = self.fn_decls[id as usize];
        let saved = std::mem::take(&mut self.frame);
        let r = self.body(
            &decl.name,
            &decl.params,
            decl.ret.as_ref(),
            &decl.body,
            Ctx::Fn,
            None,
        );
        let mut globals = std::mem::take(&mut self.frame.globals_read);
        for f in std::mem::take(&mut self.frame.fns_called) {
            globals.extend(self.fn_globals[f as usize].iter().copied());
        }
        self.frame = saved;
        let def = r?;
        self.fn_globals[id as usize] = globals;
        self.fns[id as usize] = Some(def);
        self.fn_status[id as usize] = Status::Done;
        Ok(())
    }

    /// Check a function-like body in the current (fresh) frame.
    fn body(
        &mut self,
        name: &str,
        params: &[ast::Param],
        ret: Option<&TypeExpr>,
        body: &Expr,
        ctx: Ctx,
        first: Option<(String, Ty)>,
    ) -> Result<FnDef> {
        self.frame.ctx = ctx;
        self.frame.scopes = vec![Vec::new()];
        self.frame.nslots = 0;
        let mut ptys = Vec::new();
        if let Some((n, t)) = first {
            self.bind(&n, t.clone(), false);
            ptys.push(t);
        }
        for p in params {
            if self.frame.scopes[0].iter().any(|l| l.name == p.name) {
                return Err(err(p.span, format!("duplicate parameter `{}`", p.name)));
            }
            let t = self.ty(&p.ty)?;
            self.bind(&p.name, t.clone(), false);
            ptys.push(t);
        }
        let (ex, ty) = match ret {
            Some(r) => {
                let want = self.ty(r)?;
                self.direct = true;
                let ex = self.expr_as(body, &want)?;
                (ex, want)
            }
            None => {
                self.direct = true;
                self.expr(body)?
            }
        };
        Ok(FnDef {
            name: name.to_string(),
            params: ptys,
            nslots: self.frame.nslots,
            ret: ty,
            body: ex,
        })
    }

    fn fact(&mut self, id: u32, use_span: Span) -> Result<Ty> {
        match self.fact_status[id as usize] {
            Status::Done => return Ok(self.facts[id as usize].as_ref().unwrap().ty.clone()),
            Status::Doing => {
                let (w, item) = self.fact_decls[id as usize];
                return Err(err(
                    use_span,
                    format!("`{w}.{}` depends on itself", item.name),
                ));
            }
            Status::Todo => {}
        }
        self.fact_status[id as usize] = Status::Doing;
        let (world, item) = self.fact_decls[id as usize];
        let saved = std::mem::take(&mut self.frame);
        let r = self.fact_body(world, item);
        self.frame = saved;
        let def = r?;
        let ty = def.ty.clone();
        self.facts[id as usize] = Some(def);
        self.fact_status[id as usize] = Status::Done;
        Ok(ty)
    }

    fn fact_body(&mut self, world: &str, item: &ast::WorldItem) -> Result<FactDef> {
        self.frame.ctx = Ctx::Fact;
        self.frame.world = Some(world.to_string());
        self.frame.scopes = vec![Vec::new()];
        let mut params = Vec::new();
        let mut param_enums = Vec::new();
        for p in &item.params {
            let t = self.ty(&p.ty)?;
            let e = match &t {
                Ty::Int | Ty::Bool | Ty::Str => None,
                Ty::Enum(e)
                    if self.enums[*e as usize]
                        .variants
                        .iter()
                        .all(|v| v.fields.is_empty()) =>
                {
                    Some(*e)
                }
                _ => {
                    return Err(err(
                        p.span,
                        "fact parameters are keys: use Int, Bool, String or a plain enum",
                    ));
                }
            };
            self.bind(&p.name, t.clone(), false);
            params.push(t);
            param_enums.push(e);
        }
        let want = item.ty.as_ref().map(|t| self.ty(t)).transpose()?;
        let (body, mut ty) = if item.kind == FactKind::Derived {
            let (ex, ty) = match &want {
                Some(w) => (self.expr_as(&item.body, w)?, w.clone()),
                None => self.expr(&item.body)?,
            };
            (FactBody::Derived(ex), ty)
        } else {
            self.distribution(&item.body)?
        };
        if let Some(w) = want {
            if item.kind != FactKind::Derived {
                if w != ty && !(w == Ty::FLOAT && ty == Ty::Int) {
                    return Err(err(
                        item.span,
                        format!(
                            "this distribution gives {}, not {}",
                            self.ty_name(&ty),
                            self.ty_name(&w)
                        ),
                    ));
                }
                if w == Ty::FLOAT && ty == Ty::Int {
                    return Err(err(
                        item.span,
                        "uniform with integer bounds draws an Int: write the bounds as floats",
                    ));
                }
            }
            ty = w;
        }
        Ok(FactDef {
            world: world.to_string(),
            name: item.name.clone(),
            kind: item.kind,
            params,
            param_enums,
            nslots: self.frame.nslots,
            body,
            ty,
            prefix: key_prefix(world, &item.name),
            line: item.span.line,
        })
    }

    fn distribution(&mut self, e: &Expr) -> Result<(FactBody, Ty)> {
        let ExprKind::Call(callee, args) = &e.kind else {
            return Err(err(
                e.span,
                format!(
                    "expected a distribution after `~` ({})",
                    DISTRIBUTIONS.join(", ")
                ),
            ));
        };
        let ExprKind::Name(name) = &callee.kind else {
            return Err(err(e.span, "expected a distribution after `~`"));
        };
        if args.iter().any(|a| a.name.is_some() || a.filter.is_some()) {
            return Err(err(e.span, "distribution arguments are positional"));
        }
        let arity = |n: usize| -> Result<()> {
            if args.len() != n {
                return Err(err(
                    e.span,
                    format!(
                        "`{name}` takes {n} argument{}",
                        if n == 1 { "" } else { "s" }
                    ),
                ));
            }
            Ok(())
        };
        let a = |i: usize| &args[i].value;
        match name.as_str() {
            "uniform" => {
                arity(2)?;
                let (lo, lt) = self.expr(a(0))?;
                let (hi, ht) = self.expr(a(1))?;
                if lt == Ty::Int && ht == Ty::Int {
                    return Ok((FactBody::Draw(Dist::UniformInt, vec![lo, hi]), Ty::Int));
                }
                let _ = (lo, hi, lt, ht);
                let (xs, t) = self.joined(&[a(0), a(1)], true)?;
                Ok((FactBody::Draw(Dist::Uniform, xs), t))
            }
            "bernoulli" => {
                arity(1)?;
                let p = self.expr_as(a(0), &Ty::FLOAT)?;
                Ok((FactBody::Draw(Dist::Bernoulli, vec![p]), Ty::Bool))
            }
            "normal" | "triangular" => {
                let n = if name == "normal" { 2 } else { 3 };
                arity(n)?;
                let refs: Vec<&Expr> = args.iter().map(|a| &a.value).collect();
                let (xs, t) = self.joined(&refs, true)?;
                let dist = if name == "normal" {
                    Dist::Normal
                } else {
                    Dist::Triangular
                };
                Ok((FactBody::Draw(dist, xs), t))
            }
            "lognormal" => {
                arity(2)?;
                let (m, t) = self.joined(&[a(0)], true)?;
                let s = self.expr_as(a(1), &Ty::FLOAT)?;
                Ok((FactBody::Draw(Dist::LogNormal, vec![m[0].clone(), s]), t))
            }
            "categorical" => {
                arity(2)?;
                let (vals, vt) = self.expr(a(0))?;
                let Ty::Arr(elem) = vt else {
                    return Err(err(
                        a(0).span,
                        "categorical(values, weights) needs an array of values",
                    ));
                };
                let w = self.expr_as(a(1), &Ty::Arr(Box::new(Ty::FLOAT)))?;
                Ok((FactBody::Draw(Dist::Categorical, vec![vals, w]), *elem))
            }
            "empirical" => {
                arity(1)?;
                let (vals, vt) = self.expr(a(0))?;
                let Ty::Arr(elem) = vt else {
                    return Err(err(a(0).span, "empirical(samples) needs an array"));
                };
                Ok((FactBody::Draw(Dist::Empirical, vec![vals]), *elem))
            }
            _ => Err(err(
                callee.span,
                format!(
                    "`{name}` is not a distribution (use {})",
                    DISTRIBUTIONS.join(", ")
                ),
            )),
        }
    }

    // ---------------------------------------------------------------
    // Models, policies, studies
    // ---------------------------------------------------------------

    fn action_param(&mut self, p: &ast::Param) -> Result<Ty> {
        match &p.ty {
            TypeExpr::Name(n, _) if self.action_types.contains(n) => self.ty(&p.ty),
            t => Err(err(
                t.span(),
                format!(
                    "the action `{}` must have a type declared with `action`, so decisions are \
                     marked as decisions",
                    p.name
                ),
            )),
        }
    }

    fn add_fn(&mut self, def: FnDef) -> u32 {
        let id = self.fns.len() as u32;
        self.fns.push(Some(def));
        self.fn_status.push(Status::Done);
        self.fn_globals.push(HashSet::new());
        id
    }

    fn clause_fn(
        &mut self,
        model: &str,
        c: &ast::FnDecl,
        ctx: Ctx,
        want_params: &[(&str, Option<&Ty>)],
        want_ret: Option<&Ty>,
    ) -> Result<(u32, Ty, Vec<Ty>)> {
        if c.params.len() != want_params.len() {
            let names: Vec<_> = want_params.iter().map(|(n, _)| *n).collect();
            return Err(err(
                c.span,
                format!(
                    "`{}` takes {} parameter{}: ({})",
                    c.name,
                    names.len(),
                    if names.len() == 1 { "" } else { "s" },
                    names.join(", ")
                ),
            ));
        }
        let saved = std::mem::take(&mut self.frame);
        let r = self.body(
            &format!("{model}.{}", c.name),
            &c.params,
            c.ret.as_ref(),
            &c.body,
            ctx,
            None,
        );
        self.frame = saved;
        let def = r?;
        for (i, (what, want)) in want_params.iter().enumerate() {
            if let Some(w) = want
                && def.params[i] != **w
            {
                return Err(err(
                    c.params[i].span,
                    format!(
                        "`{}` here is the {what}, of type {}, not {}",
                        c.params[i].name,
                        self.ty_name(w),
                        self.ty_name(&def.params[i])
                    ),
                ));
            }
        }
        if let Some(w) = want_ret
            && def.ret != *w
        {
            return Err(err(
                c.body.span,
                format!(
                    "`{}` must return {}, not {}",
                    c.name,
                    self.ty_name(w),
                    self.ty_name(&def.ret)
                ),
            ));
        }
        let ret = def.ret.clone();
        let params = def.params.clone();
        Ok((self.add_fn(def), ret, params))
    }

    fn model(&mut self, m: &'a ast::ModelDecl) -> Result<ModelDef> {
        match m {
            ast::ModelDecl::Decision {
                name,
                param,
                ret,
                body,
                ..
            } => {
                let action = self.action_param(param)?;
                let saved = std::mem::take(&mut self.frame);
                let r = self.body(
                    name,
                    std::slice::from_ref(param),
                    ret.as_ref(),
                    body,
                    Ctx::Model,
                    None,
                );
                self.frame = saved;
                let def = r?;
                let outcome = def.ret.clone();
                self.check_outcome(&outcome, body.span)?;
                let func = self.add_fn(def);
                Ok(ModelDef::Decision {
                    name: name.clone(),
                    action,
                    outcome,
                    func,
                })
            }
            ast::ModelDecl::Sequential {
                name,
                horizon,
                clauses,
                span,
            } => {
                for c in clauses {
                    if ![
                        "init",
                        "step",
                        "stop",
                        "observe",
                        "belief",
                        "outcome",
                        "invariant",
                    ]
                    .contains(&c.name.as_str())
                    {
                        return Err(err(
                            c.span,
                            format!(
                                "unknown model clause `{}` (expected horizon, init, step, stop, \
                                 observe, belief, outcome or invariant)",
                                c.name
                            ),
                        ));
                    }
                    if c.name != "invariant"
                        && clauses.iter().filter(|d| d.name == c.name).count() > 1
                    {
                        return Err(err(c.span, format!("`{}` is defined twice", c.name)));
                    }
                }
                let need = |n: &str| -> Result<&'a ast::FnDecl> {
                    clauses
                        .iter()
                        .find(|c| c.name == n)
                        .ok_or_else(|| err(*span, format!("model `{name}` needs a `{n}` clause")))
                };
                let horizon = horizon
                    .as_ref()
                    .ok_or_else(|| err(*span, format!("model `{name}` needs a `horizon`")))?;
                let (horizon, _) =
                    self.code(Ctx::Setup, |c| Ok((c.expr_as(horizon, &Ty::Int)?, Ty::Int)))?;

                let init_c = need("init")?;
                let (init, state, _) = self.clause_fn(name, init_c, Ctx::Model, &[], None)?;
                let step_c = need("step")?;
                if step_c.params.len() != 3 {
                    return Err(err(step_c.span, "`step` takes (state, action, step)"));
                }
                let action = self.action_param(&step_c.params[1])?;
                let (step, _, _) = self.clause_fn(
                    name,
                    step_c,
                    Ctx::Model,
                    &[
                        ("state", Some(&state)),
                        ("action", Some(&action)),
                        ("step number", Some(&Ty::Int)),
                    ],
                    Some(&state),
                )?;
                let stop = match clauses.iter().find(|c| c.name == "stop") {
                    Some(c) => Some(
                        self.clause_fn(
                            name,
                            c,
                            Ctx::Model,
                            &[("state", Some(&state))],
                            Some(&Ty::Bool),
                        )?
                        .0,
                    ),
                    None => None,
                };
                let obs_c = need("observe")?;
                let (observe, obs, _) = self.clause_fn(
                    name,
                    obs_c,
                    Ctx::Observe,
                    &[("state", Some(&state)), ("step number", Some(&Ty::Int))],
                    None,
                )?;
                let belief = match clauses.iter().find(|c| c.name == "belief") {
                    Some(c) => Some(
                        self.clause_fn(
                            name,
                            c,
                            Ctx::Belief,
                            &[("observation", Some(&obs)), ("step number", Some(&Ty::Int))],
                            Some(&state),
                        )?
                        .0,
                    ),
                    None if obs == state => None,
                    None => {
                        return Err(err(
                            *span,
                            format!(
                                "model `{name}` observes {} but its state is {}: add \
                                 `belief(o: {}, t: Int) -> {}`, the state a forecast starts from",
                                self.ty_name(&obs),
                                self.ty_name(&state),
                                self.ty_name(&obs),
                                self.ty_name(&state)
                            ),
                        ));
                    }
                };
                let (outcome_fn, outcome) = match clauses.iter().find(|c| c.name == "outcome") {
                    Some(c) => {
                        let (f, t, _) =
                            self.clause_fn(name, c, Ctx::Model, &[("state", Some(&state))], None)?;
                        (Some(f), t)
                    }
                    None => (None, state.clone()),
                };
                self.check_outcome(&outcome, *span)?;
                let mut invariants = Vec::new();
                for c in clauses.iter().filter(|c| c.name == "invariant") {
                    let (f, _, _) = self.clause_fn(
                        name,
                        c,
                        Ctx::Model,
                        &[("state", Some(&state))],
                        Some(&Ty::Bool),
                    )?;
                    invariants.push((f, c.span.line));
                }
                Ok(ModelDef::Sequential(Box::new(SeqModel {
                    name: name.clone(),
                    horizon,
                    state,
                    action,
                    obs,
                    outcome,
                    init,
                    step,
                    stop,
                    observe,
                    belief,
                    outcome_fn,
                    invariants,
                })))
            }
        }
    }

    fn check_outcome(&self, t: &Ty, span: Span) -> Result<()> {
        match t {
            Ty::Unit => Err(err(span, "a model's outcome can't be empty")),
            _ => Ok(()),
        }
    }

    fn policy(&mut self, pd: &'a ast::PolicyDecl) -> Result<PolicyDef> {
        let span = pd.span;
        let want_ret = pd.ret.as_ref().map(|t| self.ty(t)).transpose()?;
        let mut param_tys = Vec::new();
        for p in &pd.params {
            param_tys.push(self.ty(&p.ty)?);
        }
        let fits = |c: &Self, m: &ModelDef| -> bool {
            if let Some(r) = &want_ret
                && r != m.action()
            {
                return false;
            }
            match m {
                ModelDef::Decision { .. } => param_tys.is_empty(),
                ModelDef::Sequential(s) => {
                    let first = if pd.oracle { &s.state } else { &s.obs };
                    let _ = c;
                    param_tys.len() == 2 && param_tys[0] == *first && param_tys[1] == Ty::Int
                }
            }
        };
        let model = match &pd.model {
            Some((m, mspan)) => {
                let id = *self
                    .model_map
                    .get(m)
                    .ok_or_else(|| err(*mspan, format!("unknown model `{m}`")))?;
                if !fits(self, &self.models[id as usize]) {
                    return Err(err(
                        span,
                        self.policy_shape(&self.models[id as usize], pd.oracle),
                    ));
                }
                id
            }
            None => {
                let matches: Vec<u32> = (0..self.models.len() as u32)
                    .filter(|&i| fits(self, &self.models[i as usize]))
                    .collect();
                match matches.as_slice() {
                    [one] => *one,
                    [] => {
                        let hint = match self.models.first() {
                            Some(m) if self.models.len() == 1 => {
                                format!(": {}", self.policy_shape(m, pd.oracle))
                            }
                            _ => String::new(),
                        };
                        return Err(err(
                            span,
                            format!("policy `{}` doesn't fit any model{hint}", pd.name),
                        ));
                    }
                    several => {
                        // Try the body against each: the action it returns
                        // usually decides.
                        let mut ok = Vec::new();
                        for &m in several {
                            let saved = std::mem::take(&mut self.frame);
                            let fam = match &pd.family {
                                Some((var, iter)) => self
                                    .code(Ctx::Setup, |c| {
                                        c.iter(iter).map(|(_, t)| (Ex::Const(Value::Unit), t))
                                    })
                                    .ok()
                                    .map(|(_, t)| (var.clone(), t)),
                                None => None,
                            };
                            let r = self.body(
                                &pd.name,
                                &pd.params,
                                pd.ret.as_ref(),
                                &pd.body,
                                Ctx::Policy {
                                    model: m,
                                    oracle: pd.oracle,
                                },
                                fam,
                            );
                            self.frame = saved;
                            if let Ok(def) = r {
                                let action = self.models[m as usize].action();
                                if self.coerce(&pd.body, def.body, &def.ret, action).is_ok() {
                                    ok.push(m);
                                }
                            }
                        }
                        match ok.as_slice() {
                            [one] => *one,
                            _ => {
                                return Err(err(
                                    span,
                                    format!(
                                        "policy `{}` fits more than one model: write `policy {} \
                                         for MODEL`",
                                        pd.name, pd.name
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        };
        let family = match &pd.family {
            Some((var, iter)) => {
                let (code, ty) = self.code(Ctx::Setup, |c| {
                    let (it, ety) = c.iter(iter)?;
                    let slot = c.bind(var, ety.clone(), false);
                    Ok((
                        Ex::Comp {
                            slot,
                            iter: Box::new(it),
                            filter: None,
                            body: Box::new(Ex::Local(slot)),
                            line: iter.span.line,
                        },
                        ety,
                    ))
                })?;
                Some((var.clone(), code, ty))
            }
            None => None,
        };
        let saved = std::mem::take(&mut self.frame);
        let action = self.models[model as usize].action().clone();
        let ret = pd.ret.as_ref();
        let r = self.body(
            &pd.name,
            &pd.params,
            ret,
            &pd.body,
            Ctx::Policy {
                model,
                oracle: pd.oracle,
            },
            family.as_ref().map(|(v, _, t)| (v.clone(), t.clone())),
        );
        self.frame = saved;
        let mut def = r?;
        if ret.is_none() {
            def.body = self
                .coerce(&pd.body, def.body.clone(), &def.ret, &action)
                .map_err(|_| {
                    err(
                        pd.body.span,
                        format!(
                            "policy `{}` returns {}, but model `{}` takes {}",
                            pd.name,
                            self.ty_name(&def.ret),
                            self.models[model as usize].name(),
                            self.ty_name(&action)
                        ),
                    )
                })?;
            def.ret = action;
        }
        let func = self.add_fn(def);
        Ok(PolicyDef {
            name: pd.name.clone(),
            oracle: pd.oracle,
            model,
            family: family.map(|(_, c, t)| (c, t)),
            func,
            line: span.line,
        })
    }

    fn policy_shape(&self, m: &ModelDef, oracle: bool) -> String {
        match m {
            ModelDef::Decision { name, action, .. } => format!(
                "model `{name}` makes one decision, so its policies take no parameters and \
                 return {}",
                self.ty_name(action)
            ),
            ModelDef::Sequential(s) => {
                let (n, t) = if oracle {
                    ("s", &s.state)
                } else {
                    ("o", &s.obs)
                };
                format!(
                    "model `{}` needs {}policies of the form `({n}: {}, t: Int) -> {}`",
                    s.name,
                    if oracle { "oracle " } else { "" },
                    self.ty_name(t),
                    self.ty_name(&s.action)
                )
            }
        }
    }

    fn study(&mut self, sd: &'a ast::StudyDecl) -> Result<StudyDef> {
        use ast::StudyClause as C;
        let mut model = None;
        for c in &sd.clauses {
            if let C::Model(m, span) = c {
                if model.is_some() {
                    return Err(err(*span, "a study has one model"));
                }
                model = Some(
                    *self
                        .model_map
                        .get(m)
                        .ok_or_else(|| err(*span, format!("unknown model `{m}`")))?,
                );
            }
        }
        let model = match model {
            Some(m) => m,
            None if self.models.len() == 1 => 0,
            None if self.models.is_empty() => {
                return Err(err(sd.span, "a study needs a model to evaluate"));
            }
            None => {
                return Err(err(
                    sd.span,
                    "this program has several models: say which with `model NAME`",
                ));
            }
        };
        let mut def = StudyDef {
            name: sd.name.clone(),
            model,
            worlds: None,
            seed: None,
            with: Vec::new(),
            compare: Vec::new(),
            constraints: Vec::new(),
            objectives: Vec::new(),
            reports: Vec::new(),
            line: sd.span.line,
        };
        let mut compared = false;
        for c in &sd.clauses {
            match c {
                C::Model(..) => {}
                C::Worlds(e) | C::Seed(e) => {
                    let code = self
                        .code(Ctx::Setup, |c| Ok((c.expr_as(e, &Ty::Int)?, Ty::Int)))?
                        .0;
                    let slot = if matches!(c, C::Worlds(_)) {
                        &mut def.worlds
                    } else {
                        &mut def.seed
                    };
                    if slot.is_some() {
                        return Err(err(e.span, "this setting is given twice"));
                    }
                    *slot = Some(code);
                }
                C::With(list) => {
                    for (name, e, span) in list {
                        let g = match self.global_map.get(name) {
                            Some(&g)
                                if self.globals[g as usize].as_ref().unwrap().kind
                                    == GlobalKind::Input =>
                            {
                                g
                            }
                            _ => {
                                return Err(err(*span, format!("`{name}` is not an input")));
                            }
                        };
                        let ty = self.globals[g as usize].as_ref().unwrap().ty.clone();
                        let code = self
                            .code(Ctx::Const, |c| Ok((c.expr_as(e, &ty)?, ty.clone())))?
                            .0;
                        def.with.push((g, code));
                    }
                }
                C::Compare(list, span) => {
                    compared = true;
                    match list {
                        None => {
                            def.compare.extend(
                                (0..self.policies.len() as u32)
                                    .filter(|&p| self.policies[p as usize].model == model),
                            );
                        }
                        Some(names) => {
                            for (n, s) in names {
                                let p = self
                                    .policies
                                    .iter()
                                    .position(|p| p.name == *n)
                                    .ok_or_else(|| err(*s, format!("unknown policy `{n}`")))?
                                    as u32;
                                if self.policies[p as usize].model != model {
                                    return Err(err(
                                        *s,
                                        format!(
                                            "policy `{n}` is for model `{}`, not `{}`",
                                            self.models[self.policies[p as usize].model as usize]
                                                .name(),
                                            self.models[model as usize].name()
                                        ),
                                    ));
                                }
                                if !def.compare.contains(&p) {
                                    def.compare.push(p);
                                }
                            }
                        }
                    }
                    if def.compare.is_empty() {
                        return Err(err(*span, "this study has no policies to compare"));
                    }
                }
                C::Require(e) => {
                    let label = self.label(e.span);
                    let constraint = match &e.kind {
                        ExprKind::Binary(
                            op @ (BinOp::Lt
                            | BinOp::Gt
                            | BinOp::Le
                            | BinOp::Ge
                            | BinOp::Eq
                            | BinOp::Ne),
                            l,
                            r,
                        ) => {
                            let lm = self.metric(model, l)?;
                            let rm = self.metric(model, r)?;
                            let ok = lm.ty == rm.ty
                                || (lm.ty.is_numeric()
                                    && rm.ty.is_numeric()
                                    && (is_zero(r) || is_zero(l)))
                                || (matches!((&lm.ty, &rm.ty), (Ty::Int, Ty::Num(d, _)) | (Ty::Num(d, _), Ty::Int) if d.is_none()));
                            if !ok {
                                return Err(err(
                                    e.span,
                                    format!(
                                        "can't compare {} with {}",
                                        self.ty_name(&lm.ty),
                                        self.ty_name(&rm.ty)
                                    ),
                                ));
                            }
                            let op = match op {
                                BinOp::Lt => CmpOp::Lt,
                                BinOp::Gt => CmpOp::Gt,
                                BinOp::Le => CmpOp::Le,
                                BinOp::Ge => CmpOp::Ge,
                                BinOp::Eq => CmpOp::Eq,
                                _ => CmpOp::Ne,
                            };
                            Constraint {
                                label,
                                metric: lm,
                                op: Some((op, rm)),
                            }
                        }
                        _ => {
                            let m = self.metric(model, e)?;
                            if m.ty != Ty::Bool {
                                return Err(err(e.span, "`require` needs a condition"));
                            }
                            Constraint {
                                label,
                                metric: m,
                                op: None,
                            }
                        }
                    };
                    def.constraints.push(constraint);
                }
                C::Minimize(e) | C::Maximize(e) => {
                    let m = self.metric(model, e)?;
                    if !matches!(m.ty, Ty::Int | Ty::Num(..)) {
                        return Err(err(
                            e.span,
                            format!(
                                "an objective is a single number, not {}",
                                self.ty_name(&m.ty)
                            ),
                        ));
                    }
                    def.objectives.push((matches!(c, C::Maximize(_)), m));
                }
                C::Report(list) => {
                    for e in list {
                        let m = self.metric(model, e)?;
                        def.reports.push(m);
                    }
                }
            }
        }
        if !compared {
            def.compare.extend(
                (0..self.policies.len() as u32)
                    .filter(|&p| self.policies[p as usize].model == model),
            );
            if def.compare.is_empty() {
                return Err(err(
                    sd.span,
                    format!(
                        "model `{}` has no policies to compare",
                        self.models[model as usize].name()
                    ),
                ));
            }
        }
        Ok(def)
    }

    fn label(&self, span: Span) -> String {
        let text = &self.src[span.start as usize..span.end as usize];
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn metric(&mut self, model: u32, e: &Expr) -> Result<Metric> {
        let label = self.label(e.span);
        let (code, ty) = self.code(Ctx::Metric { model, row: false }, |c| {
            // slot 0: the outcome row, while a statistic's argument runs
            c.frame.nslots = 1;
            c.expr(e)
        })?;
        let simple = match &code.ex {
            Ex::Stat {
                kind: kind @ (StatKind::Mean | StatKind::Probability),
                arg,
                filter: None,
                ..
            } if matches!(ty, Ty::Num(..)) => Some((*kind, arg.clone())),
            _ => None,
        };
        Ok(Metric {
            label,
            code,
            ty,
            simple,
        })
    }

    // ---------------------------------------------------------------
    // Types and units
    // ---------------------------------------------------------------

    pub fn ty_name(&self, t: &Ty) -> String {
        match t {
            Ty::Unit => "nothing".into(),
            Ty::Bool => "Bool".into(),
            Ty::Int => "Int".into(),
            Ty::Str => "String".into(),
            Ty::Num(d, h) => {
                if *h != NO_HINT {
                    self.hints[*h as usize].name.clone()
                } else if d.is_none() {
                    "Float".into()
                } else {
                    self.units.canonical(*d)
                }
            }
            Ty::Enum(e) => self.enums[*e as usize].name.clone(),
            Ty::Rec(r) => self.records[*r as usize].name.clone(),
            Ty::Arr(t) => format!("[{}]", self.ty_name(t)),
        }
    }

    fn ty(&mut self, t: &TypeExpr) -> Result<Ty> {
        match t {
            TypeExpr::Name(n, span) => match n.as_str() {
                "Int" => Ok(Ty::Int),
                "Float" => Ok(Ty::FLOAT),
                "Bool" => Ok(Ty::Bool),
                "String" => Ok(Ty::Str),
                _ => match self.type_names.get(n) {
                    Some(TypeName::Rec(r)) => Ok(Ty::Rec(*r)),
                    Some(TypeName::Enum(e)) => Ok(Ty::Enum(*e)),
                    Some(TypeName::Alias(t)) => Ok(t.clone()),
                    None => {
                        if self.units.lookup(n).is_some() {
                            let (_, dim, hint) = self.hint_of(&[(n.clone(), 1)], *span)?;
                            Ok(Ty::Num(dim, hint))
                        } else {
                            Err(err(*span, format!("unknown type or unit `{n}`")))
                        }
                    }
                },
            },
            TypeExpr::Unit(ue, span) => {
                let (_, dim, hint) = self.hint_of(ue, *span)?;
                Ok(Ty::Num(dim, hint))
            }
            TypeExpr::Array(elem, _, _) => Ok(Ty::Arr(Box::new(self.ty(elem)?))),
            TypeExpr::Record(_, span) => Err(err(
                *span,
                "record types are declared with `type Name = { ... }`",
            )),
        }
    }

    fn intern_hint(&mut self, mut atoms: Vec<(u32, i32)>) -> Hint {
        atoms.retain(|(_, e)| *e != 0);
        // A dimensional result doesn't need dimensionless atoms (`kWh·%`).
        let dim = atoms.iter().fold(Dim::NONE, |d, (u, e)| {
            d.times(self.units.defs[*u as usize].dim.pow(*e))
        });
        if !dim.is_none() {
            atoms.retain(|(u, _)| !self.units.defs[*u as usize].dim.is_none());
        } else if atoms.len() > 1
            && atoms
                .iter()
                .all(|(u, _)| self.units.defs[*u as usize].dim.is_none())
        {
            return NO_HINT;
        }
        if atoms.is_empty() {
            return NO_HINT;
        }
        if let Some(h) = self.hint_map.get(&atoms) {
            return *h;
        }
        let named: Vec<(String, i32)> = atoms
            .iter()
            .map(|(u, e)| (self.units.defs[*u as usize].name.clone(), *e))
            .collect();
        let scale = atoms.iter().fold(1.0, |s, (u, e)| {
            s * self.units.defs[*u as usize].scale.powi(*e)
        });
        let h = self.hints.len() as Hint;
        self.hints.push(HintDef {
            name: unit_name(&named),
            scale,
            atoms: atoms.clone(),
        });
        self.hint_map.insert(atoms, h);
        h
    }

    fn hint_of(&mut self, ue: &[(String, i32)], span: Span) -> Result<(f64, Dim, Hint)> {
        let (scale, dim) = self.units.eval(ue).map_err(|m| err(span, m))?;
        let mut atoms: Vec<(u32, i32)> = Vec::new();
        for (n, e) in ue {
            let u = self.units.lookup(n).unwrap() as u32;
            match atoms.iter_mut().find(|(a, _)| *a == u) {
                Some(a) => a.1 += e,
                None => atoms.push((u, *e)),
            }
        }
        Ok((scale, dim, self.intern_hint(atoms)))
    }

    /// The atoms a value's display unit is made of: empty for a plain
    /// number, `None` for a quantity with no display unit.
    fn atoms(&self, t: &Ty) -> Option<Vec<(u32, i32)>> {
        match t {
            Ty::Int => Some(vec![]),
            Ty::Num(d, h) => {
                if *h != NO_HINT {
                    Some(self.hints[*h as usize].atoms.clone())
                } else if d.is_none() {
                    Some(vec![])
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn combine_hint(&mut self, a: &Ty, b: &Ty, sign: i32) -> Hint {
        match (self.atoms(a), self.atoms(b)) {
            (Some(mut x), Some(y)) => {
                for (u, e) in y {
                    match x.iter_mut().find(|(a, _)| *a == u) {
                        Some(a) => a.1 += e * sign,
                        None => x.push((u, e * sign)),
                    }
                }
                self.intern_hint(x)
            }
            _ => NO_HINT,
        }
    }

    fn pow_hint(&mut self, t: &Ty, n: i32) -> Hint {
        match self.atoms(t) {
            Some(x) => self.intern_hint(x.into_iter().map(|(u, e)| (u, e * n)).collect()),
            None => NO_HINT,
        }
    }

    fn sqrt_hint(&mut self, t: &Ty) -> Hint {
        match self.atoms(t) {
            Some(x) if x.iter().all(|(_, e)| e % 2 == 0) => {
                self.intern_hint(x.into_iter().map(|(u, e)| (u, e / 2)).collect())
            }
            _ => NO_HINT,
        }
    }

    fn merge_hint(a: Hint, b: Hint) -> Hint {
        if a == NO_HINT { b } else { a }
    }

    // ---------------------------------------------------------------
    // Locals
    // ---------------------------------------------------------------

    fn bind(&mut self, name: &str, ty: Ty, mutable: bool) -> u32 {
        let slot = self.frame.nslots;
        self.frame.nslots += 1;
        self.frame.scopes.last_mut().unwrap().push(Local {
            name: name.to_string(),
            slot,
            ty,
            mutable,
        });
        slot
    }

    fn hidden_slot(&mut self) -> u32 {
        let slot = self.frame.nslots;
        self.frame.nslots += 1;
        slot
    }

    fn local(&self, name: &str) -> Option<&Local> {
        self.frame
            .scopes
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|l| l.name == name)
    }

    // ---------------------------------------------------------------
    // Coercion and unification
    // ---------------------------------------------------------------

    fn coerce(&self, e: &Expr, ex: Ex, got: &Ty, want: &Ty) -> Result<Ex> {
        if got == want {
            return Ok(ex);
        }
        match (got, want) {
            (Ty::Int, Ty::Num(d, _)) if d.is_none() => Ok(Ex::ToNum(Box::new(ex))),
            (Ty::Int | Ty::Num(..), Ty::Num(..)) if is_zero(e) => Ok(Ex::Const(Value::Num(0.0))),
            _ => Err(err(
                e.span,
                format!(
                    "expected {}, found {}",
                    self.ty_name(want),
                    self.ty_name(got)
                ),
            )),
        }
    }

    fn expr_as(&mut self, e: &Expr, want: &Ty) -> Result<Ex> {
        if let (ExprKind::Array(elems), Ty::Arr(elem)) = (&e.kind, want) {
            self.direct = false;
            let mut out = Vec::new();
            for x in elems {
                out.push(self.expr_as(x, elem)?);
            }
            return Ok(Ex::Array(out));
        }
        if let (ExprKind::If(c, t, Some(f)), _) = (&e.kind, want) {
            self.direct = false;
            let c = self.expr_as(c, &Ty::Bool)?;
            let t = self.expr_as(t, want)?;
            let f = self.expr_as(f, want)?;
            return Ok(Ex::If(Box::new(c), Box::new(t), Box::new(f)));
        }
        if let ExprKind::Block(b) = &e.kind {
            self.direct = false;
            return self.block(b, Some(want)).map(|(ex, _)| ex);
        }
        let (ex, got) = self.expr(e)?;
        self.coerce(e, ex, &got, want)
    }

    /// Two numeric operands of `+`, `-`, a comparison or `min`: the same
    /// dimension, with integers promoted to floats if either side is one.
    #[allow(clippy::too_many_arguments)]
    fn unify_num(
        &mut self,
        le: &Expr,
        l: Ex,
        lt: Ty,
        re: &Expr,
        r: Ex,
        rt: Ty,
        span: Span,
    ) -> Result<(Ex, Ex, Ty)> {
        match (&lt, &rt) {
            (Ty::Int, Ty::Int) => Ok((l, r, Ty::Int)),
            (Ty::Num(a, ha), Ty::Num(b, hb)) if a == b => {
                Ok((l, r, Ty::Num(*a, Self::merge_hint(*ha, *hb))))
            }
            (Ty::Int, Ty::Num(d, h)) if d.is_none() => {
                Ok((Ex::ToNum(Box::new(l)), r, Ty::Num(*d, *h)))
            }
            (Ty::Num(d, h), Ty::Int) if d.is_none() => {
                Ok((l, Ex::ToNum(Box::new(r)), Ty::Num(*d, *h)))
            }
            (Ty::Int | Ty::Num(..), Ty::Num(..)) if is_zero(le) => {
                Ok((Ex::Const(Value::Num(0.0)), r, rt.clone()))
            }
            (Ty::Num(..), Ty::Int | Ty::Num(..)) if is_zero(re) => {
                Ok((l, Ex::Const(Value::Num(0.0)), lt.clone()))
            }
            _ if lt.is_numeric() && rt.is_numeric() => Err(err(
                span,
                format!(
                    "units don't match: {} and {}",
                    self.ty_name(&lt),
                    self.ty_name(&rt)
                ),
            )),
            _ => Err(err(
                span,
                format!(
                    "expected numbers, found {} and {}",
                    self.ty_name(&lt),
                    self.ty_name(&rt)
                ),
            )),
        }
    }

    /// Two values that must have one type: branches, array elements.
    #[allow(clippy::too_many_arguments)]
    fn unify(
        &mut self,
        le: &Expr,
        l: Ex,
        lt: Ty,
        re: &Expr,
        r: Ex,
        rt: Ty,
        span: Span,
    ) -> Result<(Ex, Ex, Ty)> {
        if lt.is_numeric() && rt.is_numeric() {
            return self.unify_num(le, l, lt, re, r, rt, span);
        }
        if lt == rt {
            return Ok((l, r, lt));
        }
        Err(err(
            span,
            format!(
                "these have different types: {} and {}",
                self.ty_name(&lt),
                self.ty_name(&rt)
            ),
        ))
    }

    /// The common type of several values (integers promote to floats, a
    /// literal 0 takes any unit), each coerced to it. With `numeric`, the
    /// result is a float or quantity.
    fn joined(&mut self, es: &[&Expr], numeric: bool) -> Result<(Vec<Ex>, Ty)> {
        let mut xs = Vec::new();
        let mut tys = Vec::new();
        for e in es {
            let (x, t) = self.expr(e)?;
            xs.push(x);
            tys.push(t);
        }
        let mut ty = tys[0].clone();
        let mut zero = is_zero(es[0]);
        for (e, t) in es.iter().zip(&tys).skip(1) {
            ty = self.join_ty(&ty, zero, t, is_zero(e), e.span)?;
            zero = zero && is_zero(e);
        }
        if numeric {
            ty = match ty {
                Ty::Int => Ty::FLOAT,
                Ty::Num(..) => ty,
                t => {
                    return Err(err(
                        es[0].span,
                        format!("expected numbers, found {}", self.ty_name(&t)),
                    ));
                }
            };
        }
        for i in 0..xs.len() {
            if tys[i] != ty {
                xs[i] = if matches!(es[i].kind, ExprKind::Array(_)) {
                    self.expr_as(es[i], &ty)?
                } else {
                    let x = std::mem::replace(&mut xs[i], Ex::Const(Value::Unit));
                    self.coerce(es[i], x, &tys[i], &ty)?
                };
            }
        }
        Ok((xs, ty))
    }

    fn join_ty(&self, a: &Ty, a_zero: bool, b: &Ty, b_zero: bool, span: Span) -> Result<Ty> {
        let mismatch = || {
            err(
                span,
                format!(
                    "these have different types: {} and {}",
                    self.ty_name(a),
                    self.ty_name(b)
                ),
            )
        };
        Ok(match (a, b) {
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Num(x, hx), Ty::Num(y, hy)) if x == y => Ty::Num(*x, Self::merge_hint(*hx, *hy)),
            (Ty::Int, Ty::Num(d, _)) if d.is_none() || a_zero => b.clone(),
            (Ty::Num(d, _), Ty::Int) if d.is_none() || b_zero => a.clone(),
            (Ty::Num(..), Ty::Num(..)) if a_zero => b.clone(),
            (Ty::Num(..), Ty::Num(..)) if b_zero => a.clone(),
            (Ty::Arr(x), Ty::Arr(y)) => Ty::Arr(Box::new(self.join_ty(x, false, y, false, span)?)),
            _ if a == b => a.clone(),
            (Ty::Int | Ty::Num(..), Ty::Int | Ty::Num(..)) => {
                return Err(err(
                    span,
                    format!(
                        "units don't match: {} and {}",
                        self.ty_name(a),
                        self.ty_name(b)
                    ),
                ));
            }
            _ => return Err(mismatch()),
        })
    }

    fn num_operand(&self, e: &Expr, ex: Ex, t: &Ty) -> Result<(Ex, Ty)> {
        match t {
            Ty::Int => Ok((Ex::ToNum(Box::new(ex)), Ty::FLOAT)),
            Ty::Num(..) => Ok((ex, t.clone())),
            _ => Err(err(
                e.span,
                format!("expected a number, found {}", self.ty_name(t)),
            )),
        }
    }

    // ---------------------------------------------------------------
    // Expressions
    // ---------------------------------------------------------------

    pub fn expr(&mut self, e: &Expr) -> Result<(Ex, Ty)> {
        let direct = std::mem::replace(&mut self.direct, false);
        let line = e.span.line;
        match &e.kind {
            ExprKind::Int(n) => Ok((Ex::Const(Value::Int(*n)), Ty::Int)),
            ExprKind::Float(x) => Ok((Ex::Const(Value::Num(*x)), Ty::FLOAT)),
            ExprKind::Quantity(v, ue) => {
                let (scale, dim, hint) = self.hint_of(ue, e.span)?;
                let x = v * scale;
                if !x.is_finite() {
                    return Err(err(e.span, "this quantity is too large"));
                }
                Ok((Ex::Const(Value::Num(x)), Ty::Num(dim, hint)))
            }
            ExprKind::Str(s) => Ok((Ex::Const(Value::Str(std::rc::Rc::new(s.clone()))), Ty::Str)),
            ExprKind::Bool(b) => Ok((Ex::Const(Value::Bool(*b)), Ty::Bool)),
            ExprKind::Name(n) => self.name(n, e.span, direct),
            ExprKind::Field(base, f) => self.field(base, f, e.span, direct),
            ExprKind::Call(callee, args) => self.call(callee, args, e.span, direct),
            ExprKind::Index(a, i) => {
                let (ax, at) = self.expr(a)?;
                let Ty::Arr(elem) = at else {
                    return Err(err(
                        a.span,
                        format!("only arrays can be indexed, not {}", self.ty_name(&at)),
                    ));
                };
                let ix = self.expr_as(i, &Ty::Int)?;
                Ok((Ex::Index(Box::new(ax), Box::new(ix), line), *elem))
            }
            ExprKind::Neg(x) => {
                let (ex, t) = self.expr(x)?;
                if !t.is_numeric() {
                    return Err(err(e.span, format!("can't negate {}", self.ty_name(&t))));
                }
                Ok((Ex::Neg(Box::new(ex), line), t))
            }
            ExprKind::Not(x) => {
                let ex = self.expr_as(x, &Ty::Bool)?;
                Ok((Ex::Not(Box::new(ex)), Ty::Bool))
            }
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, e.span),
            ExprKind::Convert(x, ue) => {
                let (ex, t) = self.expr(x)?;
                let (scale, dim, _) = self.hint_of(ue, e.span)?;
                let (ex, t) = self.num_operand(x, ex, &t)?;
                let Ty::Num(d, _) = t else { unreachable!() };
                if d != dim {
                    return Err(err(
                        e.span,
                        format!("can't express {} in {}", self.ty_name(&t), unit_name(ue)),
                    ));
                }
                Ok((
                    Ex::Arith(
                        ArOp::Div,
                        Box::new(ex),
                        Box::new(Ex::Const(Value::Num(scale))),
                        line,
                    ),
                    Ty::FLOAT,
                ))
            }
            ExprKind::Record(name, fields) => self.record(name, fields, e.span),
            ExprKind::Array(elems) => {
                if elems.is_empty() {
                    return Err(err(
                        e.span,
                        "an empty array needs a type: annotate it, like `let xs: [Int] = []`",
                    ));
                }
                let refs: Vec<&Expr> = elems.iter().collect();
                let (xs, ty) = self.joined(&refs, false)?;
                Ok((Ex::Array(xs), Ty::Arr(Box::new(ty))))
            }
            ExprKind::Comp {
                body,
                var,
                iter,
                filter,
            } => {
                let (it, ety) = self.iter(iter)?;
                self.frame.scopes.push(Vec::new());
                let slot = self.bind(var, ety, false);
                let filter = match filter {
                    Some(f) => Some(Box::new(self.expr_as(f, &Ty::Bool)?)),
                    None => None,
                };
                let r = self.expr(body);
                self.frame.scopes.pop();
                let (bx, bt) = r?;
                if bt == Ty::Unit {
                    return Err(err(body.span, "this comprehension produces nothing"));
                }
                Ok((
                    Ex::Comp {
                        slot,
                        iter: Box::new(it),
                        filter,
                        body: Box::new(bx),
                        line,
                    },
                    Ty::Arr(Box::new(bt)),
                ))
            }
            ExprKind::If(c, t, f) => {
                let cx = self.expr_as(c, &Ty::Bool)?;
                let (tx, tt) = self.expr(t)?;
                match f {
                    None => Ok((
                        Ex::If(Box::new(cx), Box::new(tx), Box::new(Ex::Const(Value::Unit))),
                        Ty::Unit,
                    )),
                    Some(f) => {
                        let (fx, ft) = self.expr(f)?;
                        if tt == Ty::Unit || ft == Ty::Unit {
                            return Ok((
                                Ex::If(Box::new(cx), Box::new(tx), Box::new(fx)),
                                Ty::Unit,
                            ));
                        }
                        let (tx, fx, ty) = self.unify(t, tx, tt, f, fx, ft, e.span)?;
                        Ok((Ex::If(Box::new(cx), Box::new(tx), Box::new(fx)), ty))
                    }
                }
            }
            ExprKind::Match(scrut, arms) => self.match_expr(scrut, arms, e.span),
            ExprKind::Block(b) => self.block(b, None),
        }
    }

    fn name(&mut self, n: &str, span: Span, direct: bool) -> Result<(Ex, Ty)> {
        if let Ctx::Metric { model, row } = self.frame.ctx {
            if row {
                let outcome = self.models[model as usize].outcome().clone();
                if n == "outcome" {
                    return Ok((Ex::Local(0), outcome));
                }
                if let Ty::Rec(r) = &outcome
                    && let Some(i) = self.records[*r as usize]
                        .fields
                        .iter()
                        .position(|(f, _)| f == n)
                {
                    let t = self.records[*r as usize].fields[i].1.clone();
                    return Ok((Ex::Field(Box::new(Ex::Local(0)), i as u32), t));
                }
            } else if self.local(n).is_none() && !self.global_map.contains_key(n) {
                let outcome = self.models[model as usize].outcome();
                let is_field = n == "outcome"
                    || matches!(outcome, Ty::Rec(r) if self.records[*r as usize].fields.iter().any(|(f, _)| f == n));
                if is_field {
                    return Err(err(
                        span,
                        format!(
                            "`{n}` differs from world to world: summarise it with a statistic, \
                             like mean({n}) or probability({n} > x)"
                        ),
                    ));
                }
            }
        }
        if let Some(l) = self.local(n) {
            return Ok((Ex::Local(l.slot), l.ty.clone()));
        }
        if self.frame.ctx == Ctx::Fact
            && let Some(w) = self.frame.world.clone()
            && let Some(&f) = self.world_map[&w].get(n)
        {
            return self.fact_read(f, Vec::new(), span, direct);
        }
        if let Some(&g) = self.global_map.get(n) {
            let ty = self.global(g)?;
            let kind = self.globals[g as usize].as_ref().unwrap().kind;
            if self.frame.ctx == Ctx::Const && kind != GlobalKind::Const {
                return Err(err(
                    span,
                    format!("a `const` can't use `{n}`, which depends on inputs: use `derived`"),
                ));
            }
            self.frame.globals_read.insert(g);
            return Ok((Ex::Global(g), ty));
        }
        if let Some(vs) = self.variant_names.get(n) {
            if vs.len() > 1 {
                return Err(err(
                    span,
                    format!("`{n}` is a variant of several enums: write `Enum.{n}`"),
                ));
            }
            let (e, tag) = vs[0];
            if !self.enums[e as usize].variants[tag as usize]
                .fields
                .is_empty()
            {
                return Err(err(
                    span,
                    format!("variant `{n}` needs its fields: `{n}(...)`"),
                ));
            }
            return Ok((Ex::Variant(tag, Vec::new()), Ty::Enum(e)));
        }
        if n == "pi" {
            return Ok((Ex::Const(Value::Num(std::f64::consts::PI)), Ty::FLOAT));
        }
        if self.world_map.contains_key(n) {
            return Err(err(
                span,
                format!("read a fact of world `{n}` as `{n}.fact`"),
            ));
        }
        if self.model_map.contains_key(n) {
            return Err(err(span, format!("`{n}` is a model, not a value")));
        }
        Err(err(span, format!("unknown name `{n}`")))
    }

    fn can_read_world(&self) -> bool {
        matches!(
            self.frame.ctx,
            Ctx::Fact | Ctx::Model | Ctx::Observe | Ctx::Policy { oracle: true, .. }
        )
    }

    fn fact_read(&mut self, f: u32, args: Vec<Ex>, span: Span, direct: bool) -> Result<(Ex, Ty)> {
        let ty = self.fact(f, span)?;
        let (world, item) = self.fact_decls[f as usize];
        let full = format!("{world}.{}", item.name);
        if !self.can_read_world() {
            let why = match self.frame.ctx {
                Ctx::Policy { .. } => {
                    "a policy can't read the world: it sees only its observation and inputs. Use \
                     forecast(...) to look ahead, or declare an `oracle policy`"
                }
                Ctx::Belief => "`belief` sees only the observation, not the world",
                Ctx::Fn => "functions are pure: pass world facts in as arguments from the model",
                Ctx::Metric { .. } => "study metrics see only outcomes",
                _ => "world facts are read only by models, oracle policies and other facts",
            };
            return Err(err(span, format!("can't read `{full}` here: {why}")));
        }
        if self.frame.ctx == Ctx::Observe {
            if item.kind == FactKind::Latent {
                return Err(err(
                    span,
                    format!("`{full}` is latent: it can never be observed directly"),
                ));
            }
            if !direct {
                return Err(err(
                    span,
                    format!(
                        "`observe` must reveal a world fact whole, as the value of an \
                         observation field (like `drought: {full}(t)`): a policy's forecasts \
                         hold what it has observed fixed, so partly revealing one would leak it"
                    ),
                ));
            }
        }
        let params = self.facts[f as usize].as_ref().unwrap().params.clone();
        if params.len() != args.len() {
            return Err(err(
                span,
                format!(
                    "`{full}` takes {} argument{}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        Ok((
            Ex::Fact {
                fact: f,
                args,
                line: span.line,
            },
            ty,
        ))
    }

    fn field(&mut self, base: &Expr, f: &str, span: Span, direct: bool) -> Result<(Ex, Ty)> {
        if let ExprKind::Name(n) = &base.kind
            && self.local(n).is_none()
        {
            if let Some(facts) = self.world_map.get(n) {
                let Some(&id) = facts.get(f) else {
                    return Err(err(span, format!("world `{n}` has no fact `{f}`")));
                };
                return self.fact_read(id, Vec::new(), span, direct);
            }
            if let Some(TypeName::Enum(e)) = self.type_names.get(n) {
                let e = *e;
                let Some(tag) = self.enums[e as usize]
                    .variants
                    .iter()
                    .position(|v| v.name == f)
                else {
                    return Err(err(span, format!("enum `{n}` has no variant `{f}`")));
                };
                if !self.enums[e as usize].variants[tag].fields.is_empty() {
                    return Err(err(
                        span,
                        format!("variant `{f}` needs its fields: `{n}.{f}(...)`"),
                    ));
                }
                return Ok((Ex::Variant(tag as u32, Vec::new()), Ty::Enum(e)));
            }
        }
        let (bx, bt) = self.expr(base)?;
        match &bt {
            Ty::Rec(r) => {
                let rec = &self.records[*r as usize];
                let Some(i) = rec.fields.iter().position(|(n, _)| n == f) else {
                    return Err(err(span, format!("`{}` has no field `{f}`", rec.name)));
                };
                let t = rec.fields[i].1.clone();
                Ok((Ex::Field(Box::new(bx), i as u32), t))
            }
            _ => Err(err(span, format!("{} has no fields", self.ty_name(&bt)))),
        }
    }

    fn record(&mut self, name: &str, fields: &[(String, Expr)], span: Span) -> Result<(Ex, Ty)> {
        let r = match self.type_names.get(name) {
            Some(TypeName::Rec(r)) => *r,
            Some(TypeName::Alias(Ty::Rec(r))) => *r,
            _ => return Err(err(span, format!("unknown record type `{name}`"))),
        };
        let defs = self.records[r as usize].fields.clone();
        let mut slots: Vec<Option<Ex>> = vec![None; defs.len()];
        for (f, value) in fields {
            let Some(i) = defs.iter().position(|(n, _)| n == f) else {
                return Err(err(value.span, format!("`{name}` has no field `{f}`")));
            };
            if slots[i].is_some() {
                return Err(err(value.span, format!("field `{f}` is given twice")));
            }
            self.direct = self.frame.ctx == Ctx::Observe;
            slots[i] = Some(self.expr_as(value, &defs[i].1)?);
        }
        let missing: Vec<&str> = defs
            .iter()
            .zip(&slots)
            .filter(|(_, s)| s.is_none())
            .map(|((n, _), _)| n.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(err(
                span,
                format!(
                    "`{name}` is missing field{} {}",
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", ")
                ),
            ));
        }
        Ok((
            Ex::Record(slots.into_iter().map(Option::unwrap).collect()),
            Ty::Rec(r),
        ))
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> Result<(Ex, Ty)> {
        let line = span.line;
        match op {
            BinOp::And | BinOp::Or => {
                let lx = self.expr_as(l, &Ty::Bool)?;
                let rx = self.expr_as(r, &Ty::Bool)?;
                let ex = if op == BinOp::And {
                    Ex::And(Box::new(lx), Box::new(rx))
                } else {
                    Ex::Or(Box::new(lx), Box::new(rx))
                };
                Ok((ex, Ty::Bool))
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                let (lx, lt) = self.expr(l)?;
                let (rx, rt) = self.expr(r)?;
                let ordered = !matches!(op, BinOp::Eq | BinOp::Ne);
                let (lx, rx) = if lt.is_numeric() || rt.is_numeric() {
                    let (lx, rx, _) = self.unify_num(l, lx, lt, r, rx, rt, span)?;
                    (lx, rx)
                } else {
                    if lt != rt {
                        return Err(err(
                            span,
                            format!(
                                "can't compare {} with {}",
                                self.ty_name(&lt),
                                self.ty_name(&rt)
                            ),
                        ));
                    }
                    if ordered {
                        return Err(err(
                            span,
                            format!("{} values have no order", self.ty_name(&lt)),
                        ));
                    }
                    (lx, rx)
                };
                let op = match op {
                    BinOp::Eq => CmpOp::Eq,
                    BinOp::Ne => CmpOp::Ne,
                    BinOp::Lt => CmpOp::Lt,
                    BinOp::Gt => CmpOp::Gt,
                    BinOp::Le => CmpOp::Le,
                    _ => CmpOp::Ge,
                };
                Ok((Ex::Cmp(op, Box::new(lx), Box::new(rx)), Ty::Bool))
            }
            BinOp::Add | BinOp::Sub => {
                let (lx, lt) = self.expr(l)?;
                let (rx, rt) = self.expr(r)?;
                let (lx, rx, t) = self.unify_num(l, lx, lt, r, rx, rt, span)?;
                let op = if op == BinOp::Add {
                    ArOp::Add
                } else {
                    ArOp::Sub
                };
                Ok((Ex::Arith(op, Box::new(lx), Box::new(rx), line), t))
            }
            BinOp::Mul | BinOp::Div => {
                let (lx, lt) = self.expr(l)?;
                let (rx, rt) = self.expr(r)?;
                if op == BinOp::Mul && lt == Ty::Int && rt == Ty::Int {
                    return Ok((
                        Ex::Arith(ArOp::Mul, Box::new(lx), Box::new(rx), line),
                        Ty::Int,
                    ));
                }
                let (lx, lnt) = self.num_operand(l, lx, &lt)?;
                let (rx, rnt) = self.num_operand(r, rx, &rt)?;
                let (Ty::Num(ld, _), Ty::Num(rd, _)) = (&lnt, &rnt) else {
                    unreachable!()
                };
                let (dim, sign, aop) = if op == BinOp::Mul {
                    (ld.times(*rd), 1, ArOp::Mul)
                } else {
                    (ld.per(*rd), -1, ArOp::Div)
                };
                let hint = self.combine_hint(&lnt, &rnt, sign);
                Ok((
                    Ex::Arith(aop, Box::new(lx), Box::new(rx), line),
                    Ty::Num(dim, hint),
                ))
            }
            BinOp::IDiv => {
                let lx = self.expr_as(l, &Ty::Int).map_err(|_| {
                    err(
                        span,
                        "`//` divides integers, rounding down: use `/` for floats",
                    )
                })?;
                let rx = self.expr_as(r, &Ty::Int).map_err(|_| {
                    err(
                        span,
                        "`//` divides integers, rounding down: use `/` for floats",
                    )
                })?;
                Ok((
                    Ex::Arith(ArOp::IDiv, Box::new(lx), Box::new(rx), line),
                    Ty::Int,
                ))
            }
            BinOp::Pow => {
                let (bx, bt) = self.expr(l)?;
                if let Some(n) = int_literal(r) {
                    if n.abs() > 64 {
                        return Err(err(r.span, "exponent too large"));
                    }
                    match &bt {
                        Ty::Int if n >= 0 => {
                            return Ok((
                                Ex::Arith(
                                    ArOp::Pow,
                                    Box::new(bx),
                                    Box::new(Ex::Const(Value::Int(n))),
                                    line,
                                ),
                                Ty::Int,
                            ));
                        }
                        Ty::Int | Ty::Num(..) => {
                            let (bx, bnt) = self.num_operand(l, bx, &bt)?;
                            let Ty::Num(d, _) = bnt else { unreachable!() };
                            let hint = self.pow_hint(&bnt, n as i32);
                            return Ok((
                                Ex::Arith(
                                    ArOp::Pow,
                                    Box::new(bx),
                                    Box::new(Ex::Const(Value::Int(n))),
                                    line,
                                ),
                                Ty::Num(d.pow(n as i32), hint),
                            ));
                        }
                        _ => {}
                    }
                }
                let (bx, bnt) = self.num_operand(l, bx, &bt)?;
                let (ex, et) = self.expr(r)?;
                let (ex, ent) = self.num_operand(r, ex, &et)?;
                if bnt != Ty::FLOAT || ent != Ty::FLOAT {
                    return Err(err(
                        span,
                        "a quantity's power must be a whole number written in the source, like `x^2`",
                    ));
                }
                Ok((Ex::Builtin(Bi::Pow, vec![bx, ex], line), Ty::FLOAT))
            }
        }
    }

    fn iter(&mut self, it: &ast::Iter) -> Result<(Iter, Ty)> {
        match &it.kind {
            IterKind::Over(e) => {
                let (ex, t) = self.expr(e)?;
                match t {
                    Ty::Arr(elem) => Ok((Iter::Over(ex), *elem)),
                    _ => Err(err(
                        e.span,
                        format!(
                            "can only loop over a range `a..b` or an array, not {}",
                            self.ty_name(&t)
                        ),
                    )),
                }
            }
            IterKind::Range {
                lo,
                hi,
                inclusive,
                step,
            } => {
                let (lx, lt) = self.expr(lo)?;
                let (hx, ht) = self.expr(hi)?;
                let (lx, hx, t) = self.unify_num(lo, lx, lt, hi, hx, ht, it.span)?;
                let num = t != Ty::Int;
                let step = match step {
                    Some(s) => Some(self.expr_as(s, &t)?),
                    None if num => {
                        return Err(err(
                            it.span,
                            "a range of floats or quantities needs a `step`",
                        ));
                    }
                    None => None,
                };
                Ok((
                    Iter::Range {
                        lo: lx,
                        hi: hx,
                        inclusive: *inclusive,
                        step,
                        num,
                    },
                    t,
                ))
            }
        }
    }

    fn block(&mut self, b: &ast::Block, want: Option<&Ty>) -> Result<(Ex, Ty)> {
        self.frame.scopes.push(Vec::new());
        let r = self.block_inner(b, want);
        self.frame.scopes.pop();
        r
    }

    fn block_inner(&mut self, b: &ast::Block, want: Option<&Ty>) -> Result<(Ex, Ty)> {
        let mut stmts = Vec::new();
        for s in &b.stmts {
            self.stmt(s, &mut stmts)?;
        }
        let (tail, ty) = match (&b.tail, want) {
            (Some(t), Some(w)) => (self.expr_as(t, w)?, w.clone()),
            (Some(t), None) => self.expr(t)?,
            (None, Some(w)) if *w != Ty::Unit => {
                return Err(err(
                    b.stmts
                        .last()
                        .map(|s| match s {
                            Stmt::Let { span, .. }
                            | Stmt::Assign { span, .. }
                            | Stmt::For { span, .. }
                            | Stmt::Iterate { span, .. }
                            | Stmt::Assert { span, .. }
                            | Stmt::Note { span, .. } => *span,
                            Stmt::Expr(e) => e.span,
                        })
                        .unwrap_or_default(),
                    format!(
                        "this block must end with a value of type {}",
                        self.ty_name(w)
                    ),
                ));
            }
            (None, _) => (Ex::Const(Value::Unit), Ty::Unit),
        };
        if stmts.is_empty() {
            return Ok((tail, ty));
        }
        Ok((Ex::Block(stmts, Box::new(tail)), ty))
    }

    fn stmt(&mut self, s: &Stmt, out: &mut Vec<St>) -> Result<()> {
        match s {
            Stmt::Let {
                name,
                mutable,
                ty,
                value,
                ..
            } => {
                let (ex, t) = match ty {
                    Some(t) => {
                        let want = self.ty(t)?;
                        (self.expr_as(value, &want)?, want)
                    }
                    None => self.expr(value)?,
                };
                if t == Ty::Unit {
                    return Err(err(value.span, "this has no value to bind"));
                }
                let slot = self.bind(name, t, *mutable);
                out.push(St::Let(slot, ex));
            }
            Stmt::Assign {
                target,
                value,
                span,
            } => {
                let mut path_rev = Vec::new();
                let mut cur = target;
                loop {
                    match &cur.kind {
                        ExprKind::Name(_) => break,
                        ExprKind::Field(b, f) => {
                            path_rev.push(Ok(f.clone()));
                            cur = b;
                        }
                        ExprKind::Index(b, i) => {
                            path_rev.push(Err(&**i));
                            cur = b;
                        }
                        _ => {
                            return Err(err(
                                *span,
                                "can only assign to a variable, a field or an element",
                            ));
                        }
                    }
                }
                let ExprKind::Name(root) = &cur.kind else {
                    unreachable!()
                };
                let Some(l) = self.local(root) else {
                    let why = if self.global_map.contains_key(root) {
                        "inputs, constants and derived values are immutable"
                    } else {
                        "it isn't a local variable"
                    };
                    return Err(err(*span, format!("can't assign to `{root}`: {why}")));
                };
                if !l.mutable {
                    return Err(err(
                        *span,
                        format!(
                            "`{root}` is immutable: declare it with `var` (copy a parameter with \
                             `var {root} = {root}`)"
                        ),
                    ));
                }
                let (slot, mut ty) = (l.slot, l.ty.clone());
                let mut path = Vec::new();
                for seg in path_rev.into_iter().rev() {
                    match seg {
                        Ok(f) => {
                            let Ty::Rec(r) = &ty else {
                                return Err(err(
                                    *span,
                                    format!("{} has no field `{f}`", self.ty_name(&ty)),
                                ));
                            };
                            let rec = &self.records[*r as usize];
                            let Some(i) = rec.fields.iter().position(|(n, _)| *n == f) else {
                                return Err(err(
                                    *span,
                                    format!("`{}` has no field `{f}`", rec.name),
                                ));
                            };
                            let t = rec.fields[i].1.clone();
                            path.push(PathSeg::Field(i as u32));
                            ty = t;
                        }
                        Err(ie) => {
                            let Ty::Arr(elem) = ty else {
                                return Err(err(*span, "only arrays can be indexed"));
                            };
                            let ix = self.expr_as(ie, &Ty::Int)?;
                            path.push(PathSeg::Index(ix, ie.span.line));
                            ty = *elem;
                        }
                    }
                }
                let vx = self.expr_as(value, &ty)?;
                out.push(St::Set(slot, path, vx));
            }
            Stmt::For {
                var,
                iter,
                cond,
                body,
                span,
            } => {
                let (it, ety) = self.iter(iter)?;
                if let Iter::Range { num: true, .. } = it {
                    return Err(err(
                        iter.span,
                        "a `for` loop counts in integers: loop over `0..n` and compute the value",
                    ));
                }
                self.frame.scopes.push(Vec::new());
                let slot = self.bind(var, ety, false);
                let r = (|| {
                    let cond = match cond {
                        Some(c) => Some(self.expr_as(c, &Ty::Bool)?),
                        None => None,
                    };
                    let mut stmts = Vec::new();
                    for s in &body.stmts {
                        self.stmt(s, &mut stmts)?;
                    }
                    if let Some(t) = &body.tail {
                        let (ex, _) = self.expr(t)?;
                        stmts.push(St::Expr(ex));
                    }
                    Ok((cond, stmts))
                })();
                self.frame.scopes.pop();
                let (cond, stmts) = r?;
                out.push(St::For {
                    slot,
                    iter: it,
                    cond,
                    body: stmts,
                    line: span.line,
                });
            }
            Stmt::Iterate { count, body, span } => {
                let n = self.expr_as(count, &Ty::Int)?;
                let slot = self.hidden_slot();
                self.frame.scopes.push(Vec::new());
                let r = (|| {
                    let mut stmts = Vec::new();
                    for s in &body.stmts {
                        self.stmt(s, &mut stmts)?;
                    }
                    if let Some(t) = &body.tail {
                        let (ex, _) = self.expr(t)?;
                        stmts.push(St::Expr(ex));
                    }
                    Ok(stmts)
                })();
                self.frame.scopes.pop();
                out.push(St::For {
                    slot,
                    iter: Iter::Range {
                        lo: Ex::Const(Value::Int(0)),
                        hi: n,
                        inclusive: false,
                        step: None,
                        num: false,
                    },
                    cond: None,
                    body: r?,
                    line: span.line,
                });
            }
            Stmt::Assert { cond, msg, span } => {
                let cx = self.expr_as(cond, &Ty::Bool)?;
                let msg = msg
                    .clone()
                    .unwrap_or_else(|| format!("assertion failed: {}", self.label(cond.span)));
                out.push(St::Assert(cx, msg, span.line));
            }
            Stmt::Note { name, value, span } => {
                if !matches!(self.frame.ctx, Ctx::Policy { .. }) {
                    return Err(err(
                        *span,
                        "`note` gives a policy's reasons: use it in a policy's body",
                    ));
                }
                let (ex, ty) = self.expr(value)?;
                if ty == Ty::Unit {
                    return Err(err(value.span, format!("note `{name}` has no value")));
                }
                out.push(St::Note(name.as_str().into(), ty, ex));
            }
            Stmt::Expr(e) => {
                let (ex, _) = self.expr(e)?;
                out.push(St::Expr(ex));
            }
        }
        Ok(())
    }

    fn match_expr(&mut self, scrut: &Expr, arms: &[ast::Arm], span: Span) -> Result<(Ex, Ty)> {
        let (sx, st) = self.expr(scrut)?;
        let Ty::Enum(e) = st else {
            return Err(err(
                scrut.span,
                format!("`match` needs an enum value, not {}", self.ty_name(&st)),
            ));
        };
        let def = self.enums[e as usize].clone();
        let mut covered = vec![false; def.variants.len()];
        let mut wild = false;
        let mut out: Vec<(Arm, Ty, &Expr)> = Vec::new();
        for arm in arms {
            if wild {
                return Err(err(
                    arm.span,
                    "this arm is unreachable: `_` already matched",
                ));
            }
            self.frame.scopes.push(Vec::new());
            let r = (|| {
                let (tag, binds) = match &arm.pat {
                    ast::Pattern::Wild => {
                        wild = true;
                        (None, Vec::new())
                    }
                    ast::Pattern::Variant { qual, name, binds } => {
                        if let Some(q) = qual
                            && *q != def.name
                        {
                            return Err(err(
                                arm.span,
                                format!("expected a variant of `{}`", def.name),
                            ));
                        }
                        let Some(tag) = def.variants.iter().position(|v| v.name == *name) else {
                            return Err(err(
                                arm.span,
                                format!("`{}` has no variant `{name}`", def.name),
                            ));
                        };
                        let fields = &def.variants[tag].fields;
                        if !binds.is_empty() && binds.len() != fields.len() {
                            return Err(err(
                                arm.span,
                                format!(
                                    "`{name}` has {} field{}",
                                    fields.len(),
                                    if fields.len() == 1 { "" } else { "s" }
                                ),
                            ));
                        }
                        if covered[tag] {
                            return Err(err(arm.span, format!("`{name}` is matched twice")));
                        }
                        covered[tag] = true;
                        let mut slots = Vec::new();
                        for (b, (_, t)) in binds.iter().zip(fields) {
                            slots.push(self.bind(b, t.clone(), false));
                        }
                        (Some(tag as u32), slots)
                    }
                };
                let (bx, bt) = self.expr(&arm.body)?;
                Ok((
                    Arm {
                        tag,
                        binds,
                        body: bx,
                    },
                    bt,
                ))
            })();
            self.frame.scopes.pop();
            let (a, t) = r?;
            out.push((a, t, &arm.body));
        }
        if !wild && covered.iter().any(|c| !c) {
            let missing: Vec<&str> = def
                .variants
                .iter()
                .zip(&covered)
                .filter(|(_, c)| !**c)
                .map(|(v, _)| v.name.as_str())
                .collect();
            return Err(err(
                span,
                format!("`match` doesn't cover {}", missing.join(", ")),
            ));
        }
        // Unify the arms' types.
        let mut ty = out[0].1.clone();
        for (_, t, _) in &out[1..] {
            if *t == ty {
                continue;
            }
            if ty == Ty::Int && matches!(t, Ty::Num(d, _) if d.is_none()) {
                ty = t.clone();
            } else if !(matches!(ty, Ty::Num(d, _) if d.is_none()) && *t == Ty::Int) {
                return Err(err(
                    span,
                    format!(
                        "the arms have different types: {} and {}",
                        self.ty_name(&ty),
                        self.ty_name(t)
                    ),
                ));
            }
        }
        let mut arms_out = Vec::new();
        for (mut a, t, body) in out {
            a.body = self.coerce(body, a.body, &t, &ty)?;
            arms_out.push(a);
        }
        Ok((Ex::Match(Box::new(sx), arms_out, span.line), ty))
    }

    // ---------------------------------------------------------------
    // Calls
    // ---------------------------------------------------------------

    fn call(
        &mut self,
        callee: &Expr,
        args: &[ast::Arg],
        span: Span,
        direct: bool,
    ) -> Result<(Ex, Ty)> {
        let line = span.line;
        // World.fact(args) and Enum.variant(args)
        if let ExprKind::Field(base, f) = &callee.kind {
            if let ExprKind::Name(n) = &base.kind
                && self.local(n).is_none()
            {
                if let Some(facts) = self.world_map.get(n) {
                    let Some(&id) = facts.get(f) else {
                        return Err(err(span, format!("world `{n}` has no fact `{f}`")));
                    };
                    let xs = self.fact_args(id, args, span)?;
                    return self.fact_read(id, xs, span, direct);
                }
                if let Some(TypeName::Enum(e)) = self.type_names.get(n) {
                    let e = *e;
                    let Some(tag) = self.enums[e as usize]
                        .variants
                        .iter()
                        .position(|v| v.name == *f)
                    else {
                        return Err(err(span, format!("enum `{n}` has no variant `{f}`")));
                    };
                    return self.variant_call(e, tag as u32, args, span);
                }
            }
            return Err(err(
                callee.span,
                "only functions, facts and variants can be called",
            ));
        }
        let ExprKind::Name(name) = &callee.kind else {
            return Err(err(callee.span, "only functions can be called"));
        };
        if self.local(name).is_some() {
            return Err(err(
                callee.span,
                format!("`{name}` is a value, not a function"),
            ));
        }
        if self.frame.ctx == Ctx::Fact
            && let Some(w) = self.frame.world.clone()
            && let Some(&f) = self.world_map[&w].get(name)
        {
            let xs = self.fact_args(f, args, span)?;
            return self.fact_read(f, xs, span, direct);
        }
        for a in args {
            if a.filter.is_some() && !matches!(self.frame.ctx, Ctx::Metric { row: false, .. }) {
                return Err(err(
                    a.value.span,
                    "`where` filters a statistic in a study metric",
                ));
            }
        }
        if let Some(&f) = self.fn_map.get(name) {
            self.no_named(args, name)?;
            self.check_fn(f, span)?;
            self.frame.fns_called.insert(f);
            let def = self.fns[f as usize].as_ref().unwrap();
            let params = def.params.clone();
            let ret = def.ret.clone();
            if params.len() != args.len() {
                return Err(err(
                    span,
                    format!(
                        "`{name}` takes {} argument{}",
                        params.len(),
                        if params.len() == 1 { "" } else { "s" }
                    ),
                ));
            }
            let mut xs = Vec::new();
            for (a, p) in args.iter().zip(&params) {
                xs.push(self.expr_as(&a.value, p)?);
            }
            return Ok((Ex::Call(f, xs), ret));
        }
        if let Some(vs) = self.variant_names.get(name).cloned() {
            if vs.len() > 1 {
                return Err(err(
                    span,
                    format!("`{name}` is a variant of several enums: write `Enum.{name}(...)`"),
                ));
            }
            return self.variant_call(vs[0].0, vs[0].1, args, span);
        }
        if name == "forecast" || name == "rollout" {
            return self.forecast(name == "forecast", args, span);
        }
        if let Ctx::Metric { row: false, .. } = self.frame.ctx
            && let Some((kind, q)) = stat_name(name)
            && !(matches!(kind, StatKind::Min | StatKind::Max) && args.len() == 2)
        {
            return self.world_stat(name, kind, q, args, span);
        }
        if DISTRIBUTIONS.contains(&name.as_str()) {
            return Err(err(
                span,
                format!(
                    "`{name}` is a distribution: uncertainty enters only in a `world`, as \
                     `uncertain name ~ {name}(...)`"
                ),
            ));
        }
        if self.model_map.contains_key(name) {
            return Err(err(
                span,
                format!(
                    "models can't be called: a study runs `{name}`, policies look ahead with \
                     forecast(...), and oracle policies with rollout(...)"
                ),
            ));
        }
        self.no_named(args, name)?;
        self.builtin(name, args, span, line)
    }

    fn no_named(&self, args: &[ast::Arg], name: &str) -> Result<()> {
        if let Some(a) = args.iter().find(|a| a.name.is_some()) {
            return Err(err(
                a.value.span,
                format!("`{name}` takes positional arguments"),
            ));
        }
        Ok(())
    }

    fn fact_args(&mut self, f: u32, args: &[ast::Arg], span: Span) -> Result<Vec<Ex>> {
        self.fact(f, span)?;
        let params = self.facts[f as usize].as_ref().unwrap().params.clone();
        if params.len() != args.len() {
            let (w, item) = self.fact_decls[f as usize];
            return Err(err(
                span,
                format!(
                    "`{w}.{}` takes {} argument{}",
                    item.name,
                    params.len(),
                    if params.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        let mut xs = Vec::new();
        for (a, p) in args.iter().zip(&params) {
            xs.push(self.expr_as(&a.value, p)?);
        }
        Ok(xs)
    }

    fn variant_call(
        &mut self,
        e: u32,
        tag: u32,
        args: &[ast::Arg],
        span: Span,
    ) -> Result<(Ex, Ty)> {
        let fields = self.enums[e as usize].variants[tag as usize].fields.clone();
        if fields.len() != args.len() {
            return Err(err(
                span,
                format!(
                    "`{}` has {} field{}",
                    self.enums[e as usize].variants[tag as usize].name,
                    fields.len(),
                    if fields.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        let mut xs = Vec::new();
        for (a, (fname, t)) in args.iter().zip(&fields) {
            if let Some(n) = &a.name
                && n != fname
            {
                return Err(err(a.value.span, format!("expected field `{fname}`")));
            }
            xs.push(self.expr_as(&a.value, t)?);
        }
        Ok((Ex::Variant(tag, xs), Ty::Enum(e)))
    }

    fn forecast(&mut self, is_forecast: bool, args: &[ast::Arg], span: Span) -> Result<(Ex, Ty)> {
        let what = if is_forecast { "forecast" } else { "rollout" };
        let (model, oracle) = match self.frame.ctx {
            Ctx::Policy { model, oracle } => (model, oracle),
            _ => {
                return Err(err(
                    span,
                    format!("`{what}` is used by policies, to look ahead"),
                ));
            }
        };
        if is_forecast && oracle {
            return Err(err(
                span,
                "an oracle policy sees the real future: use rollout(...) instead of forecast",
            ));
        }
        if !is_forecast && !oracle {
            return Err(err(
                span,
                "rollout(...) runs the real world's future, so only an `oracle policy` may use \
                 it: a policy uses forecast(...)",
            ));
        }
        let m = self.models[model as usize].clone();
        let seq = matches!(m, ModelDef::Sequential(_));
        let mut action = None;
        let (mut horizon, mut worlds, mut skip, mut then) = (None, None, None, None);
        for (i, a) in args.iter().enumerate() {
            match (a.name.as_deref(), i) {
                (None, 0) => action = Some(self.expr_as(&a.value, m.action())?),
                (Some("horizon"), _) if seq => {
                    horizon = Some(Box::new(self.expr_as(&a.value, &Ty::Int)?))
                }
                (Some("worlds"), _) if is_forecast => {
                    worlds = Some(Box::new(self.expr_as(&a.value, &Ty::Int)?))
                }
                (Some("skip"), _) if is_forecast => {
                    skip = Some(Box::new(self.expr_as(&a.value, &Ty::Int)?))
                }
                (Some("then"), _) if seq => {
                    then = Some(Box::new(self.expr_as(&a.value, m.action())?))
                }
                (Some(n), _) => {
                    return Err(err(
                        a.value.span,
                        format!("`{what}` has no `{n}` argument here"),
                    ));
                }
                (None, _) => {
                    return Err(err(
                        a.value.span,
                        format!("`{what}` takes the action, then named arguments"),
                    ));
                }
            }
        }
        let action = action.ok_or_else(|| err(span, format!("`{what}` needs an action")))?;
        if seq && horizon.is_none() {
            return Err(err(span, format!("`{what}` needs `horizon: steps`")));
        }
        let line = span.line;
        let outcome = m.outcome().clone();
        if is_forecast {
            let worlds = worlds.ok_or_else(|| err(span, "`forecast` needs `worlds: n`"))?;
            Ok((
                Ex::Forecast {
                    model,
                    action: Box::new(action),
                    horizon,
                    worlds,
                    skip,
                    then,
                    line,
                },
                Ty::Arr(Box::new(outcome)),
            ))
        } else {
            Ok((
                Ex::Rollout {
                    model,
                    action: Box::new(action),
                    horizon,
                    then,
                    line,
                },
                outcome,
            ))
        }
    }

    /// The type of a statistic over values of type `t`.
    fn stat_ty(&mut self, kind: StatKind, name: &str, t: &Ty, span: Span) -> Result<Ty> {
        use StatKind::*;
        match t {
            Ty::Arr(inner) => Ok(Ty::Arr(Box::new(self.stat_ty(kind, name, inner, span)?))),
            Ty::Bool => match kind {
                Mean | Probability => Ok(Ty::FLOAT),
                Count => Ok(Ty::Int),
                _ => Err(err(
                    span,
                    format!(
                        "`{name}` needs numbers: use probability(...) or count(...) for conditions"
                    ),
                )),
            },
            Ty::Int => match kind {
                Min | Max => Ok(Ty::Int),
                Probability | Count => Err(err(span, format!("`{name}` needs a condition"))),
                _ => Ok(Ty::FLOAT),
            },
            Ty::Num(d, h) => match kind {
                Variance => {
                    let hint = self.pow_hint(t, 2);
                    Ok(Ty::Num(d.pow(2), hint))
                }
                Probability | Count => Err(err(span, format!("`{name}` needs a condition"))),
                _ => Ok(Ty::Num(*d, *h)),
            },
            _ => Err(err(
                span,
                format!(
                    "`{name}` applies to numbers, conditions or arrays of them, not {}",
                    self.ty_name(t)
                ),
            )),
        }
    }

    fn stat_q(
        &mut self,
        kind: StatKind,
        q: Option<f64>,
        args: &[ast::Arg],
        name: &str,
        span: Span,
    ) -> Result<Option<Box<Ex>>> {
        let needs_q = matches!(kind, StatKind::Quantile | StatKind::Cvar) && q.is_none();
        let want = if needs_q { 2 } else { 1 };
        if args.len() != want {
            return Err(err(
                span,
                if needs_q {
                    format!("`{name}` takes the values and a level, like {name}(x, 0.95)")
                } else {
                    format!("`{name}` takes one argument")
                },
            ));
        }
        Ok(match q {
            Some(q) => Some(Box::new(Ex::Const(Value::Num(q)))),
            None if needs_q => {
                let saved = self.frame.ctx;
                if let Ctx::Metric { model, .. } = saved {
                    self.frame.ctx = Ctx::Metric { model, row: false };
                }
                let r = self.expr_as(&args[1].value, &Ty::FLOAT);
                self.frame.ctx = saved;
                Some(Box::new(r?))
            }
            None => None,
        })
    }

    fn world_stat(
        &mut self,
        name: &str,
        kind: StatKind,
        q: Option<f64>,
        args: &[ast::Arg],
        span: Span,
    ) -> Result<(Ex, Ty)> {
        let Ctx::Metric { model, .. } = self.frame.ctx else {
            unreachable!()
        };
        let q = self.stat_q(kind, q, args, name, span)?;
        self.frame.ctx = Ctx::Metric { model, row: true };
        let r = (|| {
            let (ax, at) = self.expr(&args[0].value)?;
            let filter = match &args[0].filter {
                Some(f) => Some(Box::new(self.expr_as(f, &Ty::Bool)?)),
                None => None,
            };
            Ok((ax, at, filter))
        })();
        self.frame.ctx = Ctx::Metric { model, row: false };
        let (ax, at, filter) = r?;
        let ty = self.stat_ty(kind, name, &at, span)?;
        Ok((
            Ex::Stat {
                kind,
                arg: Box::new(ax),
                q,
                filter,
                line: span.line,
            },
            ty,
        ))
    }

    fn builtin(
        &mut self,
        name: &str,
        args: &[ast::Arg],
        span: Span,
        line: u32,
    ) -> Result<(Ex, Ty)> {
        let n = args.len();
        let arity = |want: usize| -> Result<()> {
            if n != want {
                return Err(err(
                    span,
                    format!(
                        "`{name}` takes {want} argument{}",
                        if want == 1 { "" } else { "s" }
                    ),
                ));
            }
            Ok(())
        };
        let a = |i: usize| &args[i].value;
        let bi = |b: Bi, xs: Vec<Ex>| Ex::Builtin(b, xs, line);
        match name {
            "min" | "max" if n == 2 => {
                let (lx, lt) = self.expr(a(0))?;
                let (rx, rt) = self.expr(a(1))?;
                let (lx, rx, t) = self.unify_num(a(0), lx, lt, a(1), rx, rt, span)?;
                let b = if name == "min" { Bi::Min2 } else { Bi::Max2 };
                Ok((bi(b, vec![lx, rx]), t))
            }
            "clamp" => {
                arity(3)?;
                let (x, xt) = self.expr(a(0))?;
                let (lo, lt) = self.expr(a(1))?;
                let (hi, ht) = self.expr(a(2))?;
                let _ = (x, xt, lo, lt, hi, ht);
                let (xs, t) = self.joined(&[a(0), a(1), a(2)], false)?;
                if !t.is_numeric() {
                    return Err(err(span, "`clamp` needs numbers"));
                }
                Ok((bi(Bi::Clamp, xs), t))
            }
            "abs" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                if !t.is_numeric() {
                    return Err(err(span, "`abs` needs a number"));
                }
                Ok((bi(Bi::Abs, vec![x]), t))
            }
            "sqrt" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                let (x, t) = self.num_operand(a(0), x, &t)?;
                let Ty::Num(d, _) = t else { unreachable!() };
                let Some(d2) = d.sqrt() else {
                    return Err(err(
                        span,
                        format!("can't take the square root of {}", self.ty_name(&t)),
                    ));
                };
                let hint = self.sqrt_hint(&t);
                Ok((bi(Bi::Sqrt, vec![x]), Ty::Num(d2, hint)))
            }
            "exp" | "ln" | "sin" | "cos" => {
                arity(1)?;
                let x = self.expr_as(a(0), &Ty::FLOAT).map_err(|_| {
                    err(
                        span,
                        format!("`{name}` needs a plain number, without units"),
                    )
                })?;
                let b = match name {
                    "exp" => Bi::Exp,
                    "ln" => Bi::Ln,
                    "sin" => Bi::Sin,
                    _ => Bi::Cos,
                };
                Ok((bi(b, vec![x]), Ty::FLOAT))
            }
            "pow" => {
                arity(2)?;
                let x = self.expr_as(a(0), &Ty::FLOAT)?;
                let y = self.expr_as(a(1), &Ty::FLOAT)?;
                Ok((bi(Bi::Pow, vec![x, y]), Ty::FLOAT))
            }
            "floor" | "ceil" | "round" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                let x = match t {
                    Ty::Int => Ex::ToNum(Box::new(x)),
                    Ty::Num(d, _) if d.is_none() => x,
                    _ => {
                        return Err(err(
                            span,
                            format!(
                                "`{name}` rounds a plain number: for a quantity, convert it first \
                                 (`{name}(x in min)`)"
                            ),
                        ));
                    }
                };
                let b = match name {
                    "floor" => Bi::Floor,
                    "ceil" => Bi::Ceil,
                    _ => Bi::Round,
                };
                Ok((bi(b, vec![x]), Ty::Int))
            }
            "float" => {
                arity(1)?;
                let x = self.expr_as(a(0), &Ty::Int)?;
                Ok((bi(Bi::Float, vec![x]), Ty::FLOAT))
            }
            "mod" => {
                arity(2)?;
                let (lx, lt) = self.expr(a(0))?;
                let (rx, rt) = self.expr(a(1))?;
                let (lx, rx, t) = self.unify_num(a(0), lx, lt, a(1), rx, rt, span)?;
                Ok((bi(Bi::Mod, vec![lx, rx]), t))
            }
            "len" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                if !matches!(t, Ty::Arr(_)) {
                    return Err(err(span, "`len` needs an array"));
                }
                Ok((bi(Bi::Len, vec![x]), Ty::Int))
            }
            "fill" => {
                arity(2)?;
                let nx = self.expr_as(a(0), &Ty::Int)?;
                let (v, t) = self.expr(a(1))?;
                Ok((bi(Bi::Fill, vec![nx, v]), Ty::Arr(Box::new(t))))
            }
            "sum" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                match t {
                    Ty::Arr(elem) if elem.is_numeric() => Ok((bi(Bi::Sum, vec![x]), *elem)),
                    _ => Err(err(span, "`sum` needs an array of numbers")),
                }
            }
            "any" | "all" => {
                arity(1)?;
                let x = self.expr_as(a(0), &Ty::Arr(Box::new(Ty::Bool)))?;
                Ok((
                    bi(if name == "any" { Bi::Any } else { Bi::All }, vec![x]),
                    Ty::Bool,
                ))
            }
            "argmin" | "argmax" => {
                arity(1)?;
                let (x, t) = self.expr(a(0))?;
                match t {
                    Ty::Arr(elem) if elem.is_numeric() => {}
                    _ => return Err(err(span, format!("`{name}` needs an array of numbers"))),
                }
                Ok((
                    bi(
                        if name == "argmin" {
                            Bi::ArgMin
                        } else {
                            Bi::ArgMax
                        },
                        vec![x],
                    ),
                    Ty::Int,
                ))
            }
            _ => {
                if let Some((kind, q)) = stat_name(name) {
                    let q = self.stat_q(kind, q, args, name, span)?;
                    let (x, t) = self.expr(a(0))?;
                    let Ty::Arr(elem) = &t else {
                        return Err(err(
                            span,
                            format!("`{name}` summarises an array (here {})", self.ty_name(&t)),
                        ));
                    };
                    let ty = self.stat_ty(kind, name, elem, span)?;
                    let mut xs = vec![x];
                    if let Some(q) = q {
                        xs.push(*q);
                    }
                    return Ok((bi(Bi::Stat(kind), xs), ty));
                }
                Err(err(span, format!("unknown function `{name}`")))
            }
        }
    }
}
