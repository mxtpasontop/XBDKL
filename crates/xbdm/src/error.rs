use std::io;
use std::path::PathBuf;

/// Errors returned by the XBDM client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The console answered with a 4xx status line.
    #[error("console error {code}: {message}")]
    Console { code: u16, message: String },

    /// The console sent something we did not expect.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A local file could not be read or written.
    #[error("{}: {source}", path.display())]
    Local { path: PathBuf, source: io::Error },

    /// A caller-supplied argument cannot be sent safely (e.g. a path containing a quote).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl Error {
    /// Status code if this is a console-reported error.
    pub fn code(&self) -> Option<u16> {
        match self {
            Error::Console { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.code() == Some(402)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
