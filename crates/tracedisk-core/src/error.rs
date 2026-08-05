use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, TraceError>;

#[derive(Debug)]
pub enum TraceError {
    Io(io::Error),
    InvalidData(String),
    OutOfBounds {
        offset: u64,
        length: usize,
        source_len: u64,
    },
    Unsupported(String),
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidData(message) => write!(f, "invalid data: {message}"),
            Self::OutOfBounds {
                offset,
                length,
                source_len,
            } => write!(
                f,
                "read outside source: offset={offset}, length={length}, source_length={source_len}"
            ),
            Self::Unsupported(message) => write!(f, "unsupported: {message}"),
        }
    }
}

impl std::error::Error for TraceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TraceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
