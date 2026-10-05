//! Recursive-descent parser. See the grammar in README.md.

use std::collections::HashSet;

use crate::ast::*;
use crate::error::{Error, ErrorKind, Result, Span};
use crate::lexer::{Kw, Tok, Token, lex};
use crate::units::is_builtin_unit;

pub fn parse(src: &str) -> Result<Program> {
    let tokens = lex(src)?;
    // Units the program declares can follow a number anywhere in the source.
    let mut units = HashSet::new();
    for w in tokens.windows(2) {
        if let (Tok::Kw(Kw::Unit), Tok::Ident(name)) = (&w[0].tok, &w[1].tok) {
            units.insert(name.clone());
        }
    }
    let mut p = Parser {
        tokens,
        pos: 0,
        no_struct: false,
        units,
    };
    p.program()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// In `if`, `match` and `for` heads, `Name {` starts the block, not a record.
    no_struct: bool,
    units: HashSet<String>,
}

fn err(span: Span, msg: impl Into<String>) -> Error {
    Error::at(ErrorKind::Syntax, span, msg)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn next(&mut self) -> Tok {
        let t = self.tokens[self.pos].tok.clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<Span> {
        if self.peek() == t {
            let s = self.span();
            self.next();
            Ok(s)
        } else {
            Err(err(
                self.span(),
                format!("expected {what}, found {}", self.peek().describe()),
            ))
        }
    }

    fn is_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Ident(n) if n == s)
    }

    fn ident(&mut self, what: &str) -> Result<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                let span = self.span();
                self.next();
                Ok((s, span))
            }
            t => Err(err(
                self.span(),
                format!("expected {what}, found {}", t.describe()),
            )),
        }
    }

    fn skip_semis(&mut self) {
        while self.eat(&Tok::Semi) {}
    }

    /// After a declaration or clause: a terminator, or the closing `}` / end.
    fn end_item(&mut self) -> Result<()> {
        match self.peek() {
            Tok::Semi => {
                self.skip_semis();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            t => Err(err(
                self.span(),
                format!("expected a new line or `;`, found {}", t.describe()),
            )),
        }
    }

    /// After a clause in a study, world or model: as `end_item`, or the
    /// start of the next clause on the same line.
    fn end_clause(&mut self) -> Result<()> {
        match self.peek() {
            Tok::Ident(_) | Tok::Kw(Kw::Latent | Kw::Uncertain | Kw::Derived | Kw::Model) => Ok(()),
            _ => self.end_item(),
        }
    }

    fn is_unit(&self, name: &str) -> bool {
        is_builtin_unit(name) || self.units.contains(name)
    }

    fn program(&mut self) -> Result<Program> {
        let mut decls = Vec::new();
        self.skip_semis();
        while *self.peek() != Tok::Eof {
            decls.push(self.decl()?);
            if *self.peek() == Tok::RBrace {
                return Err(err(self.span(), "unexpected `}`"));
            }
            self.end_item()?;
        }
        Ok(Program { decls })
    }

    fn decl(&mut self) -> Result<Decl> {
        let start = self.span();
        match self.next() {
            Tok::Kw(kw @ (Kw::Const | Kw::Derived)) => {
                let (name, _) = self.ident("a name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.ty()?)
                } else {
                    None
                };
                self.expect(&Tok::Assign, "`=`")?;
                let value = self.expr()?;
                Ok(Decl::Const {
                    name,
                    ty,
                    value,
                    derived: kw == Kw::Derived,
                    span: start,
                })
            }
            Tok::Kw(Kw::Input) => {
                let (name, _) = self.ident("an input name")?;
                self.expect(&Tok::Colon, "`:` and the input's type")?;
                let ty = self.ty()?;
                let (mut between, mut default) = (None, None);
                loop {
                    if self.is_ident("between") && between.is_none() {
                        self.next();
                        let lo = self.cmp_expr()?;
                        self.expect(&Tok::Kw(Kw::And), "`and`")?;
                        let hi = self.cmp_expr()?;
                        between = Some((lo, hi));
                    } else if *self.peek() == Tok::Assign && default.is_none() {
                        self.next();
                        default = Some(self.cmp_expr()?);
                    } else {
                        break;
                    }
                }
                Ok(Decl::Input {
                    name,
                    ty,
                    between,
                    default,
                    span: start,
                })
            }
            Tok::Kw(Kw::Unit) => {
                let (name, _) = self.ident("a unit name")?;
                let def = if self.eat(&Tok::Assign) {
                    Some(self.expr()?)
                } else {
                    None
                };
                Ok(Decl::Unit {
                    name,
                    def,
                    span: start,
                })
            }
            Tok::Kw(Kw::Type) => {
                let (name, _) = self.ident("a type name")?;
                self.expect(&Tok::Assign, "`=`")?;
                let ty = if *self.peek() == Tok::LBrace {
                    self.record_type()?
                } else {
                    self.ty()?
                };
                Ok(Decl::Type {
                    name,
                    ty,
                    span: start,
                })
            }
            Tok::Kw(Kw::Enum) => {
                let (name, _) = self.ident("an enum name")?;
                self.expect(&Tok::Assign, "`=`")?;
                let variants = self.variants()?;
                Ok(Decl::Enum {
                    name,
                    variants,
                    action: false,
                    span: start,
                })
            }
            Tok::Kw(Kw::Action) => {
                let (name, _) = self.ident("an action type name")?;
                self.expect(&Tok::Assign, "`=`")?;
                // `action Mode = full | throttled` declares variants;
                // `action Lead = min` names a type. A single bare name is
                // resolved by the checker.
                let is_variants = matches!(
                    (self.peek(), self.peek_at(1)),
                    (Tok::Ident(_), Tok::Bar | Tok::LParen)
                );
                if is_variants {
                    let variants = self.variants()?;
                    Ok(Decl::Enum {
                        name,
                        variants,
                        action: true,
                        span: start,
                    })
                } else {
                    let ty = if *self.peek() == Tok::LBrace {
                        self.record_type()?
                    } else {
                        self.ty()?
                    };
                    Ok(Decl::ActionType {
                        name,
                        ty,
                        span: start,
                    })
                }
            }
            Tok::Kw(Kw::World) => {
                let (name, _) = self.ident("a world name")?;
                self.expect(&Tok::LBrace, "`{`")?;
                let mut items = Vec::new();
                self.skip_semis();
                while !self.eat(&Tok::RBrace) {
                    items.push(self.world_item()?);
                    self.end_clause()?;
                }
                Ok(Decl::World {
                    name,
                    items,
                    span: start,
                })
            }
            Tok::Kw(Kw::Fn) => {
                let (name, _) = self.ident("a function name")?;
                let params = self.params()?;
                let ret = self.ret()?;
                let body = self.body()?;
                Ok(Decl::Fn(FnDecl {
                    name,
                    params,
                    ret,
                    body,
                    span: start,
                }))
            }
            Tok::Kw(Kw::Model) => self.model(start),
            Tok::Kw(Kw::Oracle) => {
                self.expect(&Tok::Kw(Kw::Policy), "`policy` after `oracle`")?;
                self.policy(true, start)
            }
            Tok::Kw(Kw::Policy) => self.policy(false, start),
            Tok::Kw(Kw::Study) => self.study(start),
            t => Err(err(
                start,
                format!(
                    "expected a declaration (input, const, derived, unit, type, enum, action, \
                     world, fn, model, policy, oracle policy or study), found {}",
                    t.describe()
                ),
            )),
        }
    }

    fn variants(&mut self) -> Result<Vec<Variant>> {
        let mut out = Vec::new();
        loop {
            let (name, span) = self.ident("a variant name")?;
            let mut fields = Vec::new();
            if self.eat(&Tok::LParen) {
                while *self.peek() != Tok::RParen {
                    let (fname, fspan) = self.ident("a field name")?;
                    self.expect(&Tok::Colon, "`:`")?;
                    let ty = self.ty()?;
                    fields.push(Field {
                        name: fname,
                        ty,
                        span: fspan,
                    });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
            }
            out.push(Variant { name, fields, span });
            if !self.eat(&Tok::Bar) {
                return Ok(out);
            }
        }
    }

    fn world_item(&mut self) -> Result<WorldItem> {
        let span = self.span();
        let kind = match self.next() {
            Tok::Kw(Kw::Latent) => FactKind::Latent,
            Tok::Kw(Kw::Uncertain) => FactKind::Uncertain,
            Tok::Kw(Kw::Derived) => FactKind::Derived,
            t => {
                return Err(err(
                    span,
                    format!(
                        "expected `latent`, `uncertain` or `derived` in a world, found {}",
                        t.describe()
                    ),
                ));
            }
        };
        let (name, _) = self.ident("a fact name")?;
        let params = if *self.peek() == Tok::LParen {
            self.params()?
        } else {
            Vec::new()
        };
        let ty = if self.eat(&Tok::Colon) {
            Some(self.ty()?)
        } else {
            None
        };
        if kind == FactKind::Derived {
            self.expect(&Tok::Assign, "`=`")?;
        } else {
            self.expect(&Tok::Tilde, "`~` and a distribution")?;
        }
        let body = self.expr()?;
        Ok(WorldItem {
            kind,
            name,
            params,
            ty,
            body,
            span,
        })
    }

    fn model(&mut self, start: Span) -> Result<Decl> {
        let (name, _) = self.ident("a model name")?;
        if *self.peek() == Tok::LParen {
            let params = self.params()?;
            if params.len() != 1 {
                return Err(err(
                    start,
                    "a decision model takes exactly one parameter: the action",
                ));
            }
            let ret = self.ret()?;
            let body = self.body()?;
            return Ok(Decl::Model(ModelDecl::Decision {
                name,
                param: params.into_iter().next().unwrap(),
                ret,
                body,
                span: start,
            }));
        }
        self.expect(&Tok::LBrace, "`(action: Type)` or `{`")?;
        let mut horizon = None;
        let mut clauses = Vec::new();
        self.skip_semis();
        while !self.eat(&Tok::RBrace) {
            let (cname, cspan) = self.ident("a model clause")?;
            if cname == "horizon" {
                horizon = Some(self.expr()?);
            } else {
                let params = if cname == "init" && *self.peek() != Tok::LParen {
                    Vec::new()
                } else {
                    self.params()?
                };
                let ret = self.ret()?;
                let body = self.body()?;
                clauses.push(FnDecl {
                    name: cname,
                    params,
                    ret,
                    body,
                    span: cspan,
                });
            }
            self.end_clause()?;
        }
        Ok(Decl::Model(ModelDecl::Sequential {
            name,
            horizon,
            clauses,
            span: start,
        }))
    }

    fn policy(&mut self, oracle: bool, start: Span) -> Result<Decl> {
        let (name, _) = self.ident("a policy name")?;
        let family = if self.eat(&Tok::LBracket) {
            let (var, _) = self.ident("a parameter name")?;
            self.expect(&Tok::Kw(Kw::In), "`in`")?;
            let iter = self.iter()?;
            self.expect(&Tok::RBracket, "`]`")?;
            Some((var, iter))
        } else {
            None
        };
        let model = if self.eat(&Tok::Kw(Kw::For)) {
            Some(self.ident("a model name")?)
        } else {
            None
        };
        let params = if *self.peek() == Tok::LParen {
            self.params()?
        } else {
            Vec::new()
        };
        let ret = self.ret()?;
        let body = self.body()?;
        Ok(Decl::Policy(PolicyDecl {
            name,
            oracle,
            family,
            model,
            params,
            ret,
            body,
            span: start,
        }))
    }

    fn study(&mut self, start: Span) -> Result<Decl> {
        let (name, _) = self.ident("a study name")?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut clauses = Vec::new();
        self.skip_semis();
        while !self.eat(&Tok::RBrace) {
            let (word, span) = if *self.peek() == Tok::Kw(Kw::Model) {
                let span = self.span();
                self.next();
                ("model".to_string(), span)
            } else {
                self.ident("a study clause")?
            };
            let clause = match word.as_str() {
                "model" => {
                    let (m, s) = self.ident("a model name")?;
                    StudyClause::Model(m, s)
                }
                "worlds" => StudyClause::Worlds(self.expr()?),
                "seed" => StudyClause::Seed(self.expr()?),
                "with" => {
                    let mut list = Vec::new();
                    loop {
                        let (input, s) = self.ident("an input name")?;
                        self.expect(&Tok::Assign, "`=`")?;
                        list.push((input, self.expr()?, s));
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    StudyClause::With(list)
                }
                "compare" => {
                    if self.is_ident("all") {
                        self.next();
                        if self.is_ident("policies") {
                            self.next();
                        }
                        StudyClause::Compare(None, span)
                    } else {
                        let mut list = Vec::new();
                        loop {
                            list.push(self.ident("a policy name")?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                        StudyClause::Compare(Some(list), span)
                    }
                }
                "require" => StudyClause::Require(self.expr()?),
                "minimize" | "minimise" => StudyClause::Minimize(self.expr()?),
                "maximize" | "maximise" => StudyClause::Maximize(self.expr()?),
                "report" => {
                    let mut list = vec![self.expr()?];
                    while self.eat(&Tok::Comma) {
                        list.push(self.expr()?);
                    }
                    StudyClause::Report(list)
                }
                _ => {
                    return Err(err(
                        span,
                        format!(
                            "unknown study clause `{word}` (expected model, worlds, seed, with, \
                             compare, require, minimize, maximize or report)"
                        ),
                    ));
                }
            };
            clauses.push(clause);
            self.end_clause()?;
        }
        Ok(Decl::Study(StudyDecl {
            name,
            clauses,
            span: start,
        }))
    }

    fn params(&mut self) -> Result<Vec<Param>> {
        self.expect(&Tok::LParen, "`(`")?;
        let mut out = Vec::new();
        while *self.peek() != Tok::RParen {
            let (name, span) = self.ident("a parameter name")?;
            self.expect(&Tok::Colon, "`:` and the parameter's type")?;
            let ty = self.ty()?;
            out.push(Param { name, ty, span });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        Ok(out)
    }

    fn ret(&mut self) -> Result<Option<TypeExpr>> {
        if self.eat(&Tok::Arrow) {
            Ok(Some(self.ty()?))
        } else {
            Ok(None)
        }
    }

    fn body(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Assign) {
            self.expr()
        } else if *self.peek() == Tok::LBrace {
            self.block_expr()
        } else {
            Err(err(
                self.span(),
                format!("expected `=` or `{{`, found {}", self.peek().describe()),
            ))
        }
    }

    fn record_type(&mut self) -> Result<TypeExpr> {
        let start = self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        self.skip_semis();
        while *self.peek() != Tok::RBrace {
            let (name, span) = self.ident("a field name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.ty()?;
            fields.push(Field { name, ty, span });
            if !self.eat(&Tok::Comma) && !self.eat(&Tok::Semi) {
                break;
            }
            self.skip_semis();
        }
        self.skip_semis();
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(TypeExpr::Record(fields, start))
    }

    fn ty(&mut self) -> Result<TypeExpr> {
        let span = self.span();
        if self.eat(&Tok::LBracket) {
            let elem = self.ty()?;
            let len = if self.eat(&Tok::Semi) {
                Some(Box::new(self.expr()?))
            } else {
                None
            };
            self.expect(&Tok::RBracket, "`]`")?;
            return Ok(TypeExpr::Array(Box::new(elem), len, span));
        }
        if *self.peek() == Tok::LBrace {
            return Err(err(
                span,
                "record types are declared with `type Name = { ... }`",
            ));
        }
        let ue = self.unit_expr()?;
        if ue.len() == 1 && ue[0].1 == 1 && ue[0].0 != "%" {
            Ok(TypeExpr::Name(ue.into_iter().next().unwrap().0, span))
        } else {
            Ok(TypeExpr::Unit(ue, span))
        }
    }

    fn unit_atom(&mut self) -> Result<(String, i32)> {
        let name = if self.eat(&Tok::Percent) {
            "%".to_string()
        } else {
            self.ident("a type or unit")?.0
        };
        let mut power = 1;
        if self.eat(&Tok::Caret) {
            let neg = self.eat(&Tok::Minus);
            match self.next() {
                Tok::Int(n) if n <= 16 => power = if neg { -(n as i32) } else { n as i32 },
                _ => return Err(err(self.prev_span(), "expected a small integer power")),
            }
        }
        Ok((name, power))
    }

    /// `kWh`, `p/kWh`, `m/s^2`, `%`.
    fn unit_expr(&mut self) -> Result<UnitExpr> {
        let mut out = vec![self.unit_atom()?];
        loop {
            let sign = match self.peek() {
                Tok::Star => 1,
                Tok::Slash => -1,
                _ => break,
            };
            match self.peek_at(1) {
                Tok::Ident(n) if self.is_unit(n) => {}
                Tok::Percent => {}
                _ => break,
            }
            self.next();
            let (n, p) = self.unit_atom()?;
            out.push((n, p * sign));
        }
        Ok(out)
    }

    // ---- statements and blocks ----

    fn block(&mut self) -> Result<Block> {
        self.expect(&Tok::LBrace, "`{`")?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mut stmts = Vec::new();
        let mut tail = None;
        self.skip_semis();
        while *self.peek() != Tok::RBrace {
            let stmt = self.stmt()?;
            if *self.peek() == Tok::RBrace {
                match stmt {
                    Stmt::Expr(e) => tail = Some(Box::new(e)),
                    s => stmts.push(s),
                }
                break;
            }
            let blocky = matches!(&stmt, Stmt::For { .. } | Stmt::Iterate { .. })
                || matches!(&stmt, Stmt::Expr(e) if matches!(e.kind, ExprKind::If(..) | ExprKind::Match(..) | ExprKind::Block(_)));
            if !self.eat(&Tok::Semi) && !blocky {
                return Err(err(
                    self.span(),
                    format!(
                        "expected a new line or `;`, found {}",
                        self.peek().describe()
                    ),
                ));
            }
            self.skip_semis();
            stmts.push(stmt);
        }
        self.expect(&Tok::RBrace, "`}`")?;
        self.no_struct = saved;
        Ok(Block { stmts, tail })
    }

    fn block_expr(&mut self) -> Result<Expr> {
        let span = self.span();
        let b = self.block()?;
        Ok(Expr {
            kind: ExprKind::Block(b),
            span: span.to(self.prev_span()),
        })
    }

    fn stmt(&mut self) -> Result<Stmt> {
        let span = self.span();
        match self.peek() {
            Tok::Kw(kw @ (Kw::Let | Kw::Var)) => {
                let mutable = *kw == Kw::Var;
                self.next();
                let (name, _) = self.ident("a variable name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.ty()?)
                } else {
                    None
                };
                self.expect(&Tok::Assign, "`=`")?;
                let value = self.expr()?;
                Ok(Stmt::Let {
                    name,
                    mutable,
                    ty,
                    value,
                    span,
                })
            }
            Tok::Kw(Kw::For) => {
                self.next();
                let (var, _) = self.ident("a loop variable")?;
                self.expect(&Tok::Kw(Kw::In), "`in`")?;
                let saved = std::mem::replace(&mut self.no_struct, true);
                let iter = self.iter()?;
                let cond = if self.eat(&Tok::Kw(Kw::While)) {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.no_struct = saved;
                let body = self.block()?;
                Ok(Stmt::For {
                    var,
                    iter,
                    cond,
                    body,
                    span,
                })
            }
            Tok::Kw(Kw::While) => Err(err(
                span,
                "there is no `while` loop: use a bounded `for i in 0..n while cond { ... }`",
            )),
            Tok::Kw(Kw::Iterate) => {
                self.next();
                let saved = std::mem::replace(&mut self.no_struct, true);
                let count = self.expr()?;
                self.no_struct = saved;
                let body = self.block()?;
                Ok(Stmt::Iterate { count, body, span })
            }
            Tok::Kw(Kw::Assert) => {
                self.next();
                let cond = self.expr()?;
                let msg = if self.eat(&Tok::Comma) {
                    match self.next() {
                        Tok::Str(s) => Some(s),
                        _ => return Err(err(self.prev_span(), "expected a message string")),
                    }
                } else {
                    None
                };
                Ok(Stmt::Assert { cond, msg, span })
            }
            _ => {
                let e = self.expr()?;
                if self.eat(&Tok::Assign) {
                    let value = self.expr()?;
                    return Ok(Stmt::Assign {
                        target: e,
                        value,
                        span,
                    });
                }
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn iter(&mut self) -> Result<Iter> {
        let span = self.span();
        let first = self.add_expr()?;
        let inclusive = match self.peek() {
            Tok::DotDot => false,
            Tok::DotDotEq => true,
            _ => {
                return Ok(Iter {
                    kind: IterKind::Over(first),
                    span,
                });
            }
        };
        self.next();
        let hi = self.add_expr()?;
        let step = if self.is_ident("step") {
            self.next();
            Some(self.add_expr()?)
        } else {
            None
        };
        Ok(Iter {
            kind: IterKind::Range {
                lo: first,
                hi,
                inclusive,
                step,
            },
            span,
        })
    }

    // ---- expressions ----

    pub fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        let span = l.span.to(r.span);
        Expr {
            kind: ExprKind::Binary(op, Box::new(l), Box::new(r)),
            span,
        }
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut l = self.and_expr()?;
        while self.eat(&Tok::Kw(Kw::Or)) {
            let r = self.and_expr()?;
            l = Self::bin(BinOp::Or, l, r);
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut l = self.not_expr()?;
        while self.eat(&Tok::Kw(Kw::And)) {
            let r = self.not_expr()?;
            l = Self::bin(BinOp::And, l, r);
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        let span = self.span();
        if self.eat(&Tok::Kw(Kw::Not)) {
            let e = self.not_expr()?;
            let span = span.to(e.span);
            return Ok(Expr {
                kind: ExprKind::Not(Box::new(e)),
                span,
            });
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let l = self.conv_expr()?;
        let op = match self.peek() {
            Tok::EqEq => BinOp::Eq,
            Tok::NotEq => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Gt => BinOp::Gt,
            Tok::Le => BinOp::Le,
            Tok::Ge => BinOp::Ge,
            _ => return Ok(l),
        };
        self.next();
        let r = self.conv_expr()?;
        if matches!(
            self.peek(),
            Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Gt | Tok::Le | Tok::Ge
        ) {
            return Err(err(
                self.span(),
                "comparisons don't chain: write `a < b and b < c`",
            ));
        }
        Ok(Self::bin(op, l, r))
    }

    fn conv_expr(&mut self) -> Result<Expr> {
        let e = self.add_expr()?;
        if *self.peek() == Tok::Kw(Kw::In) {
            self.next();
            let ue = self.unit_expr()?;
            let span = e.span.to(self.prev_span());
            return Ok(Expr {
                kind: ExprKind::Convert(Box::new(e), ue),
                span,
            });
        }
        Ok(e)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut l = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => return Ok(l),
            };
            self.next();
            let r = self.mul_expr()?;
            l = Self::bin(op, l, r);
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::SlashSlash => BinOp::IDiv,
                _ => return Ok(l),
            };
            self.next();
            let r = self.unary()?;
            l = Self::bin(op, l, r);
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        let span = self.span();
        if self.eat(&Tok::Minus) {
            let e = self.unary()?;
            let span = span.to(e.span);
            return Ok(Expr {
                kind: ExprKind::Neg(Box::new(e)),
                span,
            });
        }
        let base = self.postfix()?;
        if self.eat(&Tok::Caret) {
            let exp = self.unary()?;
            return Ok(Self::bin(BinOp::Pow, base, exp));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.next();
                    let (name, s) = self.ident("a field name")?;
                    let span = e.span.to(s);
                    e = Expr {
                        kind: ExprKind::Field(Box::new(e), name),
                        span,
                    };
                }
                Tok::LBracket => {
                    self.next();
                    let saved = std::mem::replace(&mut self.no_struct, false);
                    let i = self.expr()?;
                    self.no_struct = saved;
                    self.expect(&Tok::RBracket, "`]`")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr {
                        kind: ExprKind::Index(Box::new(e), Box::new(i)),
                        span,
                    };
                }
                Tok::LParen if matches!(e.kind, ExprKind::Name(_) | ExprKind::Field(..)) => {
                    self.next();
                    let saved = std::mem::replace(&mut self.no_struct, false);
                    let mut args = Vec::new();
                    while *self.peek() != Tok::RParen {
                        let name = match (self.peek(), self.peek_at(1)) {
                            (Tok::Ident(n), Tok::Colon) => {
                                let n = n.clone();
                                self.next();
                                self.next();
                                Some(n)
                            }
                            _ => None,
                        };
                        let value = self.expr()?;
                        let filter = if self.eat(&Tok::Kw(Kw::Where)) {
                            Some(self.expr()?)
                        } else {
                            None
                        };
                        args.push(Arg {
                            name,
                            value,
                            filter,
                        });
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.no_struct = saved;
                    self.expect(&Tok::RParen, "`)`")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr {
                        kind: ExprKind::Call(Box::new(e), args),
                        span,
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    fn number_unit(&mut self, value: f64, span: Span) -> Result<Option<Expr>> {
        let has_unit = match self.peek() {
            Tok::Percent => true,
            Tok::Ident(n) => self.is_unit(n),
            _ => false,
        };
        if !has_unit {
            return Ok(None);
        }
        let ue = self.unit_expr()?;
        Ok(Some(Expr {
            kind: ExprKind::Quantity(value, ue),
            span: span.to(self.prev_span()),
        }))
    }

    fn primary(&mut self) -> Result<Expr> {
        let span = self.span();
        let mk = |kind| Expr { kind, span };
        match self.next() {
            Tok::Int(n) => Ok(match self.number_unit(n as f64, span)? {
                Some(q) => q,
                None => mk(ExprKind::Int(n)),
            }),
            Tok::Float(x) => Ok(match self.number_unit(x, span)? {
                Some(q) => q,
                None => mk(ExprKind::Float(x)),
            }),
            Tok::Str(s) => Ok(mk(ExprKind::Str(s))),
            Tok::Kw(Kw::True) => Ok(mk(ExprKind::Bool(true))),
            Tok::Kw(Kw::False) => Ok(mk(ExprKind::Bool(false))),
            Tok::Ident(name) => {
                if *self.peek() == Tok::LBrace && !self.no_struct {
                    return self.record(name, span);
                }
                Ok(mk(ExprKind::Name(name)))
            }
            Tok::LParen => {
                let saved = std::mem::replace(&mut self.no_struct, false);
                let e = self.expr()?;
                self.no_struct = saved;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Expr {
                    span: span.to(self.prev_span()),
                    ..e
                })
            }
            Tok::LBracket => {
                let saved = std::mem::replace(&mut self.no_struct, false);
                let mut elems = Vec::new();
                if *self.peek() != Tok::RBracket {
                    let first = self.expr()?;
                    if self.eat(&Tok::Kw(Kw::For)) {
                        let (var, _) = self.ident("a loop variable")?;
                        self.expect(&Tok::Kw(Kw::In), "`in`")?;
                        let iter = self.iter()?;
                        let filter = if self.eat(&Tok::Kw(Kw::If)) {
                            Some(Box::new(self.expr()?))
                        } else {
                            None
                        };
                        self.expect(&Tok::RBracket, "`]`")?;
                        self.no_struct = saved;
                        return Ok(Expr {
                            kind: ExprKind::Comp {
                                body: Box::new(first),
                                var,
                                iter: Box::new(iter),
                                filter,
                            },
                            span: span.to(self.prev_span()),
                        });
                    }
                    elems.push(first);
                    while self.eat(&Tok::Comma) {
                        if *self.peek() == Tok::RBracket {
                            break;
                        }
                        elems.push(self.expr()?);
                    }
                }
                self.no_struct = saved;
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(Expr {
                    kind: ExprKind::Array(elems),
                    span: span.to(self.prev_span()),
                })
            }
            Tok::LBrace => {
                self.pos -= 1;
                self.block_expr()
            }
            Tok::Kw(Kw::If) => self.if_rest(span),
            Tok::Kw(Kw::Match) => {
                let saved = std::mem::replace(&mut self.no_struct, true);
                let scrut = self.expr()?;
                self.no_struct = saved;
                self.expect(&Tok::LBrace, "`{`")?;
                let mut arms = Vec::new();
                self.skip_semis();
                while *self.peek() != Tok::RBrace {
                    let aspan = self.span();
                    let pat = if self.is_ident("_") {
                        self.next();
                        Pattern::Wild
                    } else {
                        let (first, _) = self.ident("a variant pattern")?;
                        let (qual, name) = if self.eat(&Tok::Dot) {
                            (Some(first), self.ident("a variant name")?.0)
                        } else {
                            (None, first)
                        };
                        let mut binds = Vec::new();
                        if self.eat(&Tok::LParen) {
                            while *self.peek() != Tok::RParen {
                                binds.push(self.ident("a binding name")?.0);
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                            self.expect(&Tok::RParen, "`)`")?;
                        }
                        Pattern::Variant { qual, name, binds }
                    };
                    self.expect(&Tok::FatArrow, "`=>`")?;
                    let body = self.expr()?;
                    arms.push(Arm {
                        pat,
                        body,
                        span: aspan,
                    });
                    if !self.eat(&Tok::Comma) && !self.eat(&Tok::Semi) {
                        break;
                    }
                    self.skip_semis();
                }
                self.skip_semis();
                self.expect(&Tok::RBrace, "`}`")?;
                Ok(Expr {
                    kind: ExprKind::Match(Box::new(scrut), arms),
                    span: span.to(self.prev_span()),
                })
            }
            t => Err(err(
                span,
                format!("expected an expression, found {}", t.describe()),
            )),
        }
    }

    fn if_rest(&mut self, span: Span) -> Result<Expr> {
        let saved = std::mem::replace(&mut self.no_struct, true);
        let cond = self.expr()?;
        self.no_struct = saved;
        let then = self.block_expr()?;
        let els = if self.eat(&Tok::Kw(Kw::Else)) {
            let espan = self.span();
            if self.eat(&Tok::Kw(Kw::If)) {
                Some(Box::new(self.if_rest(espan)?))
            } else {
                Some(Box::new(self.block_expr()?))
            }
        } else {
            None
        };
        Ok(Expr {
            kind: ExprKind::If(Box::new(cond), Box::new(then), els),
            span: span.to(self.prev_span()),
        })
    }

    fn record(&mut self, name: String, span: Span) -> Result<Expr> {
        self.expect(&Tok::LBrace, "`{`")?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mut fields = Vec::new();
        self.skip_semis();
        while *self.peek() != Tok::RBrace {
            let (f, fspan) = self.ident("a field name")?;
            let value = if self.eat(&Tok::Colon) {
                self.expr()?
            } else {
                Expr {
                    kind: ExprKind::Name(f.clone()),
                    span: fspan,
                }
            };
            fields.push((f, value));
            if !self.eat(&Tok::Comma) && !self.eat(&Tok::Semi) {
                break;
            }
            self.skip_semis();
        }
        self.skip_semis();
        self.no_struct = saved;
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Expr {
            kind: ExprKind::Record(name, fields),
            span: span.to(self.prev_span()),
        })
    }
}
