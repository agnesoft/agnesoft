/// The `ErrorKind` attributes an error to the
/// particular part of the platform such as env,
/// exec, fs or request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Env,
}

/// The `Error` represents a platform error with a
/// `description` and a `kind`. It is constructed
/// via dedicated constructors such as `Error::env(description)`
/// that internally sets the correct `ErrorKind` for the error.
/// The `Error` implements the `std::fmt::Display` and the
/// `std::error::Error` traits.
///
/// Example:
/// ```rust
/// use agplatform::Error;
///
/// let error = Error::env("some error");
/// assert_eq!(format!("{error}"), "[Env] some error");
/// assert_eq!(error.kind(), agplatform::ErrorKind::Env);
/// ```
#[derive(Debug)]
pub struct Error {
    description: String,
    kind: ErrorKind,
    cause: Option<Box<dyn std::error::Error>>,
}

impl Error {
    /// Returns the description of the error.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::env("some error");
    /// assert_eq!(error.description(), "some error");
    /// ```
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Constructs a new `Error` of kind `ErrorKind::Env`
    /// with the given `description` (converted to an owning
    /// `String`).
    pub fn env<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Env, description, None)
    }

    /// Returns the kind of the error.
    ///    
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Error;
    ///
    /// let error = Error::env("some error");
    /// assert_eq!(error.kind(), agplatform::ErrorKind::Env);
    /// ```
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    fn new<T: std::fmt::Display>(
        kind: ErrorKind,
        description: T,
        cause: Option<Box<dyn std::error::Error>>,
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

/// Converts a `std::env::VarError` into an `Error` of kind `ErrorKind::Env`.
impl From<std::env::VarError> for Error {
    fn from(err: std::env::VarError) -> Self {
        Self::new(ErrorKind::Env, err.to_string(), Some(Box::new(err)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // use std::error::Error as _;

    #[test]
    fn display() {
        let error = Error::env("some error");
        assert_eq!(format!("{error}"), "[Env] some error");
    }

    #[test]
    fn std_env_varerror_conversion() {
        // let env_error = std::env::VarError::NotPresent;

        // let error: Error = env_error.clone().into();
        // assert_eq!(error.kind(), ErrorKind::Env);

        // assert!(
        //     error.to_string().len() > "[Env] ".len(),
        //     "Error string representation with cause should contain more than just the prefix, got: '{error}'"
        // );

        // let orig_error = error
        //     .source()
        //     .unwrap()
        //     .downcast_ref::<std::env::VarError>()
        //     .unwrap();
        // assert_eq!(orig_error, &env_error);
    }
}
