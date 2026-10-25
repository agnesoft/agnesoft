//! [internal]
//!
//! The Windows specific parts of [`FsImpl`](super::FsImpl).

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::RawHandle;
use std::path::Path;
use std::path::PathBuf;

use super::Attributes;
use super::FileId;
use super::FileType;
use super::OpenedFile;
use super::fs_error_at;
use crate::Result;

pub(super) fn open_for_metadata(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    // WHY: directories can only be opened with this flag.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    File::options()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(super) fn path_file_id(path: &Path) -> std::io::Result<FileId> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_READ_ATTRIBUTES: u32 = 0x80;

    File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .open(path)
        .and_then(|file| handle_file_id(&file))
}

/// Returns the [`FileId`] of the file at `path` whose `metadata` was
/// already queried. Unlike on Unix the metadata does not hold it.
pub(super) fn metadata_file_id(path: &Path, _: &std::fs::Metadata) -> std::io::Result<FileId> {
    path_file_id(path)
}

/// Returns the [`OpenedFile`] details of the opened `file`, see
/// [`opened_len`] for its length.
pub(super) fn opened_file(file: &File) -> std::io::Result<OpenedFile> {
    // WHY: a directory cannot be opened without FILE_FLAG_BACKUP_SEMANTICS,
    // so an opened file never is one. Its ID takes another query, made
    // only once needed.
    let _ = file;
    Ok(OpenedFile {})
}

/// Returns the [`FileId`] of the opened `file` if it is to be `queried`,
/// which is not among its `opened` details.
pub(super) fn opened_id(file: &File, _: &OpenedFile, queried: bool) -> Option<FileId> {
    queried.then(|| handle_file_id(file).ok()).flatten()
}

/// Removes the directory or the symlink at `path` with its content, see
/// [`std::fs::remove_dir_all`].
pub(super) fn native_remove_dir_all(path: &Path) -> std::io::Result<()> {
    // WHY: unlike on Unix std refuses a symlink to a file, which is removed
    // itself like a symlink to a directory.
    std::fs::remove_dir_all(path).or_else(|error| match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => std::fs::remove_file(path),
        _ => Err(error),
    })
}

/// Returns the error for failing to open `path` for reading, which is
/// that it is a directory if it is one.
pub(super) fn open_read_error(path: &Path, error: std::io::Error) -> crate::Error {
    // WHY: unlike on Unix a directory cannot be opened, so it is checked
    // only to explain the failure.
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        super::directory_error(path)
    } else {
        fs_error_at(path)(error)
    }
}

/// Returns the length of the opened `file`, which is not among its
/// `opened` details, queried only when needed.
pub(super) fn opened_len(file: &File, _: &OpenedFile) -> std::io::Result<u64> {
    file.metadata().map(|metadata| metadata.len())
}

pub(super) fn handle_file_id(file: &File) -> std::io::Result<FileId> {
    select_file_id(file_id_info(file), file_information(file))
}

fn select_file_id(
    file_id: std::io::Result<FileId>,
    information: std::io::Result<ByHandleFileInformation>,
) -> std::io::Result<FileId> {
    // WHY: the 128-bit file ID is unique on ReFS too, unlike the 64-bit
    // file index, but it is not supported by every filesystem (e.g. some
    // network shares), which fall back to the index.
    file_id.or_else(|_| {
        information.map(|information| FileId {
            volume: u64::from(information.volume_serial_number),
            index: u128::from(information.file_index_high) << 32
                | u128::from(information.file_index_low),
        })
    })
}

/// Returns the number of hard links of the opened `file`.
pub(super) fn handle_link_count(file: &File) -> std::io::Result<u32> {
    file_information(file).map(|information| information.number_of_links)
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(
        file: RawHandle,
        information_class: i32,
        information: *mut c_void,
        buffer_size: u32,
    ) -> i32;

    fn GetFileInformationByHandle(file: RawHandle, information: *mut c_void) -> i32;
}

fn file_id_info(file: &File) -> std::io::Result<FileId> {
    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    const FILE_ID_INFO_CLASS: i32 = 18;

    let mut information = FileIdInfo::default();

    // SAFETY: `file` is an open handle and `information` is a FILE_ID_INFO
    // alive for the duration of the call.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO_CLASS,
            (&raw mut information).cast(),
            size_of::<FileIdInfo>() as u32,
        )
    };

    (result != 0)
        .then(|| FileId {
            volume: information.volume_serial_number,
            index: u128::from_le_bytes(information.file_id),
        })
        .ok_or_else(std::io::Error::last_os_error)
}

/// [internal]
///
/// The `BY_HANDLE_FILE_INFORMATION` of a file.
#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

fn file_information(file: &File) -> std::io::Result<ByHandleFileInformation> {
    let mut information = ByHandleFileInformation::default();

    // SAFETY: `file` is an open handle and `information` is a
    // BY_HANDLE_FILE_INFORMATION alive for the duration of the call.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), (&raw mut information).cast()) };

    (result != 0)
        .then_some(information)
        .ok_or_else(std::io::Error::last_os_error)
}

pub(super) fn native_metadata(path: &Path) -> Result<(std::fs::Metadata, FileType)> {
    std::fs::metadata(path)
        .map(with_file_type)
        .map_err(fs_error_at(path))
}

/// Returns the metadata and the file type of the opened `file` at `path`.
pub(super) fn native_handle_metadata(
    path: &Path,
    file: &File,
) -> Result<(std::fs::Metadata, FileType)> {
    file.metadata()
        .map(with_file_type)
        .map_err(fs_error_at(path))
}

pub(super) fn native_symlink_metadata(path: &Path) -> Result<(std::fs::Metadata, FileType)> {
    std::fs::symlink_metadata(path)
        .map(with_file_type)
        .map_err(fs_error_at(path))
}

fn with_file_type(metadata: std::fs::Metadata) -> (std::fs::Metadata, FileType) {
    let file_type = metadata.file_type();
    let file_type = if file_type.is_symlink() {
        FileType::Symlink
    } else if file_type.is_dir() {
        FileType::Dir
    } else {
        FileType::File
    };

    (metadata, file_type)
}

/// Returns `target` with `/` replaced by `\`: the target of a relative
/// symlink is not resolved if it contains `/`.
pub(crate) fn native_symlink_target(target: &Path) -> PathBuf {
    let bytes: Vec<u8> = target
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|&byte| if byte == b'/' { b'\\' } else { byte })
        .collect();

    // SAFETY: replacing one ASCII byte with another keeps the bytes valid
    // as they were obtained from `as_encoded_bytes`.
    PathBuf::from(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes) })
}

pub(super) fn native_symlink(
    path: &Path,
    target: &Path,
    file_type: FileType,
) -> std::io::Result<()> {
    if file_type.is_dir() {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    }
}

/// A directory symlink is a directory entry that only `RemoveDirectoryW`
/// removes, `DeleteFileW` fails for it.
pub(super) fn native_remove_symlink(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    if metadata.file_type().is_symlink_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub(super) const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub(super) const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub(super) const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

pub(super) fn native_attributes(metadata: &std::fs::Metadata) -> Attributes {
    use std::os::windows::fs::MetadataExt;

    let native = metadata.file_attributes();

    Attributes {
        readonly: native & FILE_ATTRIBUTE_READONLY != 0,
        executable: false,
        hidden: native & FILE_ATTRIBUTE_HIDDEN != 0,
    }
}

pub(super) fn set_native_attributes(
    path: &Path,
    metadata: &std::fs::Metadata,
    attributes: Attributes,
) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    let current = metadata.file_attributes();
    let mut native = current & !(FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN);

    if attributes.readonly {
        native |= FILE_ATTRIBUTE_READONLY;
    }

    if attributes.hidden {
        native |= FILE_ATTRIBUTE_HIDDEN;
    }

    if native == current {
        return Ok(());
    }

    // WHY: in FILE_BASIC_INFO an attribute value of 0 leaves the attributes
    // unchanged, clearing all of them requires FILE_ATTRIBUTE_NORMAL.
    if native == 0 {
        native = FILE_ATTRIBUTE_NORMAL;
    }

    #[repr(C)]
    struct FileBasicInfo {
        creation_time: i64,
        last_access_time: i64,
        last_write_time: i64,
        change_time: i64,
        file_attributes: u32,
    }

    const FILE_BASIC_INFO_CLASS: i32 = 0;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            file: RawHandle,
            information_class: i32,
            information: *const std::ffi::c_void,
            buffer_size: u32,
        ) -> i32;
    }

    // WHY: zero times leave the timestamps unchanged.
    let information = FileBasicInfo {
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        file_attributes: native,
    };

    // WHY: opening through std, unlike passing the path to Win32, supports
    // paths longer than MAX_PATH.
    open_for_metadata(path).and_then(|file| {
        // SAFETY: `file` is an open handle with FILE_WRITE_ATTRIBUTES access
        // and `information` is a FILE_BASIC_INFO alive for the duration of
        // the call.
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FILE_BASIC_INFO_CLASS,
                (&raw const information).cast(),
                size_of::<FileBasicInfo>() as u32,
            )
        };

        (result != 0)
            .then_some(())
            .ok_or_else(std::io::Error::last_os_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod select_file_id {
        use super::*;

        #[test]
        fn select_file_id_should_prefer_file_id_info() {
            let file_id = FileId {
                volume: 7,
                index: 11,
            };
            let information = ByHandleFileInformation {
                volume_serial_number: 3,
                file_index_high: 2,
                file_index_low: 1,
                ..Default::default()
            };

            let selected = select_file_id(Ok(file_id), Ok(information)).unwrap();

            assert_eq!(
                selected,
                FileId {
                    volume: 7,
                    index: 11
                }
            );
        }

        #[test]
        fn select_file_id_failing_info_should_fall_back_to_file_information() {
            let fallback = ByHandleFileInformation {
                volume_serial_number: 3,
                file_index_high: 2,
                file_index_low: 1,
                ..Default::default()
            };

            let selected = select_file_id(
                Err(std::io::Error::other("no FILE_ID_INFO")),
                Ok(fallback),
            )
            .unwrap();

            assert_eq!(
                selected,
                FileId {
                    volume: 3,
                    index: (u128::from(2_u32) << 32) | u128::from(1_u32)
                }
            );
        }
    }
}
