use crate::Fs;
use crate::fs::normalize_path;
use crate::platform::platform_env::CurrentDirSetter;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

    pub fn set<P: AsRef<std::path::Path>>(&mut self, path: P, content: Vec<u8>) {
        let path = normalize_path(&self.current_dir, path);
        self.files.insert(path, content);
    }

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
    fn read<P: AsRef<std::path::Path>>(&self, path: P) -> crate::Result<Vec<u8>> {
        let normalized_path = normalize_path(&self.current_dir, path);
        match self.files.get(&normalized_path) {
            Some(content) => Ok(content.clone()),
            None => Err(crate::Error::fs(format!(
                "File not found: {}",
                normalized_path.display()
            ))),
        }
    }
}
