use std::sync::Arc;

/// Classifies errors by platform subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// An environment-related error.
    Env,
    /// A filesystem-related error.
    Fs,
    /// A process-related error.
    Process,
    /// A network request-related error.
    Request,
}

/// A platform error carrying a human-readable description and an
/// [`ErrorKind`].
///
/// Constructed via [`Error::env`], [`Error::fs`], [`Error::process`] or
/// [`Error::request`], which set the kind automatically. An error caused by another one, e.g. a failed
/// [`std::io`] operation, returns it from
/// [`source`](std::error::Error::source).
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
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
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

    /// Returns the output of the child process whose failure caused the
    /// error, see [`Process::run`](crate::Process::run).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// process.set_output("git", CommandOutput { status: Some(1), stderr: b"fatal".to_vec(), ..Default::default() });
    ///
    /// let error = Command::new("git").run(&process).unwrap_err();
    ///
    /// assert_eq!(error.command_output().unwrap().stderr_str().unwrap(), "fatal");
    /// ```
    pub fn command_output(&self) -> Option<&crate::CommandOutput> {
        self.source_as()
    }

    /// Enabled by the `request` feature flag.
    ///
    /// Returns the response whose status caused the error, see
    /// [`Request::send_blocking`](crate::Request::send_blocking).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// request.set_response("", RequestOutput { status: 404, body: b"not found".to_vec(), ..Default::default() });
    ///
    /// let client = request.blocking_client().unwrap();
    /// let error = client.get("https://example.com").send_blocking(&request).unwrap_err();
    ///
    /// assert_eq!(error.request_output().unwrap().text().unwrap(), "not found");
    /// ```
    #[cfg(feature = "request")]
    pub fn request_output(&self) -> Option<&crate::RequestOutput> {
        self.source_as()
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

    /// Constructs a new [`Error`] of kind [`ErrorKind::Process`] with the
    /// given `description` (converted to an owning `String`).
    pub fn process<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Process, description)
    }

    /// Constructs a new [`Error`] of kind [`ErrorKind::Request`] with the
    /// given `description` (converted to an owning `String`).
    pub fn request<T: std::fmt::Display>(description: T) -> Self {
        Self::new(ErrorKind::Request, description)
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

    /// Returns the error that caused this one if it is a `T`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// process.set_output("git", CommandOutput { status: Some(1), ..Default::default() });
    /// let error = Command::new("git").run(&process).unwrap_err();
    ///
    /// assert_eq!(error.source_as::<CommandOutput>().unwrap().status, Some(1));
    /// assert!(error.source_as::<std::io::Error>().is_none());
    /// ```
    pub fn source_as<T: std::error::Error + 'static>(&self) -> Option<&T> {
        self.source
            .as_deref()
            .and_then(|source| source.downcast_ref::<T>())
    }

    /// [internal]
    ///
    /// Records `source` as the error that caused this one.
    pub(crate) fn with_source<E: std::error::Error + Send + Sync + 'static>(
        mut self,
        source: E,
    ) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    fn new<T: std::fmt::Display>(kind: ErrorKind, description: T) -> Self {
        Self {
            kind,
            description: description.to_string(),
            source: None,
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ErrorKind::Env => write!(f, "Env"),
            ErrorKind::Fs => write!(f, "Fs"),
            ErrorKind::Process => write!(f, "Process"),
            ErrorKind::Request => write!(f, "Request"),
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
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod error_kind {
        use super::*;

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_display_variant() {
                for (variant, displayed) in &[
                    (ErrorKind::Env, "Env"),
                    (ErrorKind::Fs, "Fs"),
                    (ErrorKind::Process, "Process"),
                    (ErrorKind::Request, "Request"),
                ] {
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

        mod command_output {
            use super::*;

            #[test]
            fn command_output_should_return_output_source() {
                let output = crate::CommandOutput {
                    status: Some(1),
                    ..Default::default()
                };
                let error = Error::process("failed").with_source(output.clone());

                assert_eq!(error.command_output(), Some(&output));
            }

            #[test]
            fn command_output_without_output_source_should_return_none() {
                let io = Error::process("failed").with_source(std::io::Error::other("io error"));

                assert_eq!(io.command_output(), None);
                assert_eq!(Error::process("failed").command_output(), None);
            }
        }

        #[cfg(feature = "request")]
        mod request_output {
            use super::*;

            #[test]
            fn request_output_should_return_output_source() {
                let output = crate::RequestOutput {
                    status: 404,
                    ..Default::default()
                };
                let error = Error::request("failed").with_source(output.clone());

                assert_eq!(error.request_output(), Some(&output));
            }

            #[test]
            fn request_output_without_output_source_should_return_none() {
                assert_eq!(Error::request("failed").request_output(), None);
            }
        }

        mod source_as {
            use super::*;

            #[test]
            fn source_as_should_return_source_of_type() {
                let error = Error::process("failed").with_source(std::io::Error::other("io error"));

                assert_eq!(
                    error.source_as::<std::io::Error>().map(|e| e.to_string()),
                    Some("io error".to_string())
                );
            }

            #[test]
            fn source_as_without_source_should_return_none() {
                assert!(
                    Error::process("failed")
                        .source_as::<std::io::Error>()
                        .is_none()
                );
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

        mod process {
            use super::*;

            #[test]
            fn process_should_create_error_with_process_kind() {
                let error = Error::process("process error");
                assert_eq!(error.kind(), ErrorKind::Process);
                assert_eq!(error.description(), "process error");
            }
        }

        mod request {
            use super::*;

            #[test]
            fn request_should_create_error_with_request_kind() {
                let error = Error::request("request error");
                assert_eq!(error.kind(), ErrorKind::Request);
                assert_eq!(error.description(), "request error");
            }
        }

        mod kind {
            use super::*;

            #[test]
            fn kind_should_return_error_kind() {
                assert_eq!(Error::env("e").kind(), ErrorKind::Env);
                assert_eq!(Error::fs("e").kind(), ErrorKind::Fs);
                assert_eq!(Error::process("e").kind(), ErrorKind::Process);
                assert_eq!(Error::request("e").kind(), ErrorKind::Request);
            }
        }

        mod with_source {
            use super::*;

            #[test]
            fn with_source_should_keep_kind_and_description() {
                let error = Error::fs("fs error").with_source(std::io::Error::other("io error"));
                assert_eq!(error.kind(), ErrorKind::Fs);
                assert_eq!(error.description(), "fs error");
            }
        }

        mod std_error_error {
            use super::*;

            mod source {
                use super::*;
                use std::error::Error as _;

                #[test]
                fn source_should_return_recorded_error() {
                    let error = Error::fs("fs error")
                        .with_source(std::io::Error::from(std::io::ErrorKind::NotFound));

                    assert_eq!(
                        error
                            .source()
                            .and_then(|source| source.downcast_ref::<std::io::Error>())
                            .map(std::io::Error::kind),
                        Some(std::io::ErrorKind::NotFound)
                    );
                }

                #[test]
                fn source_without_recorded_error_should_return_none() {
                    assert!(Error::fs("fs error").source().is_none());
                }
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
