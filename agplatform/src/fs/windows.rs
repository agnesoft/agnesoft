//! [internal]
//!
//! The Windows specific parts of [`FsImpl`](super::FsImpl).

use std::fs::File;
use std::path::Path;

use super::Attributes;
use super::FileType;
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

pub(super) fn native_metadata(path: &Path) -> Result<(std::fs::Metadata, FileType)> {
    std::fs::metadata(path)
        .map(|metadata| {
            let file_type = if metadata.is_dir() {
                FileType::Dir
            } else {
                FileType::File
            };
            (metadata, file_type)
        })
        .map_err(fs_error_at(path))
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
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::RawHandle;

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
