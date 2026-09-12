/// The [`ErrorKind`] attributes an error to the particular part of the
/// platform such as env, exec, fs, or request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Env,
    IO,
}

/// The [`Error`] represents a platform error with a description and a
/// kind. It is constructed via dedicated constructors such as
/// [`Error::io`] that internally sets the correct [`ErrorKind`] for the
/// error. [`Error`] implements the [`std::fmt::Display`] and
/// [`std::error::Error`] traits.
///
/// Example:
/// ```rust
/// use agplatform::Error;
///
/// let error = Error::io("some error");
/// assert_eq!(format!("{error}"), "[IO] some error");
/// assert_eq!(error.kind(), agplatform::ErrorKind::IO);
/// ```
#[derive(Debug, Clone)]
pub struct Error {
    description: String,
    kind: ErrorKind,
    cause: Option<std::sync::Arc<dyn std::error::Error>>,
}

macro_rules! env_error {
    ($($arg:tt)*) => {
        $crate::Error::env(format!($($arg)*))
    };
}

impl Error {
    /// Returns the description of the error.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::io("some error");
    /// assert_eq!(error.description(), "some error");
    /// ```
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Constructs a new [`Error`] of kind [`ErrorKind::Env`] with the
    /// given `description` (converted to an owning `String`).
    pub fn env<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Env, description, None)
    }

    /// Constructs a new [`Error`] of kind [`ErrorKind::IO`] with the
    /// given `description` (converted to an owning `String`).
    pub fn io<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::IO, description, None)
    }

    /// Returns the kind of the error.
    ///    
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::io("some error");
    /// assert_eq!(error.kind(), agplatform::ErrorKind::IO);
    /// ```
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    fn new<T: std::fmt::Display>(
        kind: ErrorKind,
        description: T,
        cause: Option<std::sync::Arc<dyn std::error::Error>>,
    ) -> Self {
        Self {
            kind,
            description: description.to_string(),
            cause,
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ErrorKind::IO => write!(f, "IO"),
            ErrorKind::Env => write!(f, "Env"),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "[{}] {}", self.kind(), self.description())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref()
    }
}

/// Converts a [`std::io::Error`] into an [`Error`] of kind
/// [`ErrorKind::IO`].
impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::new(
            ErrorKind::IO,
            err.to_string(),
            Some(std::sync::Arc::new(err)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    #[test]
    fn env_macro_with_format_args() {
        let error = env_error!("{} {}", "some", "error");
        assert_eq!(error.kind(), ErrorKind::Env);
        assert_eq!(error.description(), "some error");
        assert_eq!(format!("{error}"), "[Env] some error");
    }

    #[test]
    fn display_with_kind() {
        let error = Error::io("some error");
        assert_eq!(format!("{error}"), "[IO] some error");
    }

    #[test]
    fn std_io_error_conversion() {
        let io_error = std::io::Error::other("some io error");

        let error: Error = io_error.into();
        assert_eq!(error.kind(), ErrorKind::IO);

        assert!(
            error.to_string().len() > "[IO] ".len(),
            "Error string representation with cause should contain more than just the prefix, got: '{error}'"
        );

        let orig_error = error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(orig_error.to_string(), "some io error");
    }
}
