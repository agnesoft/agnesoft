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
    pub(crate) current_dir: std::path::PathBuf,
}

impl FsImpl {
    /// Creates a new instance of the default file system implementation.
    pub fn new(current_dir: std::path::PathBuf) -> Self {
        FsImpl { current_dir }
    }
}

impl CurrentDirSetter for FsImpl {
    fn set_current_dir(&mut self, path: std::path::PathBuf) {
        self.current_dir = path;
    }
}

impl Fs for FsImpl {
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        std::fs::read(normalize_path(&self.current_dir, path)).map_err(Error::fs)
    }
}

pub(crate) fn normalize_path<P: AsRef<Path>>(current_dir: &std::path::PathBuf, path: P) -> PathBuf {
    if path.as_ref().is_relative() {
        let mut normalized = PathBuf::new();
        normalized.push(current_dir);
        normalized.push(path);
        cannonicalize_path(&normalized)
    } else {
        cannonicalize_path(path)
    }
}

fn cannonicalize_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.as_ref().components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                normalized.push(component);
            }
            Component::Normal(part) => {
                normalized.push(part);
            }
        }
    }

    normalized
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
        let result = fs.read("Cargo.toml").unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_fs_impl_read_to_string() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let result = fs.read_to_string("Cargo.toml").unwrap();
        assert!(!result.is_empty());
    }
}
