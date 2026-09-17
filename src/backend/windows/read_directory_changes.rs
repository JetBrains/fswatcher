use std::{
    alloc::Layout,
    ffi::OsString,
    io,
    mem::{self, offset_of, MaybeUninit},
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

pub unsafe fn read_file_notify_information(buffer: *mut u8, notify_information_class: NotifyInformationClass) -> Vec<FileNotification> {
    use std::slice;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_NOTIFY_EXTENDED_INFORMATION, FILE_NOTIFY_INFORMATION,
    };

    // We cannot simply reinterpret the pointer from *u8 to *FILE_NOTIFY(_EXTENDED)_INFORMATION
    // here, because there is no guarantee that `ptr` will stay aligned by the alignment
    // of the struct (8) after adding `NextEntryOffset` to it.
    //
    // Note that, since rust 1.70, a check is performed on all pointer reads for proper
    // alignment. Misaligned reads cause a panic.
    //
    // Also note that this read is incomplete: FILE_NOTIFY_EXTENDED_INFORMATION uses
    // the flexible array member pattern to store the file name as a part of the struct,
    // which means we are slicing off the file name and it isn't part of the `entry`.
    //
    // Because of that we get the file name as a separate read by constructing a slice
    // at the offset of the FileName field relative to the base of the current entry.
    //
    // Note that `slice::from_raw_parts` also must be properly aligned, in our case, by the
    // size of u16 (2 bytes). Given that the base pointer of the entire buffer is known to be
    // aligned at DWORD (a requirement of ReadDirectoryChangesEx) and that all the struct
    // members' sizes are a multiple of two, we can safely assume that the alignment here will
    // also be proper.

    let mut notifications = Vec::new();
    let mut ptr = buffer;
    match notify_information_class {
        NotifyInformationClass::Information => loop {
            let mut entry = MaybeUninit::<FILE_NOTIFY_INFORMATION>::uninit();
            ptr.copy_to(entry.as_mut_ptr() as *mut u8, mem::size_of::<FILE_NOTIFY_INFORMATION>());
            let entry = entry.assume_init();

            let name_slice = slice::from_raw_parts(
                ptr.add(offset_of!(FILE_NOTIFY_INFORMATION, FileName)) as *const u16,
                (entry.FileNameLength / 2) as usize,
            );
            let file_name = OsString::from_wide(name_slice);

            notifications.push(FileNotification::Information {
                file_name,
                action: FileAction::from_win(entry.Action),
            });

            ptr = match entry.NextEntryOffset {
                0 => break notifications,
                n => ptr.add(n as usize),
            };
        },

        NotifyInformationClass::ExtendedInformation => loop {
            let mut entry = MaybeUninit::<FILE_NOTIFY_EXTENDED_INFORMATION>::uninit();
            ptr.copy_to(entry.as_mut_ptr() as *mut u8, mem::size_of::<FILE_NOTIFY_EXTENDED_INFORMATION>());
            let entry = entry.assume_init();

            let name_slice = slice::from_raw_parts(
                ptr.add(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName)) as *const u16,
                (entry.FileNameLength / 2) as usize,
            );
            let file_name = OsString::from_wide(name_slice);

            notifications.push(FileNotification::ExtendedInformation {
                file_name,
                action: FileAction::from_win(entry.Action),
                is_dir: (entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0,
                is_symlink: (entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0,
            });

            ptr = match entry.NextEntryOffset {
                0 => break notifications,
                n => ptr.add(n as usize),
            };
        },
    }
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
