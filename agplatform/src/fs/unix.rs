//! [internal]
//!
//! The Unix specific parts of [`FsImpl`](super::FsImpl).

use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use super::Attribute;
use super::Attributes;
use super::FileId;
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
    std::fs::metadata(path)
        .map_err(fs_error_at(path))
        .and_then(|metadata| with_file_type(path, metadata))
}

pub(super) fn native_symlink_metadata(path: &Path) -> Result<(std::fs::Metadata, FileType)> {
    std::fs::symlink_metadata(path)
        .map_err(fs_error_at(path))
        .and_then(|metadata| with_file_type(path, metadata))
}

fn with_file_type(
    path: &Path,
    metadata: std::fs::Metadata,
) -> Result<(std::fs::Metadata, FileType)> {
    let file_type = metadata.file_type();

    if file_type.is_dir() {
        Ok((metadata, FileType::Dir))
    } else if file_type.is_file() {
        Ok((metadata, FileType::File))
    } else if file_type.is_symlink() {
        Ok((metadata, FileType::Symlink))
    } else {
        Err(fs_error!(
            "Unsupported file type, path is neither a file, a directory nor a symlink: {}",
            path.display()
        ))
    }
}

pub(super) fn path_file_id(path: &Path) -> std::io::Result<FileId> {
    std::fs::metadata(path).map(|metadata| file_id(&metadata))
}

pub(super) fn handle_file_id(file: &File) -> std::io::Result<FileId> {
    file.metadata().map(|metadata| file_id(&metadata))
}

fn file_id(metadata: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;

    FileId {
        volume: metadata.dev(),
        index: u128::from(metadata.ino()),
    }
}

pub(crate) fn native_symlink_target(target: &Path) -> PathBuf {
    target.to_path_buf()
}

pub(super) fn native_symlink(path: &Path, target: &Path, _: FileType) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

pub(super) fn native_remove_symlink(path: &Path, _: &std::fs::Metadata) -> std::io::Result<()> {
    std::fs::remove_file(path)
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
