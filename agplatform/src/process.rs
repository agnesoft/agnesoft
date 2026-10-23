use std::path::PathBuf;

use crate::platform::platform_env::CurrentDirSetter;

/// Abstract process interface.
///
/// Spawns and runs child processes. Offers a blocking API and, behind
/// the `tokio` feature, an async one. Child processes run in the
/// configured current directory unless specified otherwise.
///
/// The two provided implementations are `ProcessImpl` (delegates to
/// [`std::process`]) and
/// [`TestProcess`](crate::test_platform::test_process::TestProcess)
/// (purely in-memory, behind the `testing` feature).
pub trait Process {}

/// [`Process`] implementation backed by [`std::process`].
///
/// Child processes run in the configured current directory.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct ProcessImpl {
    pub(crate) current_dir: PathBuf,
}

impl ProcessImpl {
    /// Creates a new `ProcessImpl` running child processes in the given
    /// directory.
    pub fn new(current_dir: PathBuf) -> Self {
        ProcessImpl { current_dir }
    }
}

impl CurrentDirSetter for ProcessImpl {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl Process for ProcessImpl {}

#[cfg(test)]
mod tests {
    use super::*;

    mod process_impl {
        use super::*;

        mod current_dir_setter {
            use super::*;

            #[test]
            fn set_current_dir_should_update_current_directory() {
                let mut process = ProcessImpl::new(PathBuf::new());
                let new_dir = PathBuf::from("/tmp");
                process.set_current_dir(new_dir.clone());
                assert_eq!(process.current_dir, new_dir);
            }
        }
    }
}
