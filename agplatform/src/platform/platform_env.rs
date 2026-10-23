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

/// An [`Env`] proxy that synchronizes [`set_current_dir`](Env::set_current_dir)
/// across the environment, the filesystem and the process interface.
///
/// Returned by [`Platform::env_mut`](crate::Platform::env_mut).
/// All other [`Env`] methods delegate directly to the inner
/// environment.
pub struct PlatformEnv<'a, E: Env, F: Fs + CurrentDirSetter, P: Process + CurrentDirSetter> {
    pub(crate) env: &'a mut E,
    pub(crate) fs: &'a mut F,
    pub(crate) process: &'a mut P,
}

impl<'a, E: Env, F: Fs + CurrentDirSetter, P: Process + CurrentDirSetter> Env
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

    /// Delegates to the inner environment.
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
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

    /// Delegates to the inner environment.
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
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
