use std::path::Path;
use std::path::PathBuf;

use crate::Env;
use crate::fs::Fs;

pub trait CurrentDirSetter {
    fn set_current_dir(&mut self, path: PathBuf);
}

pub struct PlatformEnv<'a, E: Env, F: Fs + CurrentDirSetter> {
    pub(crate) env: &'a mut E,
    pub(crate) fs: &'a mut F,
}

impl<'a, E: Env, F: Fs + CurrentDirSetter> Env for PlatformEnv<'a, E, F> {
    fn args(&self) -> crate::EnvArgs<'_> {
        self.env.args()
    }

    fn current_dir(&self) -> &Path {
        self.env.current_dir()
    }

    fn current_exe(&self) -> &Path {
        self.env.current_exe()
    }

    fn home_dir(&self) -> &Path {
        self.env.home_dir()
    }

    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        self.env.remove_var(key)
    }

    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        let new: PathBuf = path.into();
        let old = self.env.set_current_dir(new.clone());
        self.fs.set_current_dir(new);
        old
    }

    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
        self.env.set_var(key, value)
    }

    fn tmp_dir(&self) -> &Path {
        self.env.tmp_dir()
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        self.env.var(key)
    }

    fn vars(&self) -> crate::EnvVars<'_> {
        self.env.vars()
    }
}
