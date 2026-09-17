use tracing::error;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

#[derive(Debug)]
pub struct FileHandle(HANDLE);

impl FileHandle {
    pub fn wrap(handle: HANDLE) -> Self {
        FileHandle(handle)
    }

    pub fn as_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        if let Err(err) = unsafe { CloseHandle(self.0) } {
            error!(error = ?err, file_handle = ?self.0, "failed to close file handle");
        }
    }
}

unsafe impl Sync for FileHandle {}

unsafe impl Send for FileHandle {}
