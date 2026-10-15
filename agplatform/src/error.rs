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
/// automatically.
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
        Self::new(ErrorKind::Env, description)
    }

    /// Constructs a new [`Error`] of kind [`ErrorKind::Fs`] with the
    /// given `description` (converted to an owning `String`).
    pub fn fs<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Fs, description)
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

    fn new<T: std::fmt::Display>(kind: ErrorKind, description: T) -> Self {
        Self {
            kind,
            description: description.to_string(),
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

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    mod error_kind {
        use super::*;

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_display_variant() {
                for (variant, displayed) in &[(ErrorKind::Env, "Env"), (ErrorKind::Fs, "Fs")] {
                    assert_eq!(format!("{}", variant), *displayed);
                }
            }
        }
    }

    mod error {
        use super::*;

        mod description {
            use super::*;

            #[test]
            fn description_should_return_error_message() {
                let error = Error::fs("some error");
                assert_eq!(error.description(), "some error");
            }
        }

        mod env {
            use super::*;

            #[test]
            fn env_should_create_error_with_env_kind() {
                let error = Error::env("env error");
                assert_eq!(error.kind(), ErrorKind::Env);
                assert_eq!(error.description(), "env error");
            }
        }

        mod fs {
            use super::*;

            #[test]
            fn fs_should_create_error_with_fs_kind() {
                let error = Error::fs("fs error");
                assert_eq!(error.kind(), ErrorKind::Fs);
                assert_eq!(error.description(), "fs error");
            }
        }

        mod kind {
            use super::*;

            #[test]
            fn kind_should_return_error_kind() {
                assert_eq!(Error::env("e").kind(), ErrorKind::Env);
                assert_eq!(Error::fs("e").kind(), ErrorKind::Fs);
            }
        }

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_include_kind_prefix_and_description() {
                let error = Error::fs("some error");
                assert_eq!(format!("{error}"), "[Fs] some error");
            }
        }
    }

    mod env_error {
        use super::*;

        #[test]
        fn env_error_should_format_arguments_into_env_error() {
            let error = env_error!("{} {}", "some", "error");
            assert_eq!(error.kind(), ErrorKind::Env);
            assert_eq!(error.description(), "some error");
            assert_eq!(format!("{error}"), "[Env] some error");
        }
    }
}
