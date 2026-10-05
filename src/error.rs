//! Errors that stop a program before or instead of running it.
//!
//! A failure inside one world (an assertion, an overflow) is not an `Error`:
//! it is a model error, counted against that policy and world in the report.

use serde::Serialize;
use std::fmt;

/// A position in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn to(self, other: Span) -> Span {
        Span {
            end: other.end.max(self.end),
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The source doesn't parse.
    Syntax,
    /// Types, units, names, information boundaries, recursion.
    Check,
    /// Host inputs are missing, unknown, mistyped or out of bounds.
    Input,
    /// The work can't be bounded, or exceeds the host's limits.
    Budget,
    /// Evaluating inputs, constants or a study's settings failed.
    Runtime,
    /// A served decision failed: an assertion, an overflow, an index out
    /// of range in the policy, its forecasts or the model.
    Model,
}

#[derive(Clone, Debug, Serialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            line: None,
            col: None,
        }
    }

    pub fn at(kind: ErrorKind, span: Span, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            line: Some(span.line),
            col: Some(span.col),
        }
    }

    pub fn line(kind: ErrorKind, line: u32, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            line: (line > 0).then_some(line),
            col: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ErrorKind::Syntax => "syntax error",
            ErrorKind::Check => "error",
            ErrorKind::Input => "input error",
            ErrorKind::Budget => "budget error",
            ErrorKind::Runtime => "runtime error",
            ErrorKind::Model => "model error",
        };
        match (self.line, self.col) {
            (Some(l), Some(c)) => write!(f, "{kind} at {l}:{c}: {}", self.message),
            (Some(l), None) => write!(f, "{kind} at line {l}: {}", self.message),
            _ => write!(f, "{kind}: {}", self.message),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
