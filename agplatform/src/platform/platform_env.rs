use std::path::Path;
use std::path::PathBuf;

use crate::Env;
use crate::Process;
use crate::fs::Fs;

/// Internal trait for updating the current directory of the filesystem
/// and process interfaces.
///
/// Implemented by `FsImpl`, `TestFs`, `ProcessImpl` and `TestProcess` so
/// that [`PlatformEnv::set_current_dir`] can keep the environment,
/// filesystem and process interfaces in sync.
pub trait CurrentDirSetter {
    /// Sets the current directory used for path resolution.
    fn set_current_dir(&mut self, path: PathBuf);
}

/// Internal trait for updating the environment variables of the process
/// and request interfaces.
///
/// Implemented by `ProcessImpl`, `TestProcess`, `RequestImpl` and
/// `TestRequest` so that [`PlatformEnv::set_var`] and
/// [`PlatformEnv::remove_var`] can pass the variables to child processes
/// and to the proxy configuration of the clients.
pub trait EnvVarSetter {
    /// Sets the variable `key` to `value`.
    fn set_var(&mut self, key: String, value: String);

    /// Removes the variable `key`.
    fn remove_var(&mut self, key: &str);
}

/// An [`Env`] proxy that synchronizes [`set_current_dir`](Env::set_current_dir)
/// across the environment, the filesystem and the process interface, and
/// [`set_var`](Env::set_var) and [`remove_var`](Env::remove_var) across the
/// environment and the process and request interfaces.
///
/// Returned by [`Platform::env_mut`](crate::Platform::env_mut).
/// All other [`Env`] methods delegate directly to the inner
/// environment.
pub struct PlatformEnv<
    'a,
    E: Env,
    F: Fs + CurrentDirSetter,
    P: Process + CurrentDirSetter + EnvVarSetter,
> {
    pub(crate) env: &'a mut E,
    pub(crate) fs: &'a mut F,
    pub(crate) process: &'a mut P,
    #[cfg(feature = "request")]
    pub(crate) request: &'a mut dyn EnvVarSetter,
}

impl<'a, E: Env, F: Fs + CurrentDirSetter, P: Process + CurrentDirSetter + EnvVarSetter> Env
    for PlatformEnv<'a, E, F, P>
{
    /// Delegates to the inner environment.
    fn args(&self) -> crate::EnvArgs<'_> {
        self.env.args()
    }

    /// Delegates to the inner environment.
    fn current_dir(&self) -> &Path {
        self.env.current_dir()
    }

    /// Delegates to the inner environment.
    fn current_exe(&self) -> &Path {
        self.env.current_exe()
    }

    /// Delegates to the inner environment.
    fn home_dir(&self) -> &Path {
        self.env.home_dir()
    }

    /// Removes the variable from the environment and the process and
    /// request interfaces, returning the previous value.
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        self.process.remove_var(key.as_ref());
        #[cfg(feature = "request")]
        self.request.remove_var(key.as_ref());
        self.env.remove_var(key)
    }

    /// Sets the current directory on the environment, the filesystem and
    /// the process interface, returning the previous value.
    fn set_current_dir<T: Into<PathBuf>>(&mut self, path: T) -> PathBuf {
        let new: PathBuf = path.into();
        let old = self.env.set_current_dir(new.clone());
        self.fs.set_current_dir(new.clone());
        self.process.set_current_dir(new);
        old
    }

    /// Sets the variable on the environment and the process and request
    /// interfaces, returning the previous value.
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
        let key = key.into();
        let value = value.into();
        self.process.set_var(key.clone(), value.clone());
        #[cfg(feature = "request")]
        self.request.set_var(key.clone(), value.clone());
        self.env.set_var(key, value)
    }

    /// Delegates to the inner environment.
    fn tmp_dir(&self) -> &Path {
        self.env.tmp_dir()
    }

    /// Delegates to the inner environment.
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        self.env.var(key)
    }

    /// Delegates to the inner environment.
    fn vars(&self) -> crate::EnvVars<'_> {
        self.env.vars()
    }
}
