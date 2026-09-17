use std::{io, os::windows::ffi::OsStrExt};
use windows::{core::PCWSTR, Win32::Foundation::MAX_PATH};

use super::drive_prefix::DrivePrefix;

pub fn get_volume_filesystem_name(prefix: &DrivePrefix) -> io::Result<String> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    let mut filesystem_name_buf = [0u16; MAX_PATH as usize + 1];
    let root_path_name = prefix.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let lp_root_path_name = PCWSTR::from_raw(root_path_name.as_ptr());

    let name = unsafe {
        GetVolumeInformationW(
            lp_root_path_name,              // lpRootPathName
            None,                           // lpVolumeNameBuffer
            None,                           // lpVolumeSerialNumber
            None,                           // lpMaximumComponentLength
            None,                           // lpFileSystemFlags
            Some(&mut filesystem_name_buf), // lpFileSystemNameBuffer
        )?;

        PCWSTR::from_raw(filesystem_name_buf.as_ptr())
            .to_string()
            .expect("Invalid file system name.")
    };

    Ok(name)
}
