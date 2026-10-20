//! [internal]
//!
//! The Unix specific parts of [`FsImpl`](super::FsImpl).

use std::fs::File;
use std::path::Path;

use super::Attribute;
use super::Attributes;
use super::FileType;
use super::fs_error_at;
use crate::Result;

/// [internal]
///
/// The mode and owner of a Unix entry as natively stored, from which its
/// [`Attributes`] derive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnixAttributes {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl UnixAttributes {
    pub(crate) fn attributes(&self) -> Attributes {
        Attributes {
            readonly: self.mode & 0o222 == 0,
            executable: self.mode & 0o111 != 0,
            hidden: false,
        }
    }

    /// Changes the mode to store `attribute` set to `value`.
    pub(crate) fn set(&mut self, attribute: Attribute, value: bool) {
        let mut attributes = self.attributes();
        attributes.set(attribute, value);

        // WHY: std's `set_readonly(false)` makes the file world-writable,
        // only the owner gets write access back here.
        if attributes.readonly {
            self.mode &= !0o222;
        } else if self.mode & 0o222 == 0 {
            self.mode |= 0o200;
        }

        if !attributes.executable {
            self.mode &= !0o111;
        } else if self.mode & 0o111 == 0 {
            self.mode |= ((self.mode & 0o444) >> 2) | 0o100;
        }
    }
}

pub(super) fn open_for_metadata(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

pub(super) fn native_metadata(path: &Path) -> Result<(std::fs::Metadata, FileType)> {
    let metadata = std::fs::metadata(path).map_err(fs_error_at(path))?;

    if metadata.is_dir() {
        Ok((metadata, FileType::Dir))
    } else if metadata.is_file() {
        Ok((metadata, FileType::File))
    } else {
        Err(fs_error!(
            "Unsupported file type, path is neither a file nor a directory: {}",
            path.display()
        ))
    }
}

pub(super) fn native_attributes(metadata: &std::fs::Metadata) -> UnixAttributes {
    use std::os::unix::fs::MetadataExt;

    UnixAttributes {
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
    }
}

pub(super) fn set_native_attributes(
    path: &Path,
    metadata: &std::fs::Metadata,
    native: UnixAttributes,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // WHY: changing permissions requires owning the file, so a request
    // that is already satisfied must not fail.
    if native.mode == native_attributes(metadata).mode {
        return Ok(());
    }

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(native.mode))
}
