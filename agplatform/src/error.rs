/// Classifies errors by platform subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// An environment-related error.
    Env,
    /// A filesystem-related error.
    Fs,
}

/// A platform error carrying a human-readable description and an
/// [`ErrorKind`].
///
/// Constructed via [`Error::env`] or [`Error::fs`], which set the kind
/// automatically. Also converts from [`std::io::Error`] (as
/// [`ErrorKind::Fs`]).
///
/// # Examples
///
/// ```rust
/// use agplatform::Error;
///
/// let error = Error::fs("some error");
/// assert_eq!(format!("{error}"), "[Fs] some error");
/// assert_eq!(error.kind(), agplatform::ErrorKind::Fs);
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
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::fs("some error");
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

    /// Constructs a new [`Error`] of kind [`ErrorKind::Fs`] with the
    /// given `description` (converted to an owning `String`).
    pub fn fs<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Fs, description, None)
    }

    /// Returns the kind of the error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::fs("some error");
    /// assert_eq!(error.kind(), agplatform::ErrorKind::Fs);
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
            ErrorKind::Env => write!(f, "Env"),
            ErrorKind::Fs => write!(f, "Fs"),
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
/// [`ErrorKind::Fs`].
impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::new(
            ErrorKind::Fs,
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
        let error = Error::fs("some error");
        assert_eq!(format!("{error}"), "[Fs] some error");
    }

    #[test]
    fn std_io_error_conversion() {
        let io_error = std::io::Error::other("some io error");

        let error: Error = io_error.into();
        assert_eq!(error.kind(), ErrorKind::Fs);

        assert!(
            error.to_string().len() > "[Fs] ".len(),
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
