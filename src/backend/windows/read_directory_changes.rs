use std::{
    alloc::Layout,
    ffi::OsString,
    io,
    mem::{self, offset_of},
    os::windows::ffi::{OsStrExt, OsStringExt},
    panic,
    path::{Path, PathBuf},
};
use tracing::{trace, warn};
use windows::{
    core::PCWSTR,
    Win32::Foundation::ERROR_INVALID_FUNCTION,
    Win32::Storage::FileSystem::{
        ReadDirectoryNotifyExtendedInformation, ReadDirectoryNotifyInformation, READ_DIRECTORY_NOTIFY_INFORMATION_CLASS,
    },
    Win32::System::IO::OVERLAPPED,
};

use super::{file_handle::FileHandle, overlapped::OverlappedIo, FileType};
use crate::backend::notify_buffer;

const READ_DIRECTORY_CHANGES_BUFFER_SIZE: usize = 512 * 1024;
pub const READ_DIRECTORY_CHANGES_BUFFER_LAYOUT: Layout = unsafe {
    // Layout's from_size_align_unchecked parameters must adhere to the requirements
    // outlined here: https://doc.rust-lang.org/stable/std/alloc/struct.Layout.html#method.from_size_align
    Layout::from_size_align_unchecked(READ_DIRECTORY_CHANGES_BUFFER_SIZE, mem::size_of::<u32>())
};

#[derive(Debug, Clone, Copy)]
pub enum NotifyInformationClass {
    Information,
    ExtendedInformation,
    // In the documentation there are two more values of this enum, but it seems they are
    // unsupported at the moment.
    //
    // FullInformation,
    // MaximumInformation,
}

impl NotifyInformationClass {
    fn as_raw(&self) -> READ_DIRECTORY_NOTIFY_INFORMATION_CLASS {
        match self {
            Self::Information => ReadDirectoryNotifyInformation,
            Self::ExtendedInformation => ReadDirectoryNotifyExtendedInformation,
        }
    }
}

#[derive(Debug)]
pub enum FileNotification {
    Information {
        // the file name relative to the directory handle
        file_name: OsString,
        action: FileAction,
    },

    ExtendedInformation {
        file_name: OsString,
        action: FileAction,
        is_dir: bool,
        is_symlink: bool,
    },
}

pub struct UnpackedNotification {
    pub file_path: PathBuf,
    pub action: FileAction,
    pub file_type: Option<FileType>,
}

pub fn file_type_from_flags(is_dir: bool, is_symlink: bool) -> FileType {
    // NOTE: the ordering here may matter. On Windows, a file can be both a symlink and a directory simultaneously.
    // On other platforms these flags are exclusive. To preserve the overall semantics, the is_symlink check
    // must go first.
    if is_symlink {
        FileType::Symlink
    } else if is_dir {
        FileType::Directory
    } else {
        FileType::Regular
    }
}

impl FileNotification {
    pub fn unpack(self) -> UnpackedNotification {
        let (action, file_type, file_path) = match self {
            FileNotification::Information { action, file_name, .. } => (action, None, PathBuf::from(file_name)),
            FileNotification::ExtendedInformation {
                action,
                is_dir,
                is_symlink,
                file_name,
                ..
            } => (action, Some(file_type_from_flags(is_dir, is_symlink)), PathBuf::from(file_name)),
        };
        UnpackedNotification {
            file_type,
            action,
            file_path,
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(non_camel_case_types)]
pub enum FileAction {
    FILE_ACTION_ADDED,
    FILE_ACTION_REMOVED,
    FILE_ACTION_MODIFIED,
    FILE_ACTION_RENAMED_OLD_NAME,
    FILE_ACTION_RENAMED_NEW_NAME,
}

impl FileAction {
    pub fn from_win(dword: windows::Win32::Storage::FileSystem::FILE_ACTION) -> Self {
        use windows::Win32::Storage::FileSystem::{
            FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
        };
        match dword {
            FILE_ACTION_ADDED => FileAction::FILE_ACTION_ADDED,
            FILE_ACTION_REMOVED => FileAction::FILE_ACTION_REMOVED,
            FILE_ACTION_MODIFIED => FileAction::FILE_ACTION_MODIFIED,
            FILE_ACTION_RENAMED_OLD_NAME => FileAction::FILE_ACTION_RENAMED_OLD_NAME,
            FILE_ACTION_RENAMED_NEW_NAME => FileAction::FILE_ACTION_RENAMED_NEW_NAME,
            unknown => panic!("Unknown file action {:?}", unknown),
        }
    }
}

// The bounded reader knows the entry layouts as plain offsets. They must match the `windows` crate's types.
const _: () = {
    use windows::Win32::Storage::FileSystem::{FILE_NOTIFY_EXTENDED_INFORMATION, FILE_NOTIFY_INFORMATION};
    assert!(offset_of!(FILE_NOTIFY_INFORMATION, NextEntryOffset) == 0);
    assert!(offset_of!(FILE_NOTIFY_INFORMATION, Action) == notify_buffer::BASIC.action);
    assert!(offset_of!(FILE_NOTIFY_INFORMATION, FileNameLength) == notify_buffer::BASIC.name_length);
    assert!(offset_of!(FILE_NOTIFY_INFORMATION, FileName) == notify_buffer::BASIC.name);
    assert!(notify_buffer::BASIC.attributes.is_none());
    assert!(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, NextEntryOffset) == 0);
    assert!(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, Action) == notify_buffer::EXTENDED.action);
    assert!(matches!(
        notify_buffer::EXTENDED.attributes,
        Some(offset) if offset == offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileAttributes)
    ));
    assert!(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileNameLength) == notify_buffer::EXTENDED.name_length);
    assert!(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName) == notify_buffer::EXTENDED.name);
};

/// Reads the notifications of one completed read. `bytes` is what the read transferred into the buffer.
///
/// The reader stays inside `bytes`. A chain that does not fit them is an error, and the caller must treat it as lost events.
pub fn read_file_notify_information(
    bytes: &[u8],
    notify_information_class: NotifyInformationClass,
) -> Result<Vec<FileNotification>, notify_buffer::Malformed> {
    use windows::Win32::Storage::FileSystem::{FILE_ACTION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

    let layout = match notify_information_class {
        NotifyInformationClass::Information => notify_buffer::BASIC,
        NotifyInformationClass::ExtendedInformation => notify_buffer::EXTENDED,
    };
    let entries = notify_buffer::read_entries(bytes, layout)?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let file_name = OsString::from_wide(&entry.name);
            // `read_entries` admits the five known actions only, so `from_win` does not reach its panic.
            let action = FileAction::from_win(FILE_ACTION(entry.action));
            match entry.attributes {
                None => FileNotification::Information { file_name, action },
                Some(attributes) => FileNotification::ExtendedInformation {
                    file_name,
                    action,
                    is_dir: (attributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0,
                    is_symlink: (attributes & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0,
                },
            }
        })
        .collect())
}

// Both the buffer and the overlapped structure must live until we receive the completion notification.
pub fn read_directory_changes_ex(
    directory: &FileHandle,
    recursive: bool,
    overlapped_io: &OverlappedIo,
    notify_information_class: &mut NotifyInformationClass,
) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        ReadDirectoryChangesExW, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
    };

    let h_directory = directory.as_handle();
    let lp_buffer = overlapped_io.get_buffer() as *mut std::ffi::c_void;
    let n_buffer_length = READ_DIRECTORY_CHANGES_BUFFER_LAYOUT.size().try_into().unwrap();
    let b_watch_subtree = recursive;
    let dw_notify_filter = {
        FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_SECURITY
    };
    let lp_bytes_returned = None;
    let lp_overlapped = Some(Box::into_raw(Box::<OVERLAPPED>::default()));
    let lp_completion_routine = None;

    overlapped_io.start_callback();
    let success = unsafe {
        ReadDirectoryChangesExW(
            h_directory,
            lp_buffer,
            n_buffer_length,
            b_watch_subtree,
            dw_notify_filter,
            lp_bytes_returned,
            lp_overlapped,
            lp_completion_routine,
            notify_information_class.as_raw(),
        )
    };
    trace!(?directory, ?success, ?notify_information_class, "ReadDirectoryChangesExW");
    success.or_else(|_| {
        let error = io::Error::last_os_error();
        overlapped_io.cancel_callback();

        // https://docs.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw#return-value
        // TODO handle ERROR_INVALID_FUNCTION: the network redirector or the target file system does not support this operation
        if error.raw_os_error() == Some(ERROR_INVALID_FUNCTION.0 as i32)
            && matches!(notify_information_class, NotifyInformationClass::ExtendedInformation)
        {
            warn!(?error, "reading directory changes failed, downgrading the information class");

            *notify_information_class = NotifyInformationClass::Information;
            return read_directory_changes_ex(directory, recursive, overlapped_io, notify_information_class);
        }

        Err(error)
    })
}

pub fn create_file(path: &Path) -> io::Result<FileHandle> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let lp_file_name = path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let dw_desired_access = FILE_LIST_DIRECTORY;
    let dw_share_mode = FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE;
    let lp_security_attributes = None;
    let dw_creation_disposition = OPEN_EXISTING;
    // https://docs.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw#remarks
    let dw_flags_and_attributes = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED;
    let h_template_file = None;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(lp_file_name.as_ptr()),
            dw_desired_access.0,
            dw_share_mode,
            lp_security_attributes,
            dw_creation_disposition,
            dw_flags_and_attributes,
            h_template_file,
        )?
    };
    trace!(path = %path.display(), ?handle, "CreateFileW");
    if handle.is_invalid() {
        Err(io::Error::last_os_error())
    } else {
        Ok(FileHandle::wrap(handle))
    }
}
