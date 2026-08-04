use crate::Result;

/// Opaque iterator over the environment variables.
pub struct EnvVars(pub(crate) EnvVarsInner);

pub(crate) enum EnvVarsInner {
    Std(std::env::VarsOs),
    #[cfg(feature = "testing")]
    Owned(std::collections::btree_map::IntoIter<String, String>),
}

impl From<std::env::VarsOs> for EnvVarsInner {
    fn from(vars: std::env::VarsOs) -> Self {
        Self::Std(vars)
    }
}

#[cfg(feature = "testing")]
impl From<std::collections::btree_map::IntoIter<String, String>> for EnvVarsInner {
    fn from(vars: std::collections::btree_map::IntoIter<String, String>) -> Self {
        Self::Owned(vars)
    }
}

impl Iterator for EnvVars {
    type Item = (String, String);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            EnvVarsInner::Std(vars) => vars.next().map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            }),
            #[cfg(feature = "testing")]
            EnvVarsInner::Owned(vars) => vars.next(),
        }
    }
}

/// A trait that represents the environment in
/// which the program runs in. It offers read/write
/// operations of the environment variables
/// and the current working directory.
///
/// The default implementation of the `Env` trait
/// uses the `std::env` implementations. The entire
/// program using it should access the environment
/// exclusively via the single instance of an object
/// implementing the `Env` trait to prevent races and
/// to allow for mocking in tests (i.e. via the object
/// returned from `agplatform::platform()`).
///
/// Example:
///
/// ```rust
/// use agplatform::{Platform, Env};
///
/// let platform = agplatform::platform();
/// let value = platform.env().var("PATH");
/// assert!(value.is_some());
/// let current_dir = platform.env().current_dir();
/// assert!(current_dir.is_ok());
/// ```
pub trait Env {
    /// Returns the current working directory as a `PathBuf`.
    /// If the current working directory cannot be retrieved
    /// an `Error` of kind `ErrorKind::IO` is returned. This
    /// can happen when the current working directory has been
    /// deleted or the user does not have permissions to access
    /// it.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// let current_dir = platform.env().current_dir();
    /// assert!(current_dir.is_ok());
    /// ```
    fn current_dir(&self) -> Result<std::path::PathBuf>;

    /// Removes the environment variable with the given `key`.
    /// If the key exists, the previous value is returned as
    /// `Some(String)`. If the key does not exist `None` is returned.
    /// If the value is non-unicode it is coerced to unicode `String`
    /// via `to_string_lossy()` and returned as `Some(String)`.
    ///
    /// SAFETY: This functions uses unsafe around `std::env::remove_var`.
    /// To avoid the race condition make sure you are accessing
    /// the environment exclusively via a single instance of the `Env`
    /// trait implementation throughout your program (i.e. via the
    /// object returned from `agplatform::platform()`).
    ///
    /// Example:
    ///
    /// ```ignore
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// platform.env_mut().set_var("TEST_ENV_VAR", "value");
    /// let old_value = platform.env_mut().remove_var("TEST_ENV_VAR");
    /// assert_eq!(old_value, Some("value".to_string()));
    /// ```
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String>;

    /// Sets the current working directory to the given `path`.
    /// If the current working directory cannot be set an `Error`
    /// of kind `ErrorKind::IO` is returned. This can happen when
    /// the given path does not exist or the user does not have
    /// permissions to access it. The return value is the previous
    /// current working directory as a `PathBuf` if the operation
    /// was successful or default `PathBuf` if the previous current
    /// working directory could not be retrieved (i.e. it was deleted
    /// or the user does not have permissions to access it).
    ///
    /// Example:
    ///
    /// ```ignore
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// platform.env_mut().set_current_dir("/some/path").unwrap();
    /// let current_dir = platform.env().current_dir().unwrap();
    /// assert_eq!(current_dir, std::path::PathBuf::from("/some/path"));
    /// ```
    fn set_current_dir<P: AsRef<std::path::Path>>(&mut self, path: P)
    -> Result<std::path::PathBuf>;

    /// Sets the environment variable with the given `key`
    /// to the given `value`. If the key already exists,
    /// the previous value is returned as `Some(String)`. If the key
    /// does not exist `None` is returned. If the previous value
    /// is non-unicode it is coerced to unicode `String` via `to_string_lossy()`
    /// and returned as `Some(String)`.
    ///
    /// SAFETY: This functions uses unsafe around `std::env::set_var`.
    /// To avoid the race condition make sure you are accessing
    /// the environment exclusively via a single instance of the `Env`
    /// trait implementation throughout your program (i.e. via the
    /// object returned from `agplatform::platform()`).
    ///
    /// Example:
    ///
    /// ```ignore
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// let old_value = platform.env_mut().set_var("TEST_ENV_VAR", "value");
    /// assert_eq!(old_value, None);
    /// let old_value = platform.env_mut().set_var("TEST_ENV_VAR", "value2");
    /// assert_eq!(old_value, Some("value".to_string()));
    /// ```
    fn set_var<T: AsRef<str>, U: AsRef<str>>(&mut self, key: T, value: U) -> Option<String>;

    /// Returns the value of the environment variable
    /// with the given `key` as `Option<String>`. If the
    /// key does not exist, `None` is returned. If the
    /// key exists but is not valid Unicode it is coerced
    /// to unicode `String` via `to_string_lossy()`.
    ///
    /// SAFETY: This function does not use unsafe however to
    /// avoid the race condition when accessing and possibly mutating
    /// the environment make sure you are accessing the environment
    /// exclusively via a single instance of the `Env` trait
    /// implementation throughout your program (i.e. via the
    /// object returned from `agplatform::platform()`).
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// let value = platform.env().var("PATH");
    /// assert!(value.is_some());
    ///
    /// let value = platform.env().var("TEST_ENV_VAR");
    /// assert_eq!(value, None);
    /// ```
    fn var<T: AsRef<str>>(&self, key: T) -> Option<String>;

    /// Returns an iterator over the environment variables
    /// as `(String, String)` tuples. The iterator is opaque
    /// and can be used in both the std backed as well as the
    /// mock implementation of the `Env` trait. The non-unicode
    /// keys and values are coerced to unicode `String` via
    /// `to_string_lossy()`.
    ///
    /// SAFETY: This function does not use unsafe however to
    /// avoid the race condition when accessing and possibly mutating
    /// the environment make sure you are accessing the environment
    /// exclusively via a single instance of the `Env` trait
    /// implementation throughout your program (i.e. via the
    /// object returned from `agplatform::platform()`).
    /// Example:
    ///
    /// ```rust
    ///  use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// for (key, value) in platform.env().vars() {
    ///     println!("{}={}", key, value);
    /// }
    /// ```
    fn vars(&self) -> EnvVars;
}

pub(crate) struct EnvImpl;

impl Env for EnvImpl {
    fn current_dir(&self) -> Result<std::path::PathBuf> {
        std::env::current_dir().map_err(From::from)
    }

    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        let old_value = self.var(key.as_ref());

        unsafe {
            std::env::remove_var(key.as_ref());
        }

        old_value
    }

    fn set_current_dir<P: AsRef<std::path::Path>>(
        &mut self,
        path: P,
    ) -> Result<std::path::PathBuf> {
        std::env::set_current_dir(path)
            .map(|_| self.current_dir().unwrap_or_default())
            .map_err(From::from)
    }

    fn set_var<T: AsRef<str>, U: AsRef<str>>(&mut self, key: T, value: U) -> Option<String> {
        let old_value = self.var(key.as_ref());

        unsafe {
            std::env::set_var(key.as_ref(), value.as_ref());
        }

        old_value
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<String> {
        std::env::var_os(key.as_ref()).map(|k| k.to_string_lossy().to_string())
    }

    fn vars(&self) -> EnvVars {
        EnvVars(std::env::vars_os().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        env: EnvImpl,
    }

    impl EnvGuard {
        fn new(key: &'static str) -> Self {
            let mut env = EnvImpl;
            let _ = env.remove_var(key);
            Self { key, env }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let _ = self.env.remove_var(self.key);
        }
    }

    struct CurrentDirGuard {
        original_dir: std::path::PathBuf,
        env: EnvImpl,
    }

    impl CurrentDirGuard {
        fn new() -> Self {
            let env = EnvImpl;
            let original_dir = env.current_dir().expect("Failed to get current dir");
            Self { original_dir, env }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = self.env.set_current_dir(&self.original_dir);
        }
    }

    #[test]
    fn current_dir() {
        let mut guard = CurrentDirGuard::new();

        // Get the current directory
        let current_dir = guard.env.current_dir().expect("Failed to get current dir");
        assert_eq!(current_dir, guard.original_dir);

        // Change to a new directory
        let new_dir = std::env::temp_dir();
        guard
            .env
            .set_current_dir(&new_dir)
            .expect("Failed to set current dir");

        // Verify the current directory has changed
        let current_dir = guard.env.current_dir().expect("Failed to get current dir");
        assert_eq!(current_dir, new_dir);
    }

    #[test]
    fn vars() {
        const KEY: &str = "THIS_ENV_VAR_SHOULD_NOT_EXIST";
        let mut guard = EnvGuard::new(KEY);

        // Get missing env var
        let missing = guard.env.var(KEY);
        assert_eq!(missing, None);

        // Set env var
        let old = guard.env.set_var(KEY, "abc");
        assert_eq!(old, None);

        // Get previously set env var
        let value = guard.env.var(KEY);
        assert_eq!(value, Some("abc".to_string()));
        assert_eq!(
            guard.env.vars().find(|(k, _)| k == KEY),
            Some((KEY.to_string(), "abc".to_string()))
        );

        // Overwrite env var
        let old = guard.env.set_var(KEY, "def");
        assert_eq!(old, Some("abc".to_string()));
        let value = guard.env.var(KEY);
        assert_eq!(value, Some("def".to_string()));
        assert_eq!(
            guard.env.vars().find(|(k, _)| k == KEY),
            Some((KEY.to_string(), "def".to_string()))
        );

        // Remove env var
        let old = guard.env.remove_var(KEY);
        assert_eq!(old, Some("def".to_string()));

        // Get missing env var again
        let missing = guard.env.var(KEY);
        assert_eq!(missing, None);

        // Remove missing env var
        let missing = guard.env.remove_var(KEY);
        assert_eq!(missing, None);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn non_unicode_var() {
        const KEY: &str = "THIS_INVALID_UNICODE_ENV_VAR_SHOULD_NOT_EXIST";
        let mut guard = EnvGuard::new(KEY);

        #[cfg(unix)]
        let non_unicode_value: std::ffi::OsString =
            std::os::unix::ffi::OsStringExt::from_vec(vec![b'a', 0x80, b'b']);

        #[cfg(windows)]
        let non_unicode_value: std::ffi::OsString =
            std::os::windows::ffi::OsStringExt::from_wide(&[b'a' as u16, 0xD800, b'b' as u16]);

        unsafe {
            std::env::set_var(KEY, &non_unicode_value);
        }

        // Attempt to read the non-unicode env var
        let value = guard.env.var(KEY);
        assert_eq!(value, Some(non_unicode_value.to_string_lossy().to_string()));
        assert_eq!(
            guard.env.vars().find(|(k, _)| k == KEY),
            Some((
                KEY.to_string(),
                non_unicode_value.to_string_lossy().to_string()
            ))
        );

        // Overwrite the non-unicode env var with a valid unicode value
        let old = guard.env.set_var(KEY, "valid");
        assert_eq!(old, Some(non_unicode_value.to_string_lossy().to_string()));

        let value = guard.env.var(KEY);
        assert_eq!(value, Some("valid".to_string()));
    }
}
