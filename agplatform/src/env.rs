use crate::Result;

/// A trait that represents the environment in
/// which the program runs in. It offers read
/// operations of the environment variables
/// and the current working directory.
pub trait Env {
    /// Returns the value of the environment variable
    /// with the given `key` as `Option<String>`. If the
    /// key does not exist, `Ok(None)` is returned. If the
    /// key exists but is not valid Unicode, an `Err(crate::Error)`
    /// is returned of kind `Env`.
    fn var<T: AsRef<str>>(&self, key: T) -> Result<Option<String>>;
}

pub(crate) struct EnvImpl;

/// Default implementation of the `Env` trait that
/// uses the `std::env` implementations internally.
impl Env for EnvImpl {
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
    use std::ffi::OsStr;
    use std::ffi::OsString;

    fn non_unicode_value() -> OsString {
        #[cfg(unix)]
        {
            std::os::unix::ffi::OsStringExt::from_vec(vec![b'a', 0x80, b'b'])
        }

        #[cfg(windows)]
        {
            std::os::windows::ffi::OsStringExt::from_wide(&[b'a' as u16, 0xD800, b'b' as u16])
        }
    }

    struct EnvVarGuard {
        key: &'static str,
    }

    impl EnvVarGuard {
        fn new<T: AsRef<OsStr>>(key: &'static str, value: T) -> Self {
            unsafe {
                std::env::set_var(key, value.as_ref());
            }
            Self { key }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn missing() {
        const KEY: &str = "THIS_MISSING_ENV_VAR_SHOULD_NOT_EXIST";
        let env = EnvImpl;

        let missing = env.var(KEY).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn valid() {
        const KEY: &str = "THIS_VALID_ENV_VAR_SHOULD_NOT_EXIST";
        let _guard = EnvVarGuard::new(KEY, "abc");

        let env = EnvImpl;
        let value = env.var(KEY).unwrap();
        assert_eq!(value, Some("abc".to_string()));
    }

    #[test]
    fn non_unicode() {
        const KEY: &str = "THIS_INVALID_UNICODE_ENV_VAR_SHOULD_NOT_EXIST";
        let _guard = EnvVarGuard::new(KEY, non_unicode_value());
        let env = EnvImpl;

        let err = env
            .var(KEY)
            .expect_err("expected non-unicode env var to fail");
        assert_eq!(err.kind(), ErrorKind::Env);
    }
}
