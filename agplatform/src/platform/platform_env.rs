use std::path::Path;
use std::path::PathBuf;

use crate::Env;
use crate::fs::Fs;

/// Internal trait for updating the filesystem's current directory.
///
/// Implemented by both `FsImpl` and `TestFs` so that
/// [`PlatformEnv::set_current_dir`] can keep the environment and
/// filesystem in sync.
pub trait CurrentDirSetter {
    /// Sets the current directory used for path resolution.
    fn set_current_dir(&mut self, path: PathBuf);
}

/// An [`Env`] proxy that synchronizes [`set_current_dir`](Env::set_current_dir)
/// across both the environment and the filesystem.
///
/// Returned by [`Platform::env_mut`](crate::Platform::env_mut).
/// All other [`Env`] methods delegate directly to the inner
/// environment.
pub struct PlatformEnv<'a, E: Env, F: Fs + CurrentDirSetter> {
    pub(crate) env: &'a mut E,
    pub(crate) fs: &'a mut F,
}

impl<'a, E: Env, F: Fs + CurrentDirSetter> Env for PlatformEnv<'a, E, F> {
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

    /// Sets the current directory on both the environment and the
    /// filesystem, returning the previous value.
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        let new: PathBuf = path.into();
        let old = self.env.set_current_dir(new.clone());
        self.fs.set_current_dir(new);
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
