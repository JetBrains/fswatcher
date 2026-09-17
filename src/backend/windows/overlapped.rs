use futures::channel::mpsc::UnboundedSender;
use std::{
    alloc::{self, Layout},
    io, panic,
    ptr::NonNull,
};
use tracing::{error, trace};
use windows::{
    Win32::Foundation::{ERROR_OPERATION_ABORTED, ERROR_SUCCESS, WIN32_ERROR},
    Win32::System::Threading::{
        CancelThreadpoolIo, CloseThreadpoolIo, CreateThreadpoolIo, StartThreadpoolIo, WaitForThreadpoolIoCallbacks, PTP_CALLBACK_INSTANCE,
        PTP_IO,
    },
    Win32::System::IO::OVERLAPPED,
};

use super::{file_handle::FileHandle, DriveId, IoMessage};

pub struct OverlappedBuffer {
    buffer: NonNull<u8>,
    layout: Layout,
}

impl OverlappedBuffer {
    pub fn alloc(layout: Layout) -> Self {
        let buffer = NonNull::new(unsafe { alloc::alloc(layout) }).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        OverlappedBuffer { buffer, layout }
    }
}

impl Drop for OverlappedBuffer {
    fn drop(&mut self) {
        unsafe {
            alloc::dealloc(self.buffer.as_ptr(), self.layout);
        }
    }
}

struct OverlappedIoContext {
    drive_id: DriveId,
    sender: UnboundedSender<IoMessage>,
}

pub struct OverlappedIo {
    handle: PTP_IO,
    buffer: OverlappedBuffer,
    context: NonNull<OverlappedIoContext>,
}

unsafe impl Send for OverlappedIo {}

impl OverlappedIo {
    pub(super) fn new(
        handle: &FileHandle,
        drive_id: DriveId,
        buffer: OverlappedBuffer,
        sender: UnboundedSender<IoMessage>,
    ) -> io::Result<Self> {
        let overlapped_io_context = Box::new(OverlappedIoContext { drive_id, sender });
        let context = unsafe { NonNull::new_unchecked(Box::into_raw(overlapped_io_context)) };
        let handle = unsafe { CreateThreadpoolIo(handle.as_handle(), Some(overlapped_io_callback), Some(context.as_ptr() as _), None) }?;
        Ok(OverlappedIo { handle, buffer, context })
    }

    pub(super) fn get_buffer(&self) -> *mut u8 {
        self.buffer.buffer.as_ptr()
    }

    pub(super) fn start_callback(&self) {
        unsafe { StartThreadpoolIo(self.handle) }
    }

    pub(super) fn cancel_callback(&self) {
        unsafe { CancelThreadpoolIo(self.handle) }
    }
}

impl Drop for OverlappedIo {
    fn drop(&mut self) {
        unsafe {
            // When we start an Overlapped IO operation, we allocate a buffer where ReadDirectoryChangesExW
            // will be writing file system notifications. We don't want to drop this buffer until overlapped
            // operations that use it are completed. The only way to know this is to let all callbacks finish.
            // Rust guarantees that impl Drop will be called before the fields are dropped (which is logical).
            // Hence, we're waiting for all pending callbacks to complete before we drop the buffer field.
            WaitForThreadpoolIoCallbacks(self.handle, false);
            CloseThreadpoolIo(self.handle);
            let _cleanup = Box::from_raw(self.context.as_ptr());
        }
    }
}

extern "system" fn overlapped_io_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut std::ffi::c_void,
    overlapped: *mut std::ffi::c_void,
    io_result: u32,
    number_of_bytes_transferred: usize,
    _io: PTP_IO,
) {
    let context = unsafe { &mut *(context as *mut OverlappedIoContext) };
    let drive_id = context.drive_id;

    let message = panic::catch_unwind(|| match WIN32_ERROR(io_result) {
        ERROR_SUCCESS if number_of_bytes_transferred == 0 => IoMessage::Empty { drive_id },
        ERROR_SUCCESS if number_of_bytes_transferred != 0 => IoMessage::IoCompleted { drive_id },
        ERROR_OPERATION_ABORTED => IoMessage::IoCancelled { drive_id },
        error => IoMessage::IoFailed {
            drive_id,
            err: io::Error::from_raw_os_error(error.0 as i32),
        },
    })
    .unwrap_or_else(|panic| IoMessage::IoPanicked { drive_id, panic });

    trace!(?message, "Sending a message");
    context
        .sender
        .unbounded_send(message)
        .unwrap_or_else(|err| error!(?err, "Failed to send a completion message."));

    // We create a new OVERLAPPED structure every time we start overlapped IO, so it's safe to drop it here.
    let _cleanup = unsafe { Box::from_raw(overlapped as *mut OVERLAPPED) };
}
