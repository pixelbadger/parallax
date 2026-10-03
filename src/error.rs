use std::fmt;

/// An error that stops an SPL program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Syntax(String),
    Runtime(String),
}

impl Error {
    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(msg) | Error::Runtime(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Runtime(format!("Output error: {e}"))
    }
}
