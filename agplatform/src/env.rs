use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use crate::Result;

macro_rules! env_error {
    ($($arg:tt)*) => {
        $crate::Error::env(format!($($arg)*))
    };
}

/// Abstract environment interface.
///
/// Provides access to command-line arguments, environment variables,
/// and well-known directories (current, home, temp, executable).
/// Changes are scoped to the instance — no global process state is
/// modified.
///
/// The two provided implementations are `EnvImpl` (loaded once from
/// [`std::env`](mod@std::env)) and [`TestEnv`](crate::test_platform::test_env::TestEnv)
/// (purely in-memory, behind the `testing` feature).
pub trait Env: Send + Sync {
    /// Returns an iterator over the command-line arguments as strings.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// platform.env().args().for_each(|arg| {
    ///     println!("{}", arg);
    /// });
    /// ```
    fn args(&self) -> EnvArgs<'_>;

    /// Returns a reference to the current directory.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let current_dir = platform.env().current_dir();
    /// assert_eq!(current_dir, std::env::current_dir().unwrap());
    /// ```
    fn current_dir(&self) -> &Path;

    /// Returns a reference to the current executable.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let current_exe = platform.env().current_exe();
    /// assert_eq!(current_exe, std::env::current_exe().unwrap());
    /// ```
    fn current_exe(&self) -> &Path;

    /// Returns a reference to the home directory.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let home_dir = platform.env().home_dir();
    /// assert_eq!(home_dir, std::env::home_dir().unwrap_or_default());
    /// ```
    fn home_dir(&self) -> &Path;

    /// Removes the environment variable with the given key and returns
    /// its value if it existed. On Windows the key is matched ASCII case
    /// insensitively like in the Windows environment.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// platform.env_mut().set_var("TEST_VAR", "value");
    /// let removed_var = platform.env_mut().remove_var("TEST_VAR");
    /// assert_eq!(removed_var, Some("value".to_string()));
    /// ```
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String>;

    /// Sets the current directory and returns the previous value. A
    /// relative `path` is resolved against the previous current directory
    /// and the `.` and `..` components are removed lexically, never going
    /// above the root.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_dir = platform.env_mut().set_current_dir("sub/../new/path");
    /// assert_eq!(old_dir, std::env::current_dir().unwrap_or_default());
    /// let current_dir = platform.env().current_dir();
    /// assert_eq!(current_dir, old_dir.join("new/path"));
    /// ```
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf;

    /// Sets the environment variable with the given key and value,
    /// returning the previous value if it existed. On Windows the key is
    /// matched ASCII case insensitively like in the Windows environment
    /// and an existing variable keeps the spelling of its key.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_var = platform.env_mut().set_var("TEST_VAR", "value");
    /// assert_eq!(old_var, None);
    /// let var = platform.env().var("TEST_VAR");
    /// assert_eq!(var, Some("value"));
    /// ```
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String>;

    /// Returns a reference to the temporary directory.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let tmp_dir = platform.env().tmp_dir();
    /// assert_eq!(tmp_dir, std::env::temp_dir().as_path());
    /// ```
    fn tmp_dir(&self) -> &Path;

    /// Returns the value of the environment variable with the given
    /// key, if it exists. On Windows the key is matched ASCII case
    /// insensitively like in the Windows environment.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// if let Some(path) = platform.env().var("PATH") {
    ///     println!("PATH: {}", path);
    /// }
    /// ```
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str>;

    /// Returns the boolean value of the environment variable with the given key.
    /// Interprets case insensitively "true", "on", "1" as `true` and "false",
    /// "off", "0", empty, or missing as `false`. The value is trimmed and unquoted
    /// before evaluation as well. Returns an error if the value cannot be interpreted
    /// as a boolean.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The value is present but cannot be interpreted as a boolean
    ///   (e.g. `"invalid"`, `"2"`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let value = platform.env().var_bool("TEST_BOOL_MISSING").unwrap();
    /// assert!(!value);
    ///
    /// platform.env_mut().set_var("TEST_BOOL_TRUE", "true");
    /// let value = platform.env().var_bool("TEST_BOOL_TRUE").unwrap();
    /// assert!(value);
    ///
    /// platform.env_mut().set_var("TEST_BOOL_INVALID", "invalid");
    /// let result = platform.env().var_bool("TEST_BOOL_INVALID");
    /// assert!(result.is_err());
    /// ```
    fn var_bool<T: AsRef<str>>(&self, key: T) -> Result<bool> {
        match self.var(key.as_ref()) {
            Some(value) => match crate::utils::unquote(value.to_lowercase().as_str()) {
                "true" | "on" | "1" => Ok(true),
                "" | "false" | "off" | "0" => Ok(false),
                _ => Err(env_error!(
                    "Invalid boolean value for key '{}': '{}' (expected [any case]: true, on, 1 / false, off, 0, <empty>, <missing>)",
                    key.as_ref(),
                    value
                )),
            },
            None => Ok(false),
        }
    }

    /// Returns an iterator over all environment variables as key-value
    /// pairs.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// platform.env().vars().for_each(|(key, value)| {
    ///     println!("{}: {}", key, value);
    /// });
    /// ```
    fn vars(&self) -> EnvVars<'_>;
}

/// In-process [`Env`] implementation backed by owned data.
///
/// Values are loaded once from [`std::env`](mod@std::env) via [`new_from_std`](Self::new_from_std)
/// and then live in memory for the lifetime of the instance. Changes
/// (e.g. [`set_var`](Env::set_var), [`set_current_dir`](Env::set_current_dir))
/// affect only this instance — the global process environment is never
/// modified.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
///
/// # Examples
///
/// ```rust
/// use agplatform::{Env, Platform};
///
/// let mut platform = agplatform::platform();
/// let current_dir = platform.env().current_dir();
/// assert_eq!(current_dir, std::env::current_dir().unwrap());
///
/// platform.env_mut().set_var("TEST_VAR", "value");
/// let var = platform.env().var("TEST_VAR");
/// assert_eq!(var, Some("value"));
/// ```
pub struct EnvImpl {
    pub(crate) args: Vec<String>,
    pub(crate) vars: Vec<(String, String)>,
    pub(crate) current_dir: PathBuf,
    pub(crate) current_exe: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) tmp_dir: PathBuf,
}

/// Type alias for an iterator over environment variables as key-value pairs.
pub type EnvVars<'a> = std::slice::Iter<'a, (String, String)>;

/// Type alias for an iterator over command-line arguments.
pub type EnvArgs<'a> = std::slice::Iter<'a, String>;

impl EnvImpl {
    /// Creates a new instance of [`EnvImpl`] with an empty environment.
    #[cfg(any(test, feature = "testing"))]
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            vars: Vec::new(),
            current_dir: PathBuf::new(),
            current_exe: PathBuf::new(),
            home_dir: PathBuf::new(),
            tmp_dir: PathBuf::new(),
        }
    }

    /// Creates a new [`EnvImpl`] instance with the current process, executable,
    /// environment variables, program arguments, current directory, home directory,
    /// and temporary directory populated from [`std::env`](mod@std::env). Variables whose key or value
    /// is not valid UTF-8 are skipped. Arguments that are not valid UTF-8 are converted lossily, replacing
    /// invalid sequences with `U+FFFD`, so that the positions of all arguments are preserved. Inaccessible
    /// or missing current directory, missing current executable and home directory default to an empty
    /// path. The temporary directory is always set to the system's temporary directory.
    pub fn new_from_std() -> Self {
        Self {
            args: args_from(std::env::args_os()),
            vars: std::env::vars_os()
                .filter_map(|(k, v)| k.into_string().ok().zip(v.into_string().ok()))
                .collect(),
            current_dir: std::env::current_dir().unwrap_or_default(),
            current_exe: std::env::current_exe().unwrap_or_default(),
            home_dir: std::env::home_dir().unwrap_or_default(),
            tmp_dir: std::env::temp_dir(),
        }
    }
}

/// Converts the arguments to strings lossily, preserving their positions.
fn args_from(args: impl Iterator<Item = OsString>) -> Vec<String> {
    args.map(|arg| arg.to_string_lossy().into_owned()).collect()
}

/// Returns `true` if the variable keys are the same: equal on Unix and
/// ASCII case insensitively equal on Windows like in its environment.
fn is_same_key(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    return a.eq_ignore_ascii_case(b);
    #[cfg(not(windows))]
    return a == b;
}

impl Env for EnvImpl {
    fn args(&self) -> EnvArgs<'_> {
        self.args.iter()
    }

    fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    fn current_exe(&self) -> &Path {
        &self.current_exe
    }

    fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        let key = key.as_ref();

        if let Some(pos) = self.vars.iter().position(|(k, _)| is_same_key(k, key)) {
            Some(self.vars.remove(pos).1)
        } else {
            None
        }
    }

    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        let current_dir = crate::utils::resolve_path(&self.current_dir, &path.into());
        std::mem::replace(&mut self.current_dir, current_dir)
    }

    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
        let key = key.into();
        let value = value.into();

        if let Some((_, v)) = self.vars.iter_mut().find(|(k, _)| is_same_key(k, &key)) {
            Some(std::mem::replace(v, value))
        } else {
            self.vars.push((key, value));
            None
        }
    }

    fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        let key = key.as_ref();
        self.vars
            .iter()
            .find(|(k, _)| is_same_key(k, key))
            .map(|(_, v)| v.as_str())
    }

    fn vars(&self) -> EnvVars<'_> {
        self.vars.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod env_error {
        use crate::ErrorKind;

        #[test]
        fn env_error_should_format_arguments_into_env_error() {
            let error = env_error!("{} {}", "some", "error");
            assert_eq!(error.kind(), ErrorKind::Env);
            assert_eq!(error.description(), "some error");
            assert_eq!(format!("{error}"), "[Env] some error");
        }
    }

    mod args_from {
        use super::*;

        #[test]
        fn args_from_should_convert_non_utf8_arguments_lossily_in_place() {
            #[cfg(unix)]
            let invalid = {
                use std::os::unix::ffi::OsStringExt;
                OsString::from_vec(vec![b'a', 0xff])
            };
            #[cfg(windows)]
            let invalid = {
                use std::os::windows::ffi::OsStringExt;
                OsString::from_wide(&[u16::from(b'a'), 0xd800])
            };

            let args =
                args_from([OsString::from("app"), invalid, OsString::from("last")].into_iter());

            assert_eq!(args, ["app", "a\u{fffd}", "last"]);
        }
    }

    mod env_impl {
        use super::*;

        mod env {
            use super::*;

            mod args {
                use super::*;

                #[test]
                fn args_should_return_at_least_one_argument() {
                    let env = EnvImpl::new_from_std();
                    assert!(
                        env.args().len() > 0,
                        "Expected at least one command-line argument"
                    );
                }
            }

            mod current_dir {
                use super::*;

                #[test]
                fn current_dir_should_return_process_current_directory() {
                    let env = EnvImpl::new_from_std();
                    let expected = std::env::current_dir().unwrap_or_default();
                    assert_eq!(env.current_dir(), expected.as_path());
                }
            }

            mod current_exe {
                use super::*;

                #[test]
                fn current_exe_should_return_process_current_executable() {
                    let env = EnvImpl::new_from_std();
                    let expected = std::env::current_exe().unwrap_or_default();
                    assert_eq!(env.current_exe(), expected.as_path());
                }
            }

            mod home_dir {
                use super::*;

                #[test]
                fn home_dir_should_return_process_home_directory() {
                    let env = EnvImpl::new_from_std();
                    let expected = std::env::home_dir().unwrap_or_default();
                    assert_eq!(env.home_dir(), expected.as_path());
                }
            }

            mod remove_var {
                use super::*;

                #[test]
                fn remove_var_should_return_value_for_present_key() {
                    let mut env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.remove_var("KEY"), Some("value".to_string()));
                }

                #[test]
                fn remove_var_should_return_none_for_missing_key() {
                    let mut env = EnvImpl::new();
                    assert_eq!(env.remove_var("MISSING"), None);
                }

                #[cfg(windows)]
                #[test]
                fn remove_var_should_match_key_ignoring_ascii_case() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.remove_var("PATH"), Some("value".to_string()));
                    assert_eq!(env.vars().len(), 0);
                }

                #[cfg(unix)]
                #[test]
                fn remove_var_should_match_key_case_sensitively() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.remove_var("PATH"), None);
                    assert_eq!(env.var("Path"), Some("value"));
                }
            }

            mod set_current_dir {
                use super::*;

                #[test]
                fn set_current_dir_should_return_previous_value() {
                    let mut env = EnvImpl::new_from_std();
                    let expected = std::env::current_dir().unwrap_or_default();
                    let previous = env.set_current_dir(std::env::temp_dir());
                    assert_eq!(previous, expected);
                }

                #[test]
                fn set_current_dir_should_update_current_directory() {
                    let mut env = EnvImpl::new_from_std();
                    let new_dir = std::env::temp_dir();
                    env.set_current_dir(new_dir.clone());
                    assert_eq!(env.current_dir(), new_dir.as_path());
                }

                #[test]
                fn set_current_dir_relative_should_resolve_against_previous_current_dir() {
                    let base = std::env::temp_dir().join("a");

                    for (path, expected) in [
                        ("b", base.join("b")),
                        ("./b/.", base.join("b")),
                        ("b/../c", base.join("c")),
                        ("../b", std::env::temp_dir().join("b")),
                    ] {
                        let mut env = EnvImpl::new();
                        env.current_dir = base.clone();

                        env.set_current_dir(path);

                        assert_eq!(env.current_dir(), expected, "path: {path}");
                    }
                }

                #[test]
                fn set_current_dir_should_not_go_above_root() {
                    let mut env = EnvImpl::new();
                    env.current_dir = std::env::temp_dir();
                    let root = env.current_dir.ancestors().last().unwrap().to_path_buf();
                    let up = vec![".."; env.current_dir.components().count() + 1].join("/");

                    env.set_current_dir(up);

                    assert_eq!(env.current_dir(), root);
                }
            }

            mod set_var {
                use super::*;

                #[test]
                fn set_var_should_return_none_for_new_key() {
                    let mut env = EnvImpl::new();
                    assert_eq!(env.set_var("KEY", "value"), None);
                }

                #[test]
                fn set_var_should_return_previous_value_for_existing_key() {
                    let mut env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.set_var("KEY", "value2"), Some("value".to_string()));
                }

                #[cfg(windows)]
                #[test]
                fn set_var_different_case_should_replace_value_keeping_key() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");

                    let previous = env.set_var("PATH", "value2");

                    assert_eq!(previous, Some("value".to_string()));
                    assert_eq!(
                        env.vars().collect::<Vec<_>>(),
                        [&("Path".to_string(), "value2".to_string())]
                    );
                }

                #[cfg(unix)]
                #[test]
                fn set_var_different_case_should_add_variable() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");

                    let previous = env.set_var("PATH", "value2");

                    assert_eq!(previous, None);
                    assert_eq!(
                        env.vars().collect::<Vec<_>>(),
                        [
                            &("Path".to_string(), "value".to_string()),
                            &("PATH".to_string(), "value2".to_string())
                        ]
                    );
                }
            }

            mod tmp_dir {
                use super::*;

                #[test]
                fn tmp_dir_should_return_process_temp_directory() {
                    let env = EnvImpl::new_from_std();
                    let expected = std::env::temp_dir();
                    assert_eq!(env.tmp_dir(), expected.as_path());
                }
            }

            mod var {
                use super::*;

                #[test]
                fn var_should_return_none_for_missing_key() {
                    let env = EnvImpl::new();
                    assert_eq!(env.var("MISSING"), None);
                }

                #[test]
                fn var_should_return_value_for_present_key() {
                    let mut env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.var("KEY"), Some("value"));
                }

                #[cfg(windows)]
                #[test]
                fn var_should_match_key_ignoring_ascii_case() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.var("PATH"), Some("value"));
                }

                #[cfg(unix)]
                #[test]
                fn var_should_match_key_case_sensitively() {
                    let mut env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.var("PATH"), None);
                }
            }

            mod var_bool {
                use super::*;

                #[test]
                fn var_bool_should_accept_truthy_values() {
                    let mut env = EnvImpl::new();
                    env.set_var("BOOL_TRUE", "true");
                    env.set_var("BOOL_TRUE_QUOTED", "\'true\'");
                    env.set_var("BOOL_TRUE_DOUBLE_QUOTED", "\"true\"");
                    env.set_var("BOOL_TRUE_UPPER", "TRUE");
                    env.set_var("BOOL_TRUE_MIXED_CASE", "True");
                    env.set_var("BOOL_ON", "on");
                    env.set_var("BOOL_ON_QUOTED", "\'on\'");
                    env.set_var("BOOL_ON_DOUBLE_QUOTED", "\"on\"");
                    env.set_var("BOOL_ON_UPPER", "ON");
                    env.set_var("BOOL_ON_MIXED_CASE", "On");
                    env.set_var("BOOL_ONE", "1");
                    env.set_var("BOOL_ONE_QUOTED", "\'1\'");
                    env.set_var("BOOL_ONE_DOUBLE_QUOTED", "\"1\"");

                    for (key, value) in env.vars() {
                        assert!(
                            env.var_bool(key).unwrap(),
                            "Expected {key}={value} to be true",
                        );
                    }
                }

                #[test]
                fn var_bool_should_accept_falsy_values() {
                    let mut env = EnvImpl::new();
                    env.set_var("BOOL_FALSE", "false");
                    env.set_var("BOOL_FALSE_QUOTED", "\'false\'");
                    env.set_var("BOOL_FALSE_DOUBLE_QUOTED", "\"false\"");
                    env.set_var("BOOL_FALSE_UPPER", "FALSE");
                    env.set_var("BOOL_FALSE_MIXED", "FalsE");
                    env.set_var("BOOL_OFF", "off");
                    env.set_var("BOOL_OFF_QUOTED", "\'off\'");
                    env.set_var("BOOL_OFF_DOUBLE_QUOTED", "\"off\"");
                    env.set_var("BOOL_OFF_UPPER", "OFF");
                    env.set_var("BOOL_OFF_MIXED", "Off");
                    env.set_var("BOOL_ZERO", "0");
                    env.set_var("BOOL_ZERO_QUOTED", "\'0\'");
                    env.set_var("BOOL_ZERO_DOUBLE_QUOTED", "\"0\"");
                    env.set_var("BOOL_EMPTY", "");
                    env.set_var("BOOL_EMPTY_QUOTED", "\'\'");
                    env.set_var("BOOL_EMPTY_DOUBLE_QUOTED", "\"\"");

                    for (key, value) in env.vars() {
                        assert!(
                            !env.var_bool(key).unwrap(),
                            "Expected {key}={value} to be false",
                        );
                    }
                }

                #[test]
                fn var_bool_should_reject_invalid_values() {
                    let mut env = EnvImpl::new();
                    env.set_var("BOOL_INVALID_STRING", "invalid");
                    env.set_var("BOOL_INVALID_NUMBER", "2");
                    env.set_var("BOOL_INVALID_DOUBLY_QUOTED_SINGLE", "''true''");
                    env.set_var("BOOL_INVALID_DOUBLY_QUOTED_MIXED", "\"'false'\"");

                    for (key, value) in env.vars() {
                        assert!(
                            matches!(env.var_bool(key), Err(e) if e.to_string().starts_with("[Env] Invalid boolean value for key")),
                            "Expected {key}={value} to be invalid"
                        );
                    }
                }

                #[test]
                fn var_bool_should_return_false_for_missing_key() {
                    let env = EnvImpl::new();
                    // WHY: the key is &String to prevent the generic var_bool being instantiated
                    // for &str and seemingly missing coverage due to monomorphization.
                    let missing = &"NON_EXISTENT_ENV_VAR".to_string();
                    assert!(
                        !env.var_bool(missing).unwrap(),
                        "Expected var_bool() on non-existent var to be false"
                    );
                }
            }

            mod vars {
                use super::*;

                #[test]
                fn vars_should_include_set_variable() {
                    let mut env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(
                        env.vars().find(|(k, _)| k == "KEY"),
                        Some(&("KEY".to_string(), "value".to_string()))
                    );
                }

                #[test]
                fn vars_should_exclude_removed_variable() {
                    let mut env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    env.set_var("KEY2", "value");
                    env.remove_var("KEY");
                    assert_eq!(env.vars().find(|(k, _)| k == "KEY"), None);
                }
            }
        }
    }
}
