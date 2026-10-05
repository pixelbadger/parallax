//! Hand-written lexer.
//!
//! Newlines end statements, as in Go or Swift: a newline becomes a `;` when
//! the line's last token can end an expression and the next line's first
//! token can't continue one. Inside `(` or `[` newlines are ignored, so long
//! argument lists and arrays can span lines; inside `{` they count again.

use crate::error::{Error, ErrorKind, Result, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kw {
    Const,
    Input,
    Derived,
    Unit,
    Type,
    Enum,
    Action,
    World,
    Latent,
    Uncertain,
    Model,
    Policy,
    Oracle,
    Study,
    Fn,
    Let,
    Var,
    For,
    In,
    While,
    Iterate,
    If,
    Else,
    Match,
    Assert,
    And,
    Or,
    Not,
    True,
    False,
    Where,
}

const KEYWORDS: &[(&str, Kw)] = &[
    ("const", Kw::Const),
    ("input", Kw::Input),
    ("derived", Kw::Derived),
    ("unit", Kw::Unit),
    ("type", Kw::Type),
    ("enum", Kw::Enum),
    ("action", Kw::Action),
    ("world", Kw::World),
    ("latent", Kw::Latent),
    ("uncertain", Kw::Uncertain),
    ("model", Kw::Model),
    ("policy", Kw::Policy),
    ("oracle", Kw::Oracle),
    ("study", Kw::Study),
    ("fn", Kw::Fn),
    ("let", Kw::Let),
    ("var", Kw::Var),
    ("for", Kw::For),
    ("in", Kw::In),
    ("while", Kw::While),
    ("iterate", Kw::Iterate),
    ("if", Kw::If),
    ("else", Kw::Else),
    ("match", Kw::Match),
    ("assert", Kw::Assert),
    ("and", Kw::And),
    ("or", Kw::Or),
    ("not", Kw::Not),
    ("true", Kw::True),
    ("false", Kw::False),
    ("where", Kw::Where),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    Kw(Kw),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semi,
    Dot,
    DotDot,
    DotDotEq,
    Arrow,
    FatArrow,
    Plus,
    Minus,
    Star,
    Slash,
    SlashSlash,
    Caret,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    Assign,
    Bar,
    Tilde,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(n) => format!("`{n}`"),
            Tok::Float(x) => format!("`{x}`"),
            Tok::Str(_) => "a string".into(),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Kw(k) => {
                let name = KEYWORDS.iter().find(|(_, kw)| kw == k).map(|(s, _)| *s);
                format!("`{}`", name.unwrap_or("?"))
            }
            Tok::Semi => "end of statement".into(),
            Tok::Eof => "end of file".into(),
            other => {
                let s = match other {
                    Tok::LParen => "(",
                    Tok::RParen => ")",
                    Tok::LBracket => "[",
                    Tok::RBracket => "]",
                    Tok::LBrace => "{",
                    Tok::RBrace => "}",
                    Tok::Comma => ",",
                    Tok::Colon => ":",
                    Tok::Dot => ".",
                    Tok::DotDot => "..",
                    Tok::DotDotEq => "..=",
                    Tok::Arrow => "->",
                    Tok::FatArrow => "=>",
                    Tok::Plus => "+",
                    Tok::Minus => "-",
                    Tok::Star => "*",
                    Tok::Slash => "/",
                    Tok::SlashSlash => "//",
                    Tok::Caret => "^",
                    Tok::Percent => "%",
                    Tok::EqEq => "==",
                    Tok::NotEq => "!=",
                    Tok::Lt => "<",
                    Tok::Gt => ">",
                    Tok::Le => "<=",
                    Tok::Ge => ">=",
                    Tok::Assign => "=",
                    Tok::Bar => "|",
                    Tok::Tilde => "~",
                    _ => "?",
                };
                format!("`{s}`")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// A newline separates this token from the previous one.
    pub nl: bool,
}

fn syntax(line: u32, col: u32, msg: impl Into<String>) -> Error {
    Error::at(
        ErrorKind::Syntax,
        Span {
            line,
            col,
            start: 0,
            end: 0,
        },
        msg,
    )
}

/// Characters that are identifiers on their own: currency units.
fn currency(c: char) -> bool {
    matches!(c, '£' | '€' | '$' | '¢')
}

pub fn lex(src: &str) -> Result<Vec<Token>> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(_, c)| c);
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1u32, 1u32);
    let mut nl = false;

    while i < chars.len() {
        let (off, c) = chars[i];
        if c == '\n' {
            nl = true;
            line += 1;
            col = 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            col += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        let (tline, tcol) = (line, col);
        let start_i = i;
        let tok = if c.is_ascii_digit() {
            let mut text = String::new();
            let mut float = false;
            while let Some(d) = at(i) {
                if d.is_ascii_digit() || d == '_' {
                    if d != '_' {
                        text.push(d);
                    }
                    i += 1;
                } else {
                    break;
                }
            }
            if at(i) == Some('.') && at(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                float = true;
                text.push('.');
                i += 1;
                while let Some(d) = at(i) {
                    if d.is_ascii_digit() || d == '_' {
                        if d != '_' {
                            text.push(d);
                        }
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            if matches!(at(i), Some('e' | 'E')) {
                let digits_at = if matches!(at(i + 1), Some('+' | '-')) {
                    i + 2
                } else {
                    i + 1
                };
                if at(digits_at).is_some_and(|d| d.is_ascii_digit()) {
                    float = true;
                    text.push('e');
                    if digits_at == i + 2 {
                        text.push(at(i + 1).unwrap());
                    }
                    i = digits_at;
                    while let Some(d) = at(i) {
                        if d.is_ascii_digit() {
                            text.push(d);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            if float {
                let x: f64 = text
                    .parse()
                    .map_err(|_| syntax(tline, tcol, format!("bad number `{text}`")))?;
                if !x.is_finite() {
                    return Err(syntax(tline, tcol, format!("number `{text}` is too large")));
                }
                Tok::Float(x)
            } else {
                let n: i64 = text
                    .parse()
                    .map_err(|_| syntax(tline, tcol, format!("integer `{text}` is too large")))?;
                Tok::Int(n)
            }
        } else if c.is_alphabetic()
            || c == '_'
            || (c == '°' && at(i + 1).is_some_and(|d| d.is_alphabetic()))
        {
            let mut s = String::new();
            s.push(c);
            i += 1;
            while let Some(d) = at(i) {
                if d.is_alphanumeric() || d == '_' {
                    s.push(d);
                    i += 1;
                } else {
                    break;
                }
            }
            match KEYWORDS.iter().find(|(k, _)| *k == s) {
                Some(&(_, kw)) => Tok::Kw(kw),
                None => Tok::Ident(s),
            }
        } else if currency(c) {
            i += 1;
            Tok::Ident(c.to_string())
        } else if c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                match at(i) {
                    None | Some('\n') => {
                        return Err(syntax(tline, tcol, "unterminated string"));
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\\') => {
                        let e = match at(i + 1) {
                            Some('n') => '\n',
                            Some('t') => '\t',
                            Some('"') => '"',
                            Some('\\') => '\\',
                            _ => return Err(syntax(tline, tcol, "bad escape in string")),
                        };
                        s.push(e);
                        i += 2;
                    }
                    Some(d) => {
                        s.push(d);
                        i += 1;
                    }
                }
            }
            Tok::Str(s)
        } else {
            let next = at(i + 1);
            let (tok, len) = match (c, next) {
                ('.', Some('.')) if at(i + 2) == Some('=') => (Tok::DotDotEq, 3),
                ('.', Some('.')) => (Tok::DotDot, 2),
                ('-', Some('>')) => (Tok::Arrow, 2),
                ('=', Some('>')) => (Tok::FatArrow, 2),
                ('=', Some('=')) => (Tok::EqEq, 2),
                ('!', Some('=')) => (Tok::NotEq, 2),
                ('<', Some('=')) => (Tok::Le, 2),
                ('>', Some('=')) => (Tok::Ge, 2),
                ('/', Some('/')) => (Tok::SlashSlash, 2),
                ('(', _) => (Tok::LParen, 1),
                (')', _) => (Tok::RParen, 1),
                ('[', _) => (Tok::LBracket, 1),
                (']', _) => (Tok::RBracket, 1),
                ('{', _) => (Tok::LBrace, 1),
                ('}', _) => (Tok::RBrace, 1),
                (',', _) => (Tok::Comma, 1),
                (':', _) => (Tok::Colon, 1),
                (';', _) => (Tok::Semi, 1),
                ('.', _) => (Tok::Dot, 1),
                ('+', _) => (Tok::Plus, 1),
                ('-', _) => (Tok::Minus, 1),
                ('*', _) => (Tok::Star, 1),
                ('/', _) => (Tok::Slash, 1),
                ('^', _) => (Tok::Caret, 1),
                ('%', _) => (Tok::Percent, 1),
                ('<', _) => (Tok::Lt, 1),
                ('>', _) => (Tok::Gt, 1),
                ('=', _) => (Tok::Assign, 1),
                ('|', _) => (Tok::Bar, 1),
                ('~', _) => (Tok::Tilde, 1),
                _ => return Err(syntax(tline, tcol, format!("unexpected character `{c}`"))),
            };
            i += len;
            tok
        };
        col += (i - start_i) as u32;
        let end = chars.get(i).map_or(src.len(), |&(o, _)| o);
        out.push(Token {
            tok,
            span: Span {
                line: tline,
                col: tcol,
                start: off as u32,
                end: end as u32,
            },
            nl,
        });
        nl = false;
    }
    out.push(Token {
        tok: Tok::Eof,
        span: Span {
            line,
            col,
            start: src.len() as u32,
            end: src.len() as u32,
        },
        nl: true,
    });
    Ok(insert_terminators(out))
}

fn ends_expr(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Int(_)
            | Tok::Float(_)
            | Tok::Str(_)
            | Tok::Ident(_)
            | Tok::Kw(Kw::True | Kw::False)
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Percent
    )
}

fn continues(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Plus
            | Tok::Minus
            | Tok::Star
            | Tok::Slash
            | Tok::SlashSlash
            | Tok::Caret
            | Tok::EqEq
            | Tok::NotEq
            | Tok::Lt
            | Tok::Gt
            | Tok::Le
            | Tok::Ge
            | Tok::Kw(Kw::And | Kw::Or | Kw::In | Kw::Else | Kw::Where)
            | Tok::Dot
            | Tok::DotDot
            | Tok::DotDotEq
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Comma
            | Tok::Arrow
            | Tok::FatArrow
            | Tok::LBrace
            | Tok::Bar
            | Tok::Tilde
            | Tok::Assign
            | Tok::Colon
            | Tok::Eof
    )
}

fn insert_terminators(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len() + tokens.len() / 4);
    let mut stack: Vec<Tok> = Vec::new();
    for t in tokens {
        let significant = matches!(stack.last(), None | Some(Tok::LBrace));
        if t.nl
            && significant
            && out.last().is_some_and(|p| ends_expr(&p.tok))
            && !continues(&t.tok)
        {
            let span = out.last().unwrap().span;
            out.push(Token {
                tok: Tok::Semi,
                span: Span {
                    start: span.end,
                    ..span
                },
                nl: false,
            });
        }
        match t.tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => stack.push(t.tok.clone()),
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                stack.pop();
            }
            _ => {}
        }
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn newlines_end_statements_only_where_an_expression_can_end() {
        assert_eq!(
            toks("let a = 1\nlet b = a +\n 2"),
            vec![
                Tok::Kw(Kw::Let),
                Tok::Ident("a".into()),
                Tok::Assign,
                Tok::Int(1),
                Tok::Semi,
                Tok::Kw(Kw::Let),
                Tok::Ident("b".into()),
                Tok::Assign,
                Tok::Ident("a".into()),
                Tok::Plus,
                Tok::Int(2),
                Tok::Eof
            ]
        );
        // inside parentheses a newline is just space
        assert!(!toks("f(a,\n b\n)").contains(&Tok::Semi));
    }

    #[test]
    fn numbers_and_units() {
        assert_eq!(
            toks("10_000 1.5e3 2e-1 °C £"),
            vec![
                Tok::Int(10000),
                Tok::Float(1500.0),
                Tok::Float(0.2),
                Tok::Ident("°C".into()),
                Tok::Ident("£".into()),
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("0..10"),
            vec![Tok::Int(0), Tok::DotDot, Tok::Int(10), Tok::Eof]
        );
    }
}
