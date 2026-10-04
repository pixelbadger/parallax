//! Tokeniser.

use crate::ast::Op;
use crate::error::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Id,
    Number,
    Str,
    Op(Op),
    Eq,
    Dot,
    Colon,
    QMark,
    Tilde,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    // Keywords
    Fn,
    Let,
    Pin,
    Reset,
    TypeDef,
    Commit,
    Discard,
    Fork,
    Observe,
    Open,
    If,
    Else,
    Repeat,
    While,
    Multiverse,
    Given,
}

impl Tok {
    /// The name used in "Expected ..." syntax errors.
    pub fn name(self) -> &'static str {
        match self {
            Tok::Id => "ID",
            Tok::Number => "NUMBER",
            Tok::Str => "STRING",
            Tok::Op(_) => "OP",
            Tok::Eq => "EQ",
            Tok::Dot => "DOT",
            Tok::Colon => "COLON",
            Tok::QMark => "Q_MARK",
            Tok::Tilde => "TILDE",
            Tok::LParen => "LPAREN",
            Tok::RParen => "RPAREN",
            Tok::LBrace => "LBRACE",
            Tok::RBrace => "RBRACE",
            Tok::LBracket => "LBRACKET",
            Tok::RBracket => "RBRACKET",
            Tok::Semi => "SEMI",
            Tok::Comma => "COMMA",
            Tok::Fn => "FN",
            Tok::Let => "LET",
            Tok::Pin => "PIN",
            Tok::Reset => "RESET",
            Tok::TypeDef => "TYPE_DEF",
            Tok::Commit => "COMMIT",
            Tok::Discard => "DISCARD",
            Tok::Fork => "FORK",
            Tok::Observe => "OBSERVE",
            Tok::Open => "OPEN",
            Tok::If => "IF",
            Tok::Else => "ELSE",
            Tok::Repeat => "REPEAT",
            Tok::While => "WHILE",
            Tok::Multiverse => "MULTIVERSE",
            Tok::Given => "GIVEN",
        }
    }
}

fn keyword(text: &str) -> Option<Tok> {
    Some(match text {
        "fn" => Tok::Fn,
        "let" => Tok::Let,
        "pin" => Tok::Pin,
        "reset" => Tok::Reset,
        "type" => Tok::TypeDef,
        "commit" => Tok::Commit,
        "discard" => Tok::Discard,
        "fork" => Tok::Fork,
        "observe" => Tok::Observe,
        "open" => Tok::Open,
        "if" => Tok::If,
        "else" => Tok::Else,
        "repeat" => Tok::Repeat,
        "while" => Tok::While,
        "multiverse" => Tok::Multiverse,
        "given" => Tok::Given,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Token<'src> {
    pub kind: Tok,
    pub text: &'src str,
    pub line: u32,
}

pub fn lex(src: &str) -> Result<Vec<Token<'_>>, Error> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let (mut pos, mut line) = (0, 1u32);
    let take_while = |mut end: usize, pred: fn(u8) -> bool| {
        while end < bytes.len() && pred(bytes[end]) {
            end += 1;
        }
        end
    };

    while pos < bytes.len() {
        let c = bytes[pos];
        let (kind, end) = match c {
            b'#' => {
                pos = take_while(pos, |b| b != b'\n');
                continue;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let end = take_while(pos, |b| b.is_ascii_alphanumeric() || b == b'_');
                (keyword(&src[pos..end]).unwrap_or(Tok::Id), end)
            }
            b'0'..=b'9' => {
                let mut end = take_while(pos, |b| b.is_ascii_digit());
                // `1.5` is a float; `a[1].b` is still a member access
                if bytes.get(end) == Some(&b'.')
                    && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
                {
                    end = take_while(end + 1, |b| b.is_ascii_digit());
                }
                (Tok::Number, end)
            }
            b'"' => match bytes[pos + 1..]
                .iter()
                .position(|&b| b == b'"' || b == b'\n')
            {
                Some(i) if bytes[pos + 1 + i] == b'"' => (Tok::Str, pos + i + 2),
                _ => return Err(illegal(src, pos, line)),
            },
            b'=' if bytes.get(pos + 1) == Some(&b'=') => (Tok::Op(Op::Eq), pos + 2),
            b'&' if bytes.get(pos + 1) == Some(&b'&') => (Tok::Op(Op::And), pos + 2),
            b'|' if bytes.get(pos + 1) == Some(&b'|') => (Tok::Op(Op::Or), pos + 2),
            b'+' => (Tok::Op(Op::Add), pos + 1),
            b'-' => (Tok::Op(Op::Sub), pos + 1),
            b'*' => (Tok::Op(Op::Mul), pos + 1),
            b'/' => (Tok::Op(Op::Div), pos + 1),
            b'<' => (Tok::Op(Op::Lt), pos + 1),
            b'>' => (Tok::Op(Op::Gt), pos + 1),
            b'=' => (Tok::Eq, pos + 1),
            b'.' => (Tok::Dot, pos + 1),
            b':' => (Tok::Colon, pos + 1),
            b'?' => (Tok::QMark, pos + 1),
            b'~' => (Tok::Tilde, pos + 1),
            b'(' => (Tok::LParen, pos + 1),
            b')' => (Tok::RParen, pos + 1),
            b'{' => (Tok::LBrace, pos + 1),
            b'[' => (Tok::LBracket, pos + 1),
            b']' => (Tok::RBracket, pos + 1),
            b'}' => (Tok::RBrace, pos + 1),
            b';' => (Tok::Semi, pos + 1),
            b',' => (Tok::Comma, pos + 1),
            _ => {
                // Whitespace, which may be any Unicode space
                let ws = src[pos..]
                    .char_indices()
                    .find(|(_, ch)| !ch.is_whitespace());
                let end = ws.map_or(src.len(), |(i, _)| pos + i);
                if end == pos {
                    return Err(illegal(src, pos, line));
                }
                line += src[pos..end].bytes().filter(|&b| b == b'\n').count() as u32;
                pos = end;
                continue;
            }
        };
        tokens.push(Token {
            kind,
            text: &src[pos..end],
            line,
        });
        pos = end;
    }
    Ok(tokens)
}

fn illegal(src: &str, pos: usize, line: u32) -> Error {
    let snippet: String = src[pos..].chars().take(10).collect();
    Error::Syntax(format!(
        "Line {line}: Illegal char '{}...'",
        snippet.replace('\n', "\\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        lex(src).unwrap().iter().map(|t| t.kind).collect()
    }

    #[test]
    fn keywords_and_prefixed_identifiers() {
        assert_eq!(
            kinds("if iffy open opener"),
            [Tok::If, Tok::Id, Tok::Open, Tok::Id]
        );
    }

    #[test]
    fn operators_longest_first() {
        assert_eq!(
            kinds("a == b = c && d || e"),
            [
                Tok::Id,
                Tok::Op(Op::Eq),
                Tok::Id,
                Tok::Eq,
                Tok::Id,
                Tok::Op(Op::And),
                Tok::Id,
                Tok::Op(Op::Or),
                Tok::Id
            ]
        );
    }

    #[test]
    fn comments_strings_and_lines() {
        let toks = lex("# hi\n\"a b\" 12 # tail\n x").unwrap();
        assert_eq!(toks.len(), 3);
        assert_eq!(
            (toks[0].kind, toks[0].text, toks[0].line),
            (Tok::Str, "\"a b\"", 2)
        );
        assert_eq!((toks[2].text, toks[2].line), ("x", 3));
    }

    #[test]
    fn illegal_characters() {
        assert_eq!(
            lex("\nlet x = 1 & 2;").unwrap_err(),
            Error::Syntax("Line 2: Illegal char '& 2;...'".into())
        );
        assert!(lex("\"unterminated\n\"").is_err());
    }
}
