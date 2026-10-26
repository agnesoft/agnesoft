use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;

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
/// Changes are scoped to the platform — no global process state is
/// modified — and take effect for its fs, process and request: they
/// resolve relative paths against the current directory, and child
/// processes and the proxies of the default clients use the variables.
///
/// The environment can be changed through a shared reference, so a
/// platform can be shared between threads and tasks, e.g. in an
/// [`Arc`]. Like with [`std::env`](mod@std::env) a change while other
/// threads use the platform takes effect for their next operation.
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

    /// Returns the current directory.
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
    fn current_dir(&self) -> PathBuf;

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
    /// let platform = agplatform::platform();
    /// platform.env().set_var("TEST_VAR", "value");
    /// let removed_var = platform.env().remove_var("TEST_VAR");
    /// assert_eq!(removed_var, Some("value".to_string()));
    /// ```
    fn remove_var<T: AsRef<str>>(&self, key: T) -> Option<String>;

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
    /// let platform = agplatform::platform();
    /// let old_dir = platform.env().set_current_dir("sub/../new/path");
    /// assert_eq!(old_dir, std::env::current_dir().unwrap_or_default());
    /// let current_dir = platform.env().current_dir();
    /// assert_eq!(current_dir, old_dir.join("new/path"));
    /// ```
    fn set_current_dir<P: Into<PathBuf>>(&self, path: P) -> PathBuf;

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
    /// let platform = agplatform::platform();
    /// let old_var = platform.env().set_var("TEST_VAR", "value");
    /// assert_eq!(old_var, None);
    /// let var = platform.env().var("TEST_VAR");
    /// assert_eq!(var.as_deref(), Some("value"));
    /// ```
    fn set_var<T: Into<String>, U: Into<String>>(&self, key: T, value: U) -> Option<String>;

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
    fn var<T: AsRef<str>>(&self, key: T) -> Option<String>;

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
    /// let platform = agplatform::platform();
    /// let value = platform.env().var_bool("TEST_BOOL_MISSING").unwrap();
    /// assert!(!value);
    ///
    /// platform.env().set_var("TEST_BOOL_TRUE", "true");
    /// let value = platform.env().var_bool("TEST_BOOL_TRUE").unwrap();
    /// assert!(value);
    ///
    /// platform.env().set_var("TEST_BOOL_INVALID", "invalid");
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

    /// Returns an iterator over a snapshot of all environment variables as
    /// key-value pairs.
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
    fn vars(&self) -> EnvVars;
}

/// In-process [`Env`] implementation backed by owned data.
///
/// Values are loaded once from [`std::env`](mod@std::env) via [`new_from_std`](Self::new_from_std)
/// and then live in memory for the lifetime of the instance. Changes
/// (e.g. [`set_var`](Env::set_var), [`set_current_dir`](Env::set_current_dir))
/// affect only this instance and the platform it belongs to — the global
/// process environment is never modified.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
///
/// # Examples
///
/// ```rust
/// use agplatform::{Env, Platform};
///
/// let platform = agplatform::platform();
/// let current_dir = platform.env().current_dir();
/// assert_eq!(current_dir, std::env::current_dir().unwrap());
///
/// platform.env().set_var("TEST_VAR", "value");
/// let var = platform.env().var("TEST_VAR");
/// assert_eq!(var.as_deref(), Some("value"));
/// ```
pub struct EnvImpl {
    pub(crate) args: Vec<String>,
    pub(crate) shared: SharedEnv,
    pub(crate) current_exe: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) tmp_dir: PathBuf,
}

pub type EnvVars = std::vec::IntoIter<(String, String)>;

pub type EnvArgs<'a> = std::slice::Iter<'a, String>;

/// [internal]
///
/// The changeable part of an environment, shared by the env of a platform
/// with its fs, process and request so that they see its changes.
#[derive(Clone)]
pub(crate) struct SharedEnv(Arc<RwLock<EnvState>>);

/// [internal]
struct EnvState {
    vars: Vec<(String, String)>,
    /// The variables set (`Some`) or removed (`None`) since creation,
    /// applied on top of the inherited environment of child processes.
    changes: Vec<(String, Option<String>)>,
    current_dir: Arc<Path>,
    /// Incremented by each change of the variables.
    generation: u64,
}

impl EnvImpl {
    #[cfg(any(test, feature = "testing"))]
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            shared: SharedEnv::new(Vec::new(), PathBuf::new()),
            current_exe: PathBuf::new(),
            home_dir: PathBuf::new(),
            tmp_dir: PathBuf::new(),
        }
    }

    pub fn new_from_std() -> Self {
        Self {
            args: args_from(std::env::args_os()),
            shared: SharedEnv::new(
                std::env::vars_os()
                    .filter_map(|(k, v)| k.into_string().ok().zip(v.into_string().ok()))
                    .collect(),
                std::env::current_dir().unwrap_or_default(),
            ),
            current_exe: std::env::current_exe().unwrap_or_default(),
            home_dir: std::env::home_dir().unwrap_or_default(),
            tmp_dir: std::env::temp_dir(),
        }
    }
}

impl SharedEnv {
    pub(crate) fn new(vars: Vec<(String, String)>, current_dir: PathBuf) -> Self {
        Self(Arc::new(RwLock::new(EnvState {
            vars,
            changes: Vec::new(),
            current_dir: current_dir.into(),
            generation: 0,
        })))
    }

    pub(crate) fn current_dir(&self) -> Arc<Path> {
        self.read().current_dir.clone()
    }

    /// Replaces the current directory by `current_dir` as is.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn replace_current_dir(&self, current_dir: PathBuf) {
        self.write().current_dir = current_dir.into();
    }

    /// Returns the variables set or removed since creation.
    #[cfg(feature = "process")]
    pub(crate) fn changes(&self) -> Vec<(String, Option<String>)> {
        self.read().changes.clone()
    }

    /// Returns the result of `f` called with the generation of the
    /// variables, incremented by each change, and the variables.
    #[cfg(feature = "request")]
    pub(crate) fn with_vars<R>(&self, f: impl FnOnce(u64, &[(String, String)]) -> R) -> R {
        let state = self.read();
        f(state.generation, &state.vars)
    }

    pub(crate) fn var(&self, key: &str) -> Option<String> {
        self.read()
            .vars
            .iter()
            .find(|(k, _)| is_same_key(k, key))
            .map(|(_, v)| v.clone())
    }

    pub(crate) fn vars(&self) -> Vec<(String, String)> {
        self.read().vars.clone()
    }

    /// Sets the variables to `vars` without recording them as changes,
    /// e.g. to be inherited by child processes.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn replace_vars(&self, vars: Vec<(String, String)>) {
        let mut state = self.write();
        state.vars = vars;
        state.generation += 1;
    }

    pub(crate) fn set_var(&self, key: String, value: String) -> Option<String> {
        let mut state = self.write();
        record_change(&mut state.changes, key.clone(), Some(value.clone()));
        state.generation += 1;

        if let Some((_, v)) = state.vars.iter_mut().find(|(k, _)| is_same_key(k, &key)) {
            Some(std::mem::replace(v, value))
        } else {
            state.vars.push((key, value));
            None
        }
    }

    pub(crate) fn remove_var(&self, key: &str) -> Option<String> {
        let mut state = self.write();
        record_change(&mut state.changes, key.to_string(), None);
        state.generation += 1;

        state
            .vars
            .iter()
            .position(|(k, _)| is_same_key(k, key))
            .map(|pos| state.vars.remove(pos).1)
    }

    fn read(&self) -> RwLockReadGuard<'_, EnvState> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, EnvState> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }
}

fn args_from(args: impl Iterator<Item = OsString>) -> Vec<String> {
    args.map(|arg| arg.to_string_lossy().into_owned()).collect()
}

pub(crate) fn is_same_key(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    return a.eq_ignore_ascii_case(b);
    #[cfg(not(windows))]
    return a == b;
}

/// Records the change of the variable `key` to `value` (`None` when
/// removed) in `changes`, replacing an earlier one.
///
/// WHY: the keys are compared like the platform does, case-insensitively
/// on Windows.
pub(crate) fn record_change(
    changes: &mut Vec<(String, Option<String>)>,
    key: String,
    value: Option<String>,
) {
    changes.retain(|(k, _)| !is_same_key(k, &key));
    changes.push((key, value));
}

impl Env for EnvImpl {
    fn args(&self) -> EnvArgs<'_> {
        self.args.iter()
    }

    fn current_dir(&self) -> PathBuf {
        self.shared.current_dir().to_path_buf()
    }

    fn current_exe(&self) -> &Path {
        &self.current_exe
    }

    fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    fn remove_var<T: AsRef<str>>(&self, key: T) -> Option<String> {
        self.shared.remove_var(key.as_ref())
    }

    fn set_current_dir<P: Into<PathBuf>>(&self, path: P) -> PathBuf {
        let path = path.into();
        let mut state = self.shared.write();
        let current_dir = crate::utils::resolve_path(&state.current_dir, &path);
        std::mem::replace(&mut state.current_dir, current_dir.into()).to_path_buf()
    }

    fn set_var<T: Into<String>, U: Into<String>>(&self, key: T, value: U) -> Option<String> {
        self.shared.set_var(key.into(), value.into())
    }

    fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<String> {
        self.shared.var(key.as_ref())
    }

    fn vars(&self) -> EnvVars {
        self.shared.vars().into_iter()
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

    #[cfg(feature = "process")]
    mod shared_env {
        use super::*;

        mod set_var {
            use super::*;

            #[test]
            fn set_var_should_record_change_replacing_earlier_one() {
                let env = SharedEnv::new(Vec::new(), PathBuf::new());
                env.remove_var("A");

                env.set_var("A".to_string(), "2".to_string());
                env.set_var("B".to_string(), "3".to_string());

                assert_eq!(
                    env.changes(),
                    [
                        ("A".to_string(), Some("2".to_string())),
                        ("B".to_string(), Some("3".to_string()))
                    ]
                );
            }

            #[cfg(windows)]
            #[test]
            fn set_var_should_replace_earlier_change_ignoring_case() {
                let env = SharedEnv::new(Vec::new(), PathBuf::new());
                env.set_var("Path".to_string(), "1".to_string());

                env.set_var("PATH".to_string(), "2".to_string());

                assert_eq!(env.changes(), [("PATH".to_string(), Some("2".to_string()))]);
            }
        }

        mod remove_var {
            use super::*;

            #[test]
            fn remove_var_should_record_removal_replacing_earlier_change() {
                let env = SharedEnv::new(Vec::new(), PathBuf::new());
                env.set_var("A".to_string(), "1".to_string());

                env.remove_var("A");

                assert_eq!(env.changes(), [("A".to_string(), None)]);
            }
        }

        mod replace_vars {
            use super::*;

            #[test]
            fn replace_vars_should_not_record_changes() {
                let env = SharedEnv::new(Vec::new(), PathBuf::new());

                env.replace_vars(vec![("A".to_string(), "1".to_string())]);

                assert_eq!(env.var("A").as_deref(), Some("1"));
                assert_eq!(env.changes(), []);
            }
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
                    let env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.remove_var("KEY"), Some("value".to_string()));
                }

                #[test]
                fn remove_var_should_return_none_for_missing_key() {
                    let env = EnvImpl::new();
                    assert_eq!(env.remove_var("MISSING"), None);
                }

                #[cfg(windows)]
                #[test]
                fn remove_var_should_match_key_ignoring_ascii_case() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.remove_var("PATH"), Some("value".to_string()));
                    assert_eq!(env.vars().len(), 0);
                }

                #[cfg(unix)]
                #[test]
                fn remove_var_should_match_key_case_sensitively() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.remove_var("PATH"), None);
                    assert_eq!(env.var("Path").as_deref(), Some("value"));
                }
            }

            mod set_current_dir {
                use super::*;

                #[test]
                fn set_current_dir_should_return_previous_value() {
                    let env = EnvImpl::new_from_std();
                    let expected = std::env::current_dir().unwrap_or_default();
                    let previous = env.set_current_dir(std::env::temp_dir());
                    assert_eq!(previous, expected);
                }

                #[test]
                fn set_current_dir_should_update_current_directory() {
                    let env = EnvImpl::new_from_std();
                    let new_dir = std::env::temp_dir();
                    env.set_current_dir(new_dir.clone());
                    assert_eq!(env.current_dir(), new_dir);
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
                        let env = EnvImpl::new();
                        env.shared.replace_current_dir(base.clone());

                        env.set_current_dir(path);

                        assert_eq!(env.current_dir(), expected, "path: {path}");
                    }
                }

                #[test]
                fn set_current_dir_should_not_go_above_root() {
                    let env = EnvImpl::new();
                    env.shared.replace_current_dir(std::env::temp_dir());
                    let current_dir = env.current_dir();
                    let root = current_dir.ancestors().last().unwrap().to_path_buf();
                    let up = vec![".."; current_dir.components().count() + 1].join("/");

                    env.set_current_dir(up);

                    assert_eq!(env.current_dir(), root);
                }
            }

            mod set_var {
                use super::*;

                #[test]
                fn set_var_should_return_none_for_new_key() {
                    let env = EnvImpl::new();
                    assert_eq!(env.set_var("KEY", "value"), None);
                }

                #[test]
                fn set_var_should_return_previous_value_for_existing_key() {
                    let env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.set_var("KEY", "value2"), Some("value".to_string()));
                }

                #[cfg(windows)]
                #[test]
                fn set_var_different_case_should_replace_value_keeping_key() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");

                    let previous = env.set_var("PATH", "value2");

                    assert_eq!(previous, Some("value".to_string()));
                    assert_eq!(
                        env.vars().collect::<Vec<_>>(),
                        [("Path".to_string(), "value2".to_string())]
                    );
                }

                #[cfg(unix)]
                #[test]
                fn set_var_different_case_should_add_variable() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");

                    let previous = env.set_var("PATH", "value2");

                    assert_eq!(previous, None);
                    assert_eq!(
                        env.vars().collect::<Vec<_>>(),
                        [
                            ("Path".to_string(), "value".to_string()),
                            ("PATH".to_string(), "value2".to_string())
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
                    let env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(env.var("KEY").as_deref(), Some("value"));
                }

                #[cfg(windows)]
                #[test]
                fn var_should_match_key_ignoring_ascii_case() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.var("PATH").as_deref(), Some("value"));
                }

                #[cfg(unix)]
                #[test]
                fn var_should_match_key_case_sensitively() {
                    let env = EnvImpl::new();
                    env.set_var("Path", "value");
                    assert_eq!(env.var("PATH"), None);
                }
            }

            mod var_bool {
                use super::*;

                #[test]
                fn var_bool_should_accept_truthy_values() {
                    let env = EnvImpl::new();
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
                            env.var_bool(&key).unwrap(),
                            "Expected {key}={value} to be true",
                        );
                    }
                }

                #[test]
                fn var_bool_should_accept_falsy_values() {
                    let env = EnvImpl::new();
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
                            !env.var_bool(&key).unwrap(),
                            "Expected {key}={value} to be false",
                        );
                    }
                }

                #[test]
                fn var_bool_should_reject_invalid_values() {
                    let env = EnvImpl::new();
                    env.set_var("BOOL_INVALID_STRING", "invalid");
                    env.set_var("BOOL_INVALID_NUMBER", "2");
                    env.set_var("BOOL_INVALID_DOUBLY_QUOTED_SINGLE", "''true''");
                    env.set_var("BOOL_INVALID_DOUBLY_QUOTED_MIXED", "\"'false'\"");

                    for (key, value) in env.vars() {
                        assert!(
                            matches!(env.var_bool(&key), Err(e) if e.to_string().starts_with("[Env] Invalid boolean value for key")),
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
                    let env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    assert_eq!(
                        env.vars().find(|(k, _)| k == "KEY"),
                        Some(("KEY".to_string(), "value".to_string()))
                    );
                }

                #[test]
                fn vars_should_exclude_removed_variable() {
                    let env = EnvImpl::new();
                    env.set_var("KEY", "value");
                    env.set_var("KEY2", "value");
                    env.remove_var("KEY");
                    assert_eq!(env.vars().find(|(k, _)| k == "KEY"), None);
                }
            }
        }
    }
}
