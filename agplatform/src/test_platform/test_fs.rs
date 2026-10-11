use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::Error;
use crate::Fs;
use crate::Result;
use crate::fs::normalize_path;
use crate::platform::platform_env::CurrentDirSetter;

/// The test file system is a memory-only but fully functional mock
/// implementation of a file system interface. It is used in tests to be
/// passed where `impl Fs` is expected.
pub struct TestFs {
    current_dir: PathBuf,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl TestFs {
    /// Creates a new instance of the test file system.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let test_fs = TestFs::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TestFs {
            current_dir: PathBuf::new(),
            files: BTreeMap::new(),
        }
    }

    /// Sets the content of a file at the given path in the test file system. The
    /// path is normalized and canonicalized using the current directory. The content
    /// will be overwritten if it already exists. The intermediate directories do not
    /// need to exist; they are not created and they are derived to exist from the
    /// normalized path for the access purposes. This means only the file needs to be
    /// created and the entire directory hierarchy exists as a result.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, test_platform::test_fs::TestFs};
    ///
    /// let mut test_fs = TestFs::new();
    /// test_fs.set("/some/path/Cargo.toml", b"content");
    /// assert_eq!(test_fs.read("/some/path/Cargo.toml").unwrap(), b"content");
    ///
    /// test_fs.set("some.text", "content".as_bytes());
    /// assert_eq!(test_fs.read_to_string("some.text").unwrap(), "content");
    /// ```
    pub fn set<P: AsRef<std::path::Path>, C: Into<Vec<u8>>>(&mut self, path: P, content: C) {
        let path = normalize_path(&self.current_dir, path);
        self.files.insert(path, content.into());
    }

    /// Removes the file at the given path from the test file system. The path is
    /// normalized and canonicalized using the current directory. If the file does
    /// not exist, this operation has no effect.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, test_platform::test_fs::TestFs};
    ///
    /// let mut test_fs = TestFs::new();
    /// test_fs.set("/some/path/Cargo.toml", b"content");
    /// assert_eq!(test_fs.read("/some/path/Cargo.toml").unwrap(), b"content");
    ///
    /// test_fs.remove("/some/path/Cargo.toml");
    /// assert!(test_fs.read("/some/path/Cargo.toml").is_err());
    /// ```
    pub fn remove<P: AsRef<std::path::Path>>(&mut self, path: P) {
        let path = normalize_path(&self.current_dir, path);
        self.files.remove(&path);
    }
}

impl CurrentDirSetter for TestFs {
    fn set_current_dir(&mut self, path: std::path::PathBuf) {
        self.current_dir = path;
    }
}

impl Fs for TestFs {
    /// Reads the content of a file at the given path from the test file system. The
    /// path is normalized and canonicalized using the current directory. If the file
    /// does not exist, an error is returned. The [TestFs] operates entirely in memory
    /// and does not interact with the real file system.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, test_platform::test_fs::TestFs};
    ///
    /// let mut test_fs = TestFs::new();
    /// test_fs.set("Cargo.toml", b"content");
    /// assert_eq!(test_fs.read("Cargo.toml").unwrap(), b"content");
    /// assert_eq!(test_fs.read_to_string("Cargo.toml").unwrap(), "content");
    /// ```
    fn read<P: AsRef<std::path::Path>>(&self, path: P) -> Result<Vec<u8>> {
        let normalized_path = normalize_path(&self.current_dir, path);
        match self.files.get(&normalized_path) {
            Some(content) => Ok(content.clone()),
            None => Err(Error::fs(format!(
                "File not found: {}",
                normalized_path.display()
            ))),
        }
    }
}
