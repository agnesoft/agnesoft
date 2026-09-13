/// The trait represents a file system interface.
pub trait Fs {}

/// The default implementation using the real file system via
/// std::fs.
pub struct FsImpl;

impl FsImpl {
    /// Creates a new instance of the default file system implementation.
    pub fn new() -> Self {
        FsImpl
    }
}

impl Fs for FsImpl {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_impl_new() {
        let _fs = FsImpl::new();
    }
}
