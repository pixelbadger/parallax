//! Recursive-descent parser. See the EBNF in README.md.

use std::rc::Rc;

use crate::ast::{Ann, Binding, Block, Expr, FuncDef, Interner, Op, Program, Stmt, Sym};
use crate::error::Error;
use crate::lexer::{Tok, Token, lex};

/// Binary operator precedence, loosest first.
const PRECEDENCE: [&[Op]; 5] = [
    &[Op::Or],
    &[Op::And],
    &[Op::Eq, Op::Gt, Op::Lt],
    &[Op::Add, Op::Sub],
    &[Op::Mul, Op::Div],
];

/// Deepest nesting of expressions and blocks the parser accepts.
const MAX_NESTING: usize = 256;

pub fn parse(src: &str, names: &mut Interner) -> Result<Program, Error> {
    let tokens = lex(src)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        no_struct: false,
        depth: 0,
        names,
    };
    let mut stmts = Vec::new();
    while parser.pos < tokens.len() {
        stmts.push(parser.stmt()?);
    }
    Ok(stmts.into())
}

struct Parser<'t, 'src> {
    tokens: &'t [Token<'src>],
    pos: usize,
    /// True while parsing the head of an `if`/loop/multiverse, where
    /// `name {` starts the block rather than a struct literal.
    no_struct: bool,
    depth: usize,
    names: &'t mut Interner,
}

type PResult<T> = Result<T, Error>;

impl<'src> Parser<'_, 'src> {
    fn peek(&self) -> Option<Tok> {
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    fn peek_at(&self, offset: usize) -> Option<Tok> {
        self.tokens.get(self.pos + offset).map(|t| t.kind)
    }

    fn at(&self, kind: Tok) -> bool {
        self.peek() == Some(kind)
    }

    fn error(&self, msg: &str) -> Error {
        Error::Syntax(match self.tokens.get(self.pos) {
            None => format!("Unexpected end of file. {msg}"),
            Some(t) => format!("Line {}: {msg}, got '{}'", t.line, t.text),
        })
    }

    fn expect(&mut self, kind: Tok) -> PResult<&'src str> {
        if !self.at(kind) {
            return Err(self.error(&format!("Expected {}", kind.name())));
        }
        self.pos += 1;
        Ok(self.tokens[self.pos - 1].text)
    }

    fn eat(&mut self, kind: Tok) -> bool {
        let found = self.at(kind);
        if found {
            self.pos += 1;
        }
        found
    }

    fn ident(&mut self) -> PResult<Sym> {
        let text = self.expect(Tok::Id)?;
        Ok(self.names.intern(text))
    }

    /// Runs `parse` with struct literals allowed or not, guarding nesting depth.
    fn nested<T>(
        &mut self,
        structs: bool,
        parse: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        if self.depth >= MAX_NESTING {
            return Err(self.error("Nesting too deep"));
        }
        let saved = std::mem::replace(&mut self.no_struct, !structs);
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        self.no_struct = saved;
        result
    }

    fn block(&mut self) -> PResult<Block> {
        self.expect(Tok::LBrace)?;
        let stmts = self.nested(true, |p| {
            let mut stmts = Vec::new();
            while !p.at(Tok::RBrace) {
                stmts.push(p.stmt()?);
            }
            Ok(stmts)
        })?;
        self.expect(Tok::RBrace)?;
        Ok(stmts.into())
    }

    /// The expression before a block: `if c {`, `repeat n {`, ...
    fn head(&mut self) -> PResult<Expr> {
        self.nested(false, Self::expr)
    }

    fn ident_list(&mut self, closer: Tok) -> PResult<Box<[Sym]>> {
        let mut names = Vec::new();
        while !self.at(closer) {
            names.push(self.ident()?);
            if !self.at(closer) {
                self.expect(Tok::Comma)?;
            }
        }
        Ok(names.into())
    }

    fn binding(&mut self) -> PResult<Binding> {
        self.pos += 1; // `let` or `pin`
        let name = self.ident()?;
        let mut ann = Ann::Any;
        if self.eat(Tok::Colon) {
            let prefix = self.peek();
            if matches!(prefix, Some(Tok::QMark | Tok::Tilde)) {
                self.pos += 1;
            }
            let text = format!(
                "{}{}",
                match prefix {
                    Some(Tok::QMark) => "?",
                    Some(Tok::Tilde) => "~",
                    _ => "",
                },
                self.expect(Tok::Id)?
            );
            ann = match prefix {
                Some(Tok::QMark) => Ann::Open(text.into()),
                Some(Tok::Tilde) => Ann::Future(text.into()),
                _ if text == "Any" => Ann::Any,
                _ => Ann::Collapsed(text.into()),
            };
        }
        self.expect(Tok::Eq)?;
        let expr = self.expr()?;
        self.expect(Tok::Semi)?;
        Ok(Binding { name, ann, expr })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let Some(t) = self.peek() else {
            return Err(self.error("Expected an expression"));
        };
        match t {
            Tok::Let => return Ok(Stmt::Let(self.binding()?)),
            Tok::Pin => return Ok(Stmt::Pin(self.binding()?)),
            Tok::Id if self.peek_at(1) == Some(Tok::Eq) => {
                let name = self.ident()?;
                self.pos += 1;
                let expr = self.expr()?;
                self.expect(Tok::Semi)?;
                return Ok(Stmt::Assign(name, expr));
            }
            Tok::Given => {
                self.pos += 1;
                let cond = self.expr()?;
                self.expect(Tok::Semi)?;
                return Ok(Stmt::Given(cond));
            }
            Tok::Reset | Tok::Commit | Tok::Discard => {
                self.pos += 1;
                let name = self.ident()?;
                self.expect(Tok::Semi)?;
                return Ok(match t {
                    Tok::Reset => Stmt::Reset(name),
                    Tok::Commit => Stmt::Commit(name),
                    _ => Stmt::Discard(name),
                });
            }
            Tok::TypeDef => {
                self.pos += 1;
                let name = self.ident()?;
                self.expect(Tok::Eq)?;
                self.expect(Tok::LBrace)?;
                let fields = self.ident_list(Tok::RBrace)?;
                self.expect(Tok::RBrace)?;
                self.expect(Tok::Semi)?;
                return Ok(Stmt::TypeDef(name, fields));
            }
            Tok::Fn => {
                self.pos += 1;
                let name = self.ident()?;
                self.expect(Tok::LParen)?;
                let params = self.ident_list(Tok::RParen)?;
                self.expect(Tok::RParen)?;
                self.expect(Tok::Eq)?;
                let body = self.block()?;
                return Ok(Stmt::Func(Rc::new(FuncDef { name, params, body })));
            }
            _ => {}
        }

        // expr_stmt: block expressions need no ';', nor does a block's trailing expr
        let expr = self.expr()?;
        if !self.eat(Tok::Semi) && !expr.is_block_expr() && !self.at(Tok::RBrace) {
            return Err(self.error("Expected ';'"));
        }
        Ok(Stmt::Expr(expr))
    }

    fn expr(&mut self) -> PResult<Expr> {
        self.binary(0)
    }

    fn binary(&mut self, level: usize) -> PResult<Expr> {
        if level == PRECEDENCE.len() {
            return self.postfix();
        }
        let mut left = self.binary(level + 1)?;
        while let Some(Tok::Op(op)) = self.peek() {
            if !PRECEDENCE[level].contains(&op) {
                break;
            }
            self.pos += 1;
            let right = self.binary(level + 1)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut node = self.primary()?;
        while self.eat(Tok::Dot) {
            node = Expr::Member(Box::new(node), self.ident()?);
        }
        Ok(node)
    }

    fn args(&mut self) -> PResult<Vec<Expr>> {
        self.nested(true, |p| {
            p.expect(Tok::LParen)?;
            let mut args = Vec::new();
            if !p.at(Tok::RParen) {
                args.push(p.expr()?);
                while p.eat(Tok::Comma) {
                    args.push(p.expr()?);
                }
            }
            p.expect(Tok::RParen)?;
            Ok(args)
        })
    }

    fn primary(&mut self) -> PResult<Expr> {
        let Some(t) = self.peek() else {
            return Err(self.error("Expected an expression"));
        };
        let text = self.tokens[self.pos].text;
        match t {
            Tok::Number => {
                let n = text
                    .parse()
                    .map_err(|_| self.error("Integer literal too large"))?;
                self.pos += 1;
                Ok(Expr::Int(n))
            }
            Tok::Str => {
                self.pos += 1;
                Ok(Expr::Str(text[1..text.len() - 1].into()))
            }
            Tok::Open => {
                self.pos += 1;
                if !self.at(Tok::LParen) {
                    return Ok(Expr::Open(None));
                }
                let args: [Expr; 2] = self
                    .args()?
                    .try_into()
                    .map_err(|_| self.error("open(...) takes exactly two bounds (lo, hi)"))?;
                let [lo, hi] = args;
                Ok(Expr::Open(Some(Box::new((lo, hi)))))
            }
            Tok::Fork => {
                self.pos += 1;
                Ok(Expr::Fork(self.block()?))
            }
            Tok::Observe => {
                self.pos += 1;
                let inner = self.nested(!self.no_struct, Self::expr)?;
                Ok(Expr::Observe(Box::new(inner)))
            }
            Tok::If => {
                self.pos += 1;
                let cond = self.head()?;
                let then_b = self.block()?;
                let else_b = if self.eat(Tok::Else) {
                    Some(self.block()?)
                } else {
                    None
                };
                Ok(Expr::If(Box::new(cond), then_b, else_b))
            }
            Tok::Repeat | Tok::While | Tok::Multiverse => {
                self.pos += 1;
                let head = Box::new(self.head()?);
                let body = self.block()?;
                Ok(match t {
                    Tok::Repeat => Expr::Repeat(head, body),
                    Tok::While => Expr::While(head, body),
                    _ => Expr::Multiverse(head, body),
                })
            }
            Tok::Id => {
                let name = self.ident()?;
                if self.at(Tok::LBrace) && !self.no_struct {
                    self.pos += 1;
                    let fields = self.nested(true, |p| {
                        let mut fields = Vec::new();
                        while !p.at(Tok::RBrace) {
                            let key = p.ident()?;
                            p.expect(Tok::Colon)?;
                            fields.push((key, p.expr()?));
                            if !p.at(Tok::RBrace) {
                                p.expect(Tok::Comma)?;
                            }
                        }
                        Ok(fields)
                    })?;
                    self.expect(Tok::RBrace)?;
                    return Ok(Expr::StructInit(name, fields.into()));
                }
                if self.at(Tok::LParen) {
                    return Ok(Expr::Call(name, self.args()?.into()));
                }
                Ok(Expr::Var(name))
            }
            Tok::LParen => {
                self.pos += 1;
                let inner = self.nested(true, Self::expr)?;
                self.expect(Tok::RParen)?;
                Ok(inner)
            }
            _ => Err(self.error("Expected an expression")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Program {
        parse(src, &mut Interner::default()).unwrap()
    }

    fn parse_err(src: &str) -> String {
        parse(src, &mut Interner::default())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn precedence_and_associativity() {
        let prog = parse_ok("1 - 2 - 3 * 4 > 0 || 1;");
        let Stmt::Expr(Expr::Binary(Op::Or, lhs, _)) = &prog[0] else {
            panic!()
        };
        let Expr::Binary(Op::Gt, sum, _) = &**lhs else {
            panic!()
        };
        let Expr::Binary(Op::Sub, left, right) = &**sum else {
            panic!()
        };
        assert!(matches!(**left, Expr::Binary(Op::Sub, ..)));
        assert!(matches!(**right, Expr::Binary(Op::Mul, ..)));
    }

    #[test]
    fn struct_literal_not_allowed_in_head() {
        let prog = parse_ok("if flag { 1 }");
        let Stmt::Expr(Expr::If(cond, ..)) = &prog[0] else {
            panic!()
        };
        assert!(matches!(**cond, Expr::Var(_)));
        let prog = parse_ok("if (P { a: 1 }).a { 1 }");
        assert!(matches!(&prog[0], Stmt::Expr(Expr::If(..))));
    }

    #[test]
    fn block_expressions_need_no_semicolon() {
        parse_ok("fn f() = { if 1 { 2 } else { 3 } repeat 2 { 1 } 4 }");
        assert_eq!(
            parse_err("fn f() = { 1 2 }"),
            "Line 1: Expected ';', got '2'"
        );
    }

    #[test]
    fn syntax_errors() {
        assert_eq!(
            parse_err("let x = 1"),
            "Unexpected end of file. Expected SEMI"
        );
        assert_eq!(
            parse_err("let x = open(1);"),
            "Line 1: open(...) takes exactly two bounds (lo, hi), got ';'"
        );
        assert_eq!(parse_err("\nlet = 1;"), "Line 2: Expected ID, got '='");
        // Debug builds need more than the default test-thread stack for this
        let deep =
            crate::with_stack(|| parse_err(&format!("{}1{};", "(".repeat(2000), ")".repeat(2000))));
        assert!(deep.contains("Nesting too deep"));
    }
}
