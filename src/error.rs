//! What a call here answers with when the bytes are not what they claim.

use std::fmt;
use std::io;

/// Why a call failed, for a caller that acts on the kinds differently. The
/// message says what happened; this says what sort of thing it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The bytes are not NUT, or not the NUT they say they are: a checksum
    /// that does not match, a field that runs off the end of its packet, a
    /// frame header byte the main header never defined.
    Format,
    /// Well-formed NUT this crate refuses by name rather than guessing at:
    /// version 4, a stream class it has no geometry for, side data.
    Unsupported,
    /// A bound was crossed: one of [`Limits`](crate::Limits), or one of the
    /// fixed ones this crate documents.
    Limit,
    /// The reader or writer underneath failed. The `io::Error` is the source.
    Io,
}

/// An error from reading or writing NUT.
///
/// It is `std::error::Error`, `Send` and `Sync`, so `?` carries it into an
/// `anyhow::Result` and it crosses a channel or sits in a mutex the way an
/// `anyhow::Error` does.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<io::Error>,
}

impl Error {
    /// What sort of failure this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub(crate) fn format(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Format, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Unsupported, message)
    }

    pub(crate) fn limit(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Limit, message)
    }

    fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            source: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e as &dyn std::error::Error)
    }
}

impl From<io::Error> for Error {
    /// An io failure, worded the way the io error words itself, so what a
    /// caller prints does not change for having come through here.
    fn from(error: io::Error) -> Error {
        Error {
            kind: ErrorKind::Io,
            message: error.to_string(),
            source: Some(error),
        }
    }
}

/// The result of anything here that can fail.
pub type Result<T> = std::result::Result<T, Error>;

/// `return Err(...)`, in the kind named first: `bail!(format: "...")`.
macro_rules! bail {
    ($kind:ident: $($arg:tt)*) => {
        return ::std::result::Result::Err(
            $crate::error::Error::$kind(format!($($arg)*)).into(),
        )
    };
}

pub(crate) use bail;
