use crate::Result;

/// A trait that represents the environment in
/// which the program runs in. It offers read
/// operations of the environment variables
/// and the current working directory.
///
/// The default implementation of the `Env` trait
/// uses the `std::env` implementations. The entire
/// program using it should access the environment
/// exclusively via the `Env` trait to prevent
/// races and to allow for mocking in tests.
///
/// Example:
///
/// ```rust
/// use agplatform::{Platform, Env};
///
/// let platform = agplatform::platform();
/// let value = platform.env().var("PATH").unwrap();
/// assert!(value.is_some());
/// ```
pub trait Env {
    /// Removes the environment variable with the given `key`.
    /// If the key exists, the previous value is returned as
    /// `Ok(Some(String))`. If the key does not exist, `Ok(None)`
    /// is returned.
    ///
    /// Example:
    ///
    /// ```ignore
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// platform.env_mut().set_var("TEST_ENV_VAR", "value").unwrap();
    /// let old_value = platform.env_mut().remove_var("TEST_ENV_VAR").unwrap();
    /// assert_eq!(old_value, Some("value".to_string()));
    /// ```
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Result<Option<String>>;

    /// Sets the environment variable with the given `key`
    /// to the given `value`. If the key already exists,
    /// the previous value is returned as `Ok(Some(String))`.
    /// If the key does not exist, `Ok(None)` is returned.
    ///
    /// Example:
    ///
    /// ```ignore
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// let old_value = platform.env_mut().set_var("TEST_ENV_VAR", "value").unwrap();
    /// assert_eq!(old_value, None);
    /// let old_value = platform.env_mut().set_var("TEST_ENV_VAR", "value2").unwrap();
    /// assert_eq!(old_value, Some("value".to_string()));
    /// ```
    fn set_var<T: AsRef<str>, U: AsRef<str>>(&mut self, key: T, value: U)
    -> Result<Option<String>>;

    /// Returns the value of the environment variable
    /// with the given `key` as `Option<String>`. If the
    /// key does not exist, `Ok(None)` is returned. If the
    /// key exists but is not valid Unicode, an `Err(crate::Error)`
    /// is returned of kind `Env`.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Platform, Env};
    ///
    /// let platform = agplatform::platform();
    /// let value = platform.env().var("PATH").unwrap();
    /// assert!(value.is_some());
    ///
    /// let value = platform.env().var("TEST_ENV_VAR").unwrap();
    /// assert_eq!(value, None);
    /// ```
    fn var<T: AsRef<str>>(&self, key: T) -> Result<Option<String>>;
}

pub(crate) struct EnvImpl;

impl Env for EnvImpl {
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Result<Option<String>> {
        let old_value = std::env::var(key.as_ref()).ok();

        unsafe {
            std::env::remove_var(key.as_ref());
        }

        Ok(old_value)
    }

    fn set_var<T: AsRef<str>, U: AsRef<str>>(
        &mut self,
        key: T,
        value: U,
    ) -> Result<Option<String>> {
        let old_value = std::env::var(key.as_ref()).ok();

        unsafe {
            std::env::set_var(key.as_ref(), value.as_ref());
        }
        Ok(old_value)
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Result<Option<String>> {
        let key = key.as_ref();
        match std::env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    struct EnvGuard {
        key: &'static str,
        env: EnvImpl,
    }

    impl EnvGuard {
        fn new(key: &'static str) -> Result<Self> {
            let mut env = EnvImpl;
            let _ = env.remove_var(key);
            Ok(Self { key, env })
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let _ = self.env.remove_var(self.key);
        }
    }

    #[test]
    fn var() {
        const KEY: &str = "THIS_ENV_VAR_SHOULD_NOT_EXIST";
        let mut guard = EnvGuard::new(KEY).unwrap();

        // Get missing env var
        let missing = guard.env.var(KEY).unwrap();
        assert_eq!(missing, None);

        // Set env var
        let old = guard.env.set_var(KEY, "abc").unwrap();
        assert_eq!(old, None);

        // Get previously set env var
        let value = guard.env.var(KEY).unwrap();
        assert_eq!(value, Some("abc".to_string()));

        // Overwrite env var
        let old = guard.env.set_var(KEY, "def").unwrap();
        assert_eq!(old, Some("abc".to_string()));
        let value = guard.env.var(KEY).unwrap();
        assert_eq!(value, Some("def".to_string()));

        // Remove env var
        let old = guard.env.remove_var(KEY).unwrap();
        assert_eq!(old, Some("def".to_string()));

        // Get missing env var again
        let missing = guard.env.var(KEY).unwrap();
        assert_eq!(missing, None);

        // Remove missing env var
        let missing = guard.env.remove_var(KEY).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn non_unicode() {
        const KEY: &str = "THIS_INVALID_UNICODE_ENV_VAR_SHOULD_NOT_EXIST";
        let mut guard = EnvGuard::new(KEY).unwrap();

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
        let err = guard
            .env
            .var(KEY)
            .expect_err("expected non-unicode env var to fail");
        assert_eq!(err.kind(), ErrorKind::Env);

        // Overwrite the non-unicode env var with a valid unicode value
        let old = guard.env.set_var(KEY, "valid").unwrap();
        assert_eq!(old, None);
        let value = guard.env.var(KEY).unwrap();
        assert_eq!(value, Some("valid".to_string()));
    }
}
