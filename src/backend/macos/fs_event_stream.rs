use std::{
    ffi::{CStr, CString, OsStr},
    fmt::Formatter,
    os::{raw, unix::ffi::OsStrExt},
    panic,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use core_foundation::base::CFRelease;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchQueueAttr, DispatchRetained};
use tracing::{instrument, trace, warn};

use stream_create_flags::*;
use stream_event_flags::*;

use crate::{backend::BackendEvent, FileType};

pub struct FSEventStream {
    ptr: FSEventStreamRef,
    #[allow(unused)]
    queue: DispatchRetained<DispatchQueue>,
    started: bool,
}

/// It has no API apart from Drop, so it is safe.
unsafe impl Sync for FSEventStream {}

#[instrument(skip_all)]
pub fn fs_event_stream(
    paths_to_watch: &[PathBuf],
    paths_to_exclude: &[PathBuf],
    latency: Duration,
    flags: FsEventStreamFlags,
    callback: impl FnMut(thread::Result<Vec<FSEventStreamEvent>>) + Send + 'static,
) -> Result<FSEventStream, FsEventStreamError> {
    let callback = CallbackContext {
        function: Box::new(callback),
    };
    let mut stream = FSEventStream::new(callback, latency, paths_to_watch, paths_to_exclude, flags);
    stream.start()?;
    Ok(stream)
}

bitflags::bitflags! {
    /// The subset of `FSEventStreamCreateFlags` that callers may control.
    ///
    /// Flags such as `kFSEventStreamCreateFlagUseCFTypes` or `kFSEventStreamCreateFlagUseExtendedData`
    /// are intentionally not exposed here: the wrapper relies on the raw C callback shape they would change.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FsEventStreamFlags: FSEventStreamCreateFlags {
        /// Without this flag, we receive only directory-level notifications. For example, if a file is created, macOS produces an event for its parent.
        const FILE_EVENTS = kFSEventStreamCreateFlagFileEvents;
        /// Controls the interpretation of `latency`: when set, the first event is emitted immediately and subsequent events are delayed.
        const NO_DEFER = kFSEventStreamCreateFlagNoDefer;
    }
}

impl Default for FsEventStreamFlags {
    fn default() -> Self {
        Self::FILE_EVENTS | Self::NO_DEFER
    }
}

#[derive(Debug)]
pub struct FsEventStreamError(&'static str);

impl std::fmt::Display for FsEventStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FsEventStreamError {}

impl FSEventStream {
    fn new(
        callback_ctx: CallbackContext,
        latency: Duration,
        paths_to_watch: &[PathBuf],
        paths_to_exclude: &[PathBuf],
        flags: FsEventStreamFlags,
    ) -> Self {
        let paths_array = cf_array(paths_to_watch.len());
        for path in paths_to_watch {
            let path_string = path_to_cf_string(path.as_path());
            unsafe {
                CFArrayAppendValue(paths_array.0, path_string.0);
            }
        }

        // The context should be a heap pointer that lives as long as the stream.
        let callback_ptr = Box::into_raw(Box::new(callback_ctx));
        let context = FSEventStreamContext {
            version: 0,
            info: callback_ptr as *mut raw::c_void,
            retain: None,
            release: Some(release_callback_info),
            copy_description: None,
        };

        unsafe {
            let stream_ptr = FSEventStreamCreate(
                kCFAllocatorDefault,
                fs_event_stream_callback,
                &context,
                paths_array.0,
                kFSEventStreamEventIdSinceNow,
                latency.as_secs_f64(),
                flags.bits(),
            );

            if !paths_to_exclude.is_empty() {
                let cf_array = cf_array(paths_to_exclude.len());
                for path in paths_to_exclude {
                    let path_string = path_to_cf_string(path.as_path());
                    CFArrayAppendValue(cf_array.0, path_string.0);
                }
                if !FSEventStreamSetExclusionPaths(stream_ptr, cf_array.0) {
                    warn!(?paths_to_exclude, "FSEventStreamSetExclusionPaths failed, events will be reported",);
                }
            }

            let qos = DispatchQoS::Default;
            let relative_priority = 0;
            let attr = DispatchQueueAttr::with_qos_class(DispatchQueueAttr::SERIAL, qos, relative_priority);
            let queue = DispatchQueue::new("FSEventsDispatchQueue", Some(attr.as_ref()));
            FSEventStreamSetDispatchQueue(stream_ptr, DispatchRetained::as_ptr(&queue).as_ptr() as _);

            Self {
                ptr: stream_ptr,
                queue,
                started: false,
            }
        }
    }

    #[instrument(skip_all)]
    fn start(&mut self) -> Result<(), FsEventStreamError> {
        if !self.started {
            let succeeds = unsafe { FSEventStreamStart(self.ptr) };
            trace!(?succeeds, "FSEventStreamStart");
            if succeeds {
                self.started = true;
                Ok(())
            } else {
                Err(FsEventStreamError("Ought to always succeed, but in the event it does not then your code should fall back to performing recursive scans of the directories of interest as appropriate."))
            }
        } else {
            Ok(())
        }
    }
}

unsafe impl Send for FSEventStream {}

impl Drop for FSEventStream {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                FSEventStreamStop(self.ptr);
            }
            FSEventStreamInvalidate(self.ptr);
            FSEventStreamRelease(self.ptr);
        }
    }
}

struct CreateRule(CFTypeRef);

impl Drop for CreateRule {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn path_to_cf_string(path: &Path) -> CreateRule {
    use std::os::unix::prelude::*;
    let cstring =
        CString::new(path.as_os_str().as_bytes()).unwrap_or_else(|_err| panic!("failed to convert path to cstring: {}", path.display()));
    unsafe {
        let cf_string = CFStringCreateWithFileSystemRepresentation(kCFAllocatorDefault, cstring.as_bytes().as_ptr());
        if cf_string.is_null() {
            panic!("There was a problem in creating the string (possible if the conversion fails due to bytes in the buffer not being a valid sequence of bytes for the appropriate character encoding");
        }
        CreateRule(cf_string)
    }
}

fn cf_array(capacity: usize) -> CreateRule {
    unsafe {
        let cf_array = CFArrayCreateMutable(kCFAllocatorDefault, capacity as CFIndex, &kCFTypeArrayCallBacks);
        if cf_array.is_null() {
            panic!("Failed to allocate CFArrayCreateMutable");
        }
        CreateRule(cf_array)
    }
}

fn print_event_flags(flags: FSEventStreamEventFlags) -> String {
    let mut r = Vec::<&str>::new();
    if (flags & kFSEventStreamEventFlagMustScanSubDirs) != 0 {
        r.push("kFSEventStreamEventFlagMustScanSubDirs")
    }
    if (flags & kFSEventStreamEventFlagUserDropped) != 0 {
        r.push("kFSEventStreamEventFlagUserDropped")
    }
    if (flags & kFSEventStreamEventFlagKernelDropped) != 0 {
        r.push("kFSEventStreamEventFlagKernelDropped")
    }
    if (flags & kFSEventStreamEventFlagEventIdsWrapped) != 0 {
        r.push("kFSEventStreamEventFlagEventIdsWrapped")
    }
    if (flags & kFSEventStreamEventFlagHistoryDone) != 0 {
        r.push("kFSEventStreamEventFlagHistoryDone")
    }
    if (flags & kFSEventStreamEventFlagRootChanged) != 0 {
        r.push("kFSEventStreamEventFlagRootChanged")
    }
    if (flags & kFSEventStreamEventFlagMount) != 0 {
        r.push("kFSEventStreamEventFlagMount")
    }
    if (flags & kFSEventStreamEventFlagUnmount) != 0 {
        r.push("kFSEventStreamEventFlagUnmount")
    }
    if (flags & kFSEventStreamEventFlagItemCreated) != 0 {
        r.push("kFSEventStreamEventFlagItemCreated")
    }
    if (flags & kFSEventStreamEventFlagItemRemoved) != 0 {
        r.push("kFSEventStreamEventFlagItemRemoved")
    }
    if (flags & kFSEventStreamEventFlagItemInodeMetaMod) != 0 {
        r.push("kFSEventStreamEventFlagItemInodeMetaMod")
    }
    if (flags & kFSEventStreamEventFlagItemRenamed) != 0 {
        r.push("kFSEventStreamEventFlagItemRenamed")
    }
    if (flags & kFSEventStreamEventFlagItemModified) != 0 {
        r.push("kFSEventStreamEventFlagItemModified")
    }
    if (flags & kFSEventStreamEventFlagItemFinderInfoMod) != 0 {
        r.push("kFSEventStreamEventFlagItemFinderInfoMod")
    }
    if (flags & kFSEventStreamEventFlagItemChangeOwner) != 0 {
        r.push("kFSEventStreamEventFlagItemChangeOwner")
    }
    if (flags & kFSEventStreamEventFlagItemXattrMod) != 0 {
        r.push("kFSEventStreamEventFlagItemXattrMod")
    }
    if (flags & kFSEventStreamEventFlagItemIsFile) != 0 {
        r.push("kFSEventStreamEventFlagItemIsFile")
    }
    if (flags & kFSEventStreamEventFlagItemIsDir) != 0 {
        r.push("kFSEventStreamEventFlagItemIsDir")
    }
    if (flags & kFSEventStreamEventFlagItemIsSymlink) != 0 {
        r.push("kFSEventStreamEventFlagItemIsSymlink")
    }
    if (flags & kFSEventStreamEventFlagOwnEvent) != 0 {
        r.push("kFSEventStreamEventFlagOwnEvent")
    }
    if (flags & kFSEventStreamEventFlagItemIsHardlink) != 0 {
        r.push("kFSEventStreamEventFlagItemIsHardlink")
    }
    if (flags & kFSEventStreamEventFlagItemIsLastHardlink) != 0 {
        r.push("kFSEventStreamEventFlagItemIsLastHardlink")
    }
    if (flags & kFSEventStreamEventFlagItemCloned) != 0 {
        r.push("kFSEventStreamEventFlagItemCloned")
    }
    r.join(" | ")
}

#[derive(Clone)]
pub struct FSEventStreamEvent {
    pub path: PathBuf,
    pub id: FSEventStreamEventId,
    pub flags: FSEventStreamEventFlags,
}

impl std::fmt::Debug for FSEventStreamEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(std::any::type_name::<FSEventStreamEvent>())
            .field("id", &self.id)
            .field("path", &self.path)
            .field("flags", &print_event_flags(self.flags))
            .finish()
    }
}

/// Returns a cooked event
pub fn event(path: &Path, flags: FSEventStreamEventFlags, has_no_audience: bool) -> Option<BackendEvent> {
    let is_dir = (flags & kFSEventStreamEventFlagItemIsDir) != 0;
    let is_symlink = (flags & kFSEventStreamEventFlagItemIsSymlink) != 0;
    let created = (flags & kFSEventStreamEventFlagItemCreated) != 0 || (flags & kFSEventStreamEventFlagMount) != 0;
    let removed = (flags & kFSEventStreamEventFlagItemRemoved) != 0 || (flags & kFSEventStreamEventFlagUnmount) != 0;
    let changed = (flags & kFSEventStreamEventFlagItemModified) != 0
        || (flags & kFSEventStreamEventFlagItemInodeMetaMod) != 0
        || (flags & kFSEventStreamEventFlagItemChangeOwner) != 0;
    // kFSEventStreamEventFlagItemRenamed is set both for the old and the new path
    let ambiguous = (flags & kFSEventStreamEventFlagItemRenamed) != 0 || (removed && changed) || (removed && created);
    return if (flags & kFSEventStreamEventFlagMustScanSubDirs) != 0 {
        warn!(
            flags = print_event_flags(flags),
            "received kFSEventStreamEventFlagMustScanSubDirs on {}",
            path.display()
        );
        // macOS carefully coalesces the path according to the hierarchy.
        // https://developer.apple.com/documentation/coreservices/1455361-fseventstreameventflags/kfseventstreameventflagmustscansubdirs
        Some(BackendEvent::Overflow)
    } else if path.eq(Path::new("/")) {
        /*
            I don't have an explanation, but sometimes macOS decides to send an event like this:
            FSEventStreamEvent { id: 2182547631, path: "/", inode: Some(149174877), flags: "kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemIsFile" }
        */
        warn!(flags = print_event_flags(flags), "received event for root path, emitting rescan");
        return Some(BackendEvent::Ambiguous);
    } else if has_no_audience {
        return None;
    } else if ambiguous {
        match path.symlink_metadata() {
            Ok(meta) => {
                if is_dir || meta.is_dir() {
                    Some(BackendEvent::Ambiguous)
                } else {
                    Some(BackendEvent::Changed {
                        file_type: if is_symlink { FileType::Symlink } else { FileType::Regular },
                    })
                }
            }
            Err(_) => Some(BackendEvent::Removed),
        }
    } else if removed {
        Some(BackendEvent::Removed)
    } else if changed || created {
        let file_type = match (is_dir, is_symlink) {
            (true, false) => FileType::Directory,
            (false, true) => FileType::Symlink,
            (false, false) => FileType::Regular,
            // In this code path, we should be able to unambiguously tell apart a directory and a symlink.
            _ => unreachable!(),
        };
        let event = if created {
            BackendEvent::RecentlyCreated { file_type }
        } else {
            BackendEvent::Changed { file_type }
        };
        Some(event)
    } else {
        None
    };
}

struct CallbackContext {
    function: Box<dyn FnMut(thread::Result<Vec<FSEventStreamEvent>>) + Send + 'static>,
}

extern "C" fn release_callback_info(info: *const raw::c_void) {
    drop(unsafe { Box::from_raw(info as *mut CallbackContext) })
}

// Before Drop can release the CallbackContext, it stops FsEventStream.
// The client callback will not be called for this stream while it is stopped.
extern "C" fn fs_event_stream_callback(
    _stream_ref: ConstFSEventStreamRef,
    client_call_back_info: UnsafeMutableRawPointer,
    num_events: usize,
    event_paths: UnsafeMutableRawPointer,
    event_flags: *const FSEventStreamEventFlags,
    event_ids: *const FSEventStreamEventId,
) {
    trace!(num_events, "fs_event_stream_callback received events");

    let context = unsafe { &mut *(client_call_back_info as *mut CallbackContext) };
    let events = panic::catch_unwind(|| {
        let event_flags = unsafe { std::slice::from_raw_parts::<FSEventStreamEventFlags>(event_flags, num_events) };
        let event_ids = unsafe { std::slice::from_raw_parts::<FSEventStreamEventId>(event_ids, num_events) };
        let event_paths = unsafe { std::slice::from_raw_parts(event_paths as *const *const raw::c_char, num_events) };
        trace!(num_events, "fs_event_stream_callback received events");
        let mut events = Vec::<FSEventStreamEvent>::with_capacity(num_events);
        for i in 0..num_events {
            let path_cstring = unsafe { CStr::from_ptr(event_paths[i]) };
            let path = PathBuf::from(OsStr::from_bytes(path_cstring.to_bytes()));
            let flags = event_flags[i];
            let id = event_ids[i];
            events.push(FSEventStreamEvent { id, path, flags });
        }

        events
    });

    (context.function)(events);
}

//======================================================= BINDINGS =======================================================

type UInt32 = raw::c_uint;
type UInt64 = raw::c_ulong;

type CFTypeRef = *mut raw::c_void;
type CFStringRef = *mut raw::c_void;
type CFAllocatorRef = *const raw::c_void;

pub type CFArrayRetainCallBack = Option<unsafe extern "C" fn(allocator: CFAllocatorRef, value: *const raw::c_void) -> *const raw::c_void>;
pub type CFArrayReleaseCallBack = Option<unsafe extern "C" fn(allocator: CFAllocatorRef, value: *const ::std::os::raw::c_void)>;
pub type CFArrayCopyDescriptionCallBack = Option<unsafe extern "C" fn(value: *const raw::c_void) -> CFStringRef>;
pub type CFArrayEqualCallBack = Option<unsafe extern "C" fn(value1: *const raw::c_void, value2: *const raw::c_void) -> bool>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CFArrayCallBacks {
    pub version: CFIndex,
    pub retain: CFArrayRetainCallBack,
    pub release: CFArrayReleaseCallBack,
    pub copy_description: CFArrayCopyDescriptionCallBack,
    pub equal: CFArrayEqualCallBack,
}

type CFMutableArrayRef = *mut raw::c_void;

type CFTimeInterval = raw::c_double;
type CFIndex = raw::c_long;

type FSEventStreamRef = *mut raw::c_void;
type ConstFSEventStreamRef = *const raw::c_void;
type UnsafeMutableRawPointer = *mut raw::c_void;

type FSEventStreamCallback = extern "C" fn(
    ConstFSEventStreamRef,
    UnsafeMutableRawPointer,
    usize,
    UnsafeMutableRawPointer,
    *const FSEventStreamEventFlags,
    *const FSEventStreamEventId,
);

#[allow(non_upper_case_globals, dead_code)]
mod stream_event_flags {
    // https://developer.apple.com/documentation/coreservices/1455361-fseventstreameventflags

    pub type FSEventStreamEventFlags = std::os::raw::c_uint;

    pub const kFSEventStreamEventFlagNone: FSEventStreamEventFlags = 0x00000000;
    pub const kFSEventStreamEventFlagMustScanSubDirs: FSEventStreamEventFlags = 0x00000001;
    pub const kFSEventStreamEventFlagUserDropped: FSEventStreamEventFlags = 0x00000002;
    pub const kFSEventStreamEventFlagKernelDropped: FSEventStreamEventFlags = 0x00000004;
    pub const kFSEventStreamEventFlagEventIdsWrapped: FSEventStreamEventFlags = 0x00000008;
    pub const kFSEventStreamEventFlagHistoryDone: FSEventStreamEventFlags = 0x00000010;
    pub const kFSEventStreamEventFlagRootChanged: FSEventStreamEventFlags = 0x00000020;
    pub const kFSEventStreamEventFlagMount: FSEventStreamEventFlags = 0x00000040;
    pub const kFSEventStreamEventFlagUnmount: FSEventStreamEventFlags = 0x00000080;
    pub const kFSEventStreamEventFlagItemCreated: FSEventStreamEventFlags = 0x00000100;
    pub const kFSEventStreamEventFlagItemRemoved: FSEventStreamEventFlags = 0x00000200;
    pub const kFSEventStreamEventFlagItemInodeMetaMod: FSEventStreamEventFlags = 0x00000400;
    pub const kFSEventStreamEventFlagItemRenamed: FSEventStreamEventFlags = 0x00000800;
    pub const kFSEventStreamEventFlagItemModified: FSEventStreamEventFlags = 0x00001000;
    pub const kFSEventStreamEventFlagItemFinderInfoMod: FSEventStreamEventFlags = 0x00002000;
    pub const kFSEventStreamEventFlagItemChangeOwner: FSEventStreamEventFlags = 0x00004000;
    pub const kFSEventStreamEventFlagItemXattrMod: FSEventStreamEventFlags = 0x00008000;
    pub const kFSEventStreamEventFlagItemIsFile: FSEventStreamEventFlags = 0x00010000;
    pub const kFSEventStreamEventFlagItemIsDir: FSEventStreamEventFlags = 0x00020000;
    pub const kFSEventStreamEventFlagItemIsSymlink: FSEventStreamEventFlags = 0x00040000;
    pub const kFSEventStreamEventFlagOwnEvent: FSEventStreamEventFlags = 0x00080000;
    pub const kFSEventStreamEventFlagItemIsHardlink: FSEventStreamEventFlags = 0x00100000;
    pub const kFSEventStreamEventFlagItemIsLastHardlink: FSEventStreamEventFlags = 0x00200000;
    pub const kFSEventStreamEventFlagItemCloned: FSEventStreamEventFlags = 0x00400000;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFTypeArrayCallBacks: CFArrayCallBacks;

    fn CFStringCreateWithFileSystemRepresentation(alloc: CFAllocatorRef, buffer: *const raw::c_uchar) -> CFStringRef;
    fn CFArrayCreateMutable(alloc: CFAllocatorRef, capacity: CFIndex, callbacks: *const CFArrayCallBacks) -> CFMutableArrayRef;
    fn CFArrayAppendValue(the_array: CFMutableArrayRef, value: *const raw::c_void);
}

type CFAllocatorRetainCallBack = Option<unsafe extern "C" fn(info: *const raw::c_void) -> *const raw::c_void>;
type CFAllocatorReleaseCallBack = Option<unsafe extern "C" fn(info: *const raw::c_void)>;
type CFAllocatorCopyDescriptionCallBack = Option<unsafe extern "C" fn(info: *const raw::c_void) -> CFStringRef>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct FSEventStreamContext {
    pub version: CFIndex,
    pub info: *mut raw::c_void,
    pub retain: CFAllocatorRetainCallBack,
    pub release: CFAllocatorReleaseCallBack,
    pub copy_description: CFAllocatorCopyDescriptionCallBack,
}

type FSEventStreamEventId = UInt64;
type FSEventStreamCreateFlags = UInt32;

#[allow(non_upper_case_globals, dead_code)]
mod stream_create_flags {
    pub const kFSEventStreamCreateFlagNone: ::std::os::raw::c_uint = 0;
    /**
     * The framework will invoke your callback function with CF types
     * rather than raw C types (i.e., a CFArrayRef of CFStringRefs, rather than a raw C array of raw C string pointers).
     * See FSEventStreamCallback.
     */
    pub const kFSEventStreamCreateFlagUseCFTypes: ::std::os::raw::c_uint = 1;
    pub const kFSEventStreamCreateFlagNoDefer: ::std::os::raw::c_uint = 2;
    pub const kFSEventStreamCreateFlagWatchRoot: ::std::os::raw::c_uint = 4;
    pub const kFSEventStreamCreateFlagIgnoreSelf: ::std::os::raw::c_uint = 8;
    pub const kFSEventStreamCreateFlagFileEvents: ::std::os::raw::c_uint = 16;
    pub const kFSEventStreamCreateFlagMarkSelf: ::std::os::raw::c_uint = 32;
    /*
     * Requires kFSEventStreamCreateFlagUseCFTypes and instructs the
     * framework to invoke your callback function with CF types but,
     * instead of passing it a CFArrayRef of CFStringRefs, a CFArrayRef of
     * CFDictionaryRefs is passed.  Each dictionary will contain the event
     * path and possibly other "extended data" about the event.  See the
     * kFSEventStreamEventExtendedData*Key definitions for the set of keys
     * that may be set in the dictionary.  (See also FSEventStreamCallback.)
     */
    // available since OS X 10.13
    pub const kFSEventStreamCreateFlagUseExtendedData: ::std::os::raw::c_uint = 64;
}

#[allow(non_upper_case_globals)]
const kFSEventStreamEventIdSinceNow: FSEventStreamEventId = 0xFFFFFFFFFFFFFFFF;

#[allow(non_upper_case_globals)]
#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    // https://developer.apple.com/documentation/coreservices/file_system_events

    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFMutableArrayRef,
        since_when: FSEventStreamEventId,
        latency: CFTimeInterval,
        flags: FSEventStreamCreateFlags,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream_ref: FSEventStreamRef, queue_ptr: *mut raw::c_void);
    // True if it succeeds, otherwise False if it fails.
    // It ought to always succeed, but in the event it does not then your code should fall back to performing recursive scans of the directories of interest as appropriate.
    fn FSEventStreamStart(stream_ref: FSEventStreamRef) -> bool;
    fn FSEventStreamStop(stream_ref: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream_ref: FSEventStreamRef);
    fn FSEventStreamRelease(stream_ref: FSEventStreamRef);
    fn FSEventStreamSetExclusionPaths(stream_ref: FSEventStreamRef, paths_to_exclude: CFMutableArrayRef) -> bool;
}
