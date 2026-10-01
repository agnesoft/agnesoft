use crate::Error;
use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// The trait represents a file system interface.
pub trait Fs {
    /// Reads the contents of a file at the given path as bytes (Vec<u8>).
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>>;

    /// Reads the contents of a file at the given path as UTF-8 encoded String.
    fn read_to_string<P: AsRef<Path>>(&self, path: P) -> Result<String> {
        String::from_utf8(self.read(path)?).map_err(Error::fs)
    }
}

/// The default implementation using the real file system via std::fs.
pub struct FsImpl {
    pub(crate) current_dir: PathBuf,
}

impl FsImpl {
    /// Creates a new instance of the default file system implementation.
    pub fn new(current_dir: PathBuf) -> Self {
        FsImpl { current_dir }
    }
}

impl CurrentDirSetter for FsImpl {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl Fs for FsImpl {
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        std::fs::read(normalize_path(&self.current_dir, path)).map_err(Error::fs)
    }
}

pub(crate) fn normalize_path<C: Into<PathBuf>, P: AsRef<Path>>(current_dir: C, path: P) -> PathBuf {
    if path.as_ref().is_relative() {
        cannonicalize_path(current_dir.into(), path)
    } else {
        cannonicalize_path(PathBuf::new(), path)
    }
}

fn cannonicalize_path<P: AsRef<Path>>(mut base: PathBuf, path: P) -> PathBuf {
    for component in path.as_ref().components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                base.push(component);
            }
            Component::Normal(part) => {
                base.push(part);
            }
        }
    }

    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_impl_new() {
        let _fs = FsImpl::new(std::env::current_dir().unwrap());
    }

    #[test]
    fn test_fs_impl_read() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let path = fs
            .current_dir
            .join("Cargo.toml")
            .to_string_lossy()
            .to_string();
        let result = fs.read(&path).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_fs_impl_read_to_string() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let result = fs.read_to_string("./src/../Cargo.toml").unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_fs_impl_read_fail() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let result = fs.read_to_string("Cargo.toml2");
        assert!(result.is_err());
    }

    #[test]
    fn test_fs_imple_set_current_dir() {
        let mut fs = FsImpl::new(PathBuf::new());
        let new_dir = PathBuf::from("/tmp");
        fs.set_current_dir(new_dir.clone());
        assert_eq!(fs.current_dir, new_dir);
    }
}
