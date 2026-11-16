//! The one error type used across the engine.

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// The user cancelled the operation.
    Cancelled,
    /// A safety rule refused the device or the request.
    Refused(String),
    /// The image or on-disk structure is malformed or unsupported.
    Invalid(String),
    /// An I/O error, with a short description of what was being done.
    Io(String, io::Error),
    /// Anything else, already phrased for the user.
    Msg(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io(context.into(), source)
    }
    pub fn invalid(text: impl Into<String>) -> Error {
        Error::Invalid(text.into())
    }
    pub fn msg(text: impl Into<String>) -> Error {
        Error::Msg(text.into())
    }
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cancelled => write!(f, "Cancelled."),
            Error::Refused(t) => write!(f, "Refused: {t}"),
            Error::Invalid(t) => write!(f, "{t}"),
            Error::Io(context, e) => write!(f, "{context}: {e}"),
            Error::Msg(t) => write!(f, "{t}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io("I/O error".into(), e)
    }
}

/// Adds a description to an `io::Result`.
pub trait IoContext<T> {
    fn ctx(self, context: &str) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: &str) -> Result<T> {
        self.map_err(|e| Error::Io(context.to_string(), e))
    }
}
