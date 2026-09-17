mod device_watcher;
mod drive_prefix;
mod file_handle;
mod get_volume_filesystem_name;
pub mod immediate;
mod overlapped;
mod read_directory_changes;

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fs, io,
    ops::DerefMut,
    panic,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use super::{
    registry::Watches, Audience, BackendError, BackendEvent, BackendEventContext, BackendEventHandler, FileType, ParentPolicy, Scope,
    SubscriptionId, WatcherBackend,
};
use crate::{
    options::WindowsOptions,
    util::{
        id_source::IdSource,
        lifetimes::{Lifetime, LifetimeDefinition},
        result_util::ResultExt,
    },
};

use device_watcher::{DeviceId, DeviceWatcher, DeviceWatcherMessage};
use drive_prefix::{extract_prefix, DrivePrefix};
use file_handle::FileHandle;
use futures::{channel::mpsc, StreamExt};
use get_volume_filesystem_name::get_volume_filesystem_name;
use overlapped::{OverlappedBuffer, OverlappedIo};
use read_directory_changes::{
    create_file, file_type_from_flags, read_directory_changes_ex, read_file_notify_information, FileAction, NotifyInformationClass,
    UnpackedNotification, READ_DIRECTORY_CHANGES_BUFFER_LAYOUT,
};
use tracing::{error, info, instrument, trace, trace_span, warn};
use windows::Win32::{Foundation::ERROR_INVALID_FUNCTION, System::IO::CancelIo};

/*
Rationale:
    The API we provide allows clients to watch arbitrary paths, e.g. a particular text document.
    While we hold a handle for a file, Windows won't allow anyone to rename its parents.
    What's worse, the user is not told the reason, only a general message like "the directory is in use".
    Watching C:\ allows us to work around this.
*/

// TODO change journals? https://blog.trailofbits.com/2020/03/16/real-time-file-monitoring-on-windows-with-osquery/

pub struct WindowsBackend {
    shared: Arc<Shared>,
    lifetime_def: LifetimeDefinition,
    thread_handle: thread::JoinHandle<()>,
}

impl WindowsBackend {
    pub fn create(options: WindowsOptions, callback: BackendEventHandler) -> Self {
        let WindowsOptions { muted_paths } = options;
        let lifetime_def = LifetimeDefinition::new();
        let (io_sender, io_receiver) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState {
                id_source: IdSource::new(),
                drives_by_id: HashMap::default(),
                drives_by_name: HashMap::default(),
                pending_io: HashMap::default(),
                io_events: io_sender,
            }),
        });
        let thread_handle = {
            let shared = shared.clone();
            thread::Builder::new()
                .name(String::from("fsevents-iocp"))
                .spawn({
                    let lifetime = lifetime_def.lifetime();
                    || futures::executor::block_on(event_loop(muted_paths, shared, io_receiver, lifetime, callback))
                })
                .expect("failed to spawn a thread")
        };
        WindowsBackend {
            shared,
            thread_handle,
            lifetime_def,
        }
    }
}

struct Shared {
    state: Mutex<SharedState>,
}

struct SharedState {
    id_source: IdSource,
    drives_by_id: HashMap<DriveId, Drive>,
    drives_by_name: HashMap<DrivePrefix, DriveId>,
    pending_io: HashMap<DriveId, PendingIO>,
    io_events: mpsc::UnboundedSender<IoMessage>,
}

impl WatcherBackend for Shared {
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        let (prefix, _) = extract_prefix(&canonical_path).unwrap(); // TODO
        let mut state = self.state.lock().expect("mutex is poisoned");
        // Enforce the parent policy before allocating a drive, so a rejected watch never spins up
        // drive IO. When the drive does not exist yet there cannot be a watched parent, so a
        // `RequireWatchedParent` request for a non-root path is refused outright.
        match state.drives_by_name.get(&prefix).copied() {
            Some(drive_id) => {
                let drive = state.drives_by_id.get_mut(&drive_id).expect("invalid drive id");
                drive.watches.check_parent_policy(canonical_path, subscription, parent_policy)?;
            }
            None if canonical_path.parent().is_some() && parent_policy == ParentPolicy::RequireWatchedParent => {
                return Err(BackendError::DetachedParent);
            }
            None => {}
        }
        let drive_id = state.ensure_drive(prefix)?;
        let drive = state.drives_by_id.get_mut(&drive_id).expect("invalid drive id");
        let tree = match scope {
            Scope::DirectChildren => &mut drive.watches.direct,
            Scope::Recursive => &mut drive.watches.recursive,
        };
        tree.entry(canonical_path)
            .unwrap() // TODO convert to backend error
            .or_insert_default()
            .subscribe(subscription);

        Ok(())
    }

    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        let (prefix, _) = extract_prefix(canonical_path).unwrap(); // TODO
        let mut state = self.state.lock().expect("mutex is poisoned");
        if let Some(&drive_id) = state.drives_by_name.get(&prefix) {
            let drive = state.drives_by_id.get_mut(&drive_id).expect("invalid drive id");
            let recursive = match scope {
                Scope::DirectChildren => false,
                Scope::Recursive => true,
            };
            let _ = drive
                .watches
                .remove_subscriber(canonical_path, subscription, recursive, |_, _| {}, |_, _| {});
            if drive.watches.is_empty() {
                let drive = state.drives_by_id.remove(&drive_id).expect("invalid drive id");
                state.drives_by_name.remove(&drive.prefix);
                state.cancel_drive_io(&drive_id);
            }
        }
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        let mut state = self.state.lock().expect("mutex is poisoned");
        let mut empty_drives = HashSet::<DriveId>::new();
        state.drives_by_id.iter_mut().for_each(|(drive_id, drive)| {
            drive.watches.destroy_subscription(subscription);
            if drive.watches.is_empty() {
                empty_drives.insert(*drive_id);
            }
        });
        for drive_id in empty_drives {
            if let Some(drive) = state.drives_by_id.remove(&drive_id) {
                state.drives_by_name.remove(&drive.prefix);
                state.cancel_drive_io(&drive_id);
            }
        }
    }

    fn shutdown_and_join(self: Box<Self>) -> thread::Result<()> {
        unreachable!()
    }
}

impl WatcherBackend for WindowsBackend {
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        self.shared.add_watch(canonical_path, subscription, scope, parent_policy)
    }

    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        self.shared.remove_watch(canonical_path, subscription, scope);
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        self.shared.destroy_subscription(subscription);
    }

    fn shutdown_and_join(self: Box<Self>) -> thread::Result<()> {
        self.lifetime_def.terminate();
        self.thread_handle.join()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, std::hash::Hash)]
struct DriveId(std::num::NonZeroU32);

struct EventLoopState {
    shared: Arc<Shared>,
    ignored_paths: Vec<PathBuf>,
    known_removable_drives: HashMap<DeviceId, DrivePrefix>,
    shutdown_requested: bool,
    callback: BackendEventHandler,
}

#[derive(Debug)]
struct Drive {
    prefix: DrivePrefix,
    watches: Watches<(), ()>,
}

#[instrument(skip_all)]
async fn event_loop(
    ignored_paths: Vec<PathBuf>,
    shared: Arc<Shared>,
    mut io_events: mpsc::UnboundedReceiver<IoMessage>,
    lifetime: Lifetime,
    callback: BackendEventHandler,
) {
    let device_watcher_manager = DeviceWatcher::new().expect("Failed to create device watcher manager");
    let mut device_watcher_events = device_watcher_manager.subscribe().fuse();
    let mut state = EventLoopState {
        shared,
        ignored_paths,
        known_removable_drives: HashMap::new(),
        shutdown_requested: false,
        callback,
    };
    let mut shutdown_requested = false;

    enum LoopEvent {
        IoMessage(IoMessage),
        DeviceWatcherMessage(DeviceWatcherMessage),
        Terminated,
        ShutdownRequested,
    }
    loop {
        let next = if shutdown_requested {
            io_events.next().await.map_or(LoopEvent::Terminated, LoopEvent::IoMessage)
        } else {
            tokio::select! {
                _ = lifetime.terminated() => LoopEvent::ShutdownRequested,
                event = io_events.next() => LoopEvent::IoMessage(event.expect("io_events closed")), // EventLoopState itself holds one of the senders, it won't be closed
                Some(device_watcher_message) = device_watcher_events.next() => LoopEvent::DeviceWatcherMessage(device_watcher_message)
            }
        };
        match next {
            LoopEvent::IoMessage(message) => {
                trace!(?message, "received a message");
                match message {
                    IoMessage::IoCompleted { drive_id } => {
                        state.receive_packet(&drive_id);
                    }
                    IoMessage::Empty { drive_id } => {
                        trace!(?drive_id, "received completion notification");
                        // Now it's safe to de-allocate the buffer.
                        state.completion_notification(&drive_id);
                    }
                    IoMessage::IoFailed { drive_id, err } => {
                        state.completed_with_error(&drive_id, err);
                    }
                    IoMessage::IoCancelled { drive_id } => {
                        trace!(?drive_id, "received IO cancellation notification");
                        state.cancellation_notification(&drive_id);
                    }
                    IoMessage::IoPanicked { drive_id, panic } => {
                        error!(?drive_id, "IO has panicked, unwinding");
                        panic::resume_unwind(panic);
                    }
                }
            }
            LoopEvent::DeviceWatcherMessage(message) => match message {
                DeviceWatcherMessage::Event(devices) => {
                    state.device_watcher_message_received(devices);
                }
                DeviceWatcherMessage::Panic(panic) => {
                    error!("Device watcher has panicked, unwinding");
                    let panic = panic.lock().expect("mutex").take().expect("panic");
                    panic::resume_unwind(panic);
                }
            },
            LoopEvent::ShutdownRequested => {
                trace!("received shutdown request, cancelling all pending IO operations");
                state.abort_everything();
                shutdown_requested = true;
            }

            LoopEvent::Terminated => {
                warn!("Channel has been closed, breaking event loop");
                break;
            }
        }
        if shutdown_requested {
            let state = state.shared.state.lock().expect("mutex is poisoned");
            if state.pending_io.is_empty() {
                trace!("No outstanding calls left, closing IOCP");
                break;
            } else {
                trace!(state.pending.len = state.pending_io.len(), "pending io operations before shutdown");
            }
        }
    }
}

impl SharedState {
    #[instrument(skip(self))]
    fn ensure_drive(&mut self, prefix: DrivePrefix) -> io::Result<DriveId> {
        if let Some(existing) = self.drives_by_name.get(&prefix) {
            trace!(?existing, "found existing drive");
            Ok(*existing)
        } else {
            trace!("initializing a new drive");
            let drive_id = DriveId(self.id_source.next());

            let filesystem = get_volume_filesystem_name(&prefix)
                .log(|e| warn!(error = ?e, "failed to recognise filesystem"))
                .ok();
            watch_drive(&self.io_events, drive_id, &prefix, NotifyInformationClass::ExtendedInformation)
                .log(|err| warn!(error = ?err, "failed to watch drive prefix"))
                .map(|pending_op| {
                    info!(
                        ?filesystem,
                        notify_information_class = ?pending_op.notify_information_class,
                        ?prefix,
                        "watching a drive",
                    );

                    self.pending_io.insert(drive_id, pending_op);
                    self.drives_by_id.insert(
                        drive_id,
                        Drive {
                            prefix: prefix.clone(),
                            watches: Watches::default(),
                        },
                    );
                    self.drives_by_name.insert(prefix, drive_id);
                    drive_id
                })
        }
    }

    #[instrument(skip(self))]
    fn cancel_drive_io(&self, drive_id: &DriveId) {
        if let Some(pending_op) = self.pending_io.get(drive_id) {
            trace!("Canceling pending IO for the drive");
            pending_op.cancel_io();
        }
    }
}

#[derive(Debug)]
struct DriveEvent {
    absolute_path: PathBuf,
    event: BackendEvent,
    audience: Audience,
}

impl EventLoopState {
    #[instrument(skip(self))]
    fn receive_packet(&mut self, drive_id: &DriveId) {
        let mut state = self.shared.state.lock().expect("mutex is poisoned");

        let SharedState {
            pending_io, drives_by_id, ..
        } = &mut state.deref_mut();
        if let Some(pending) = pending_io.get_mut(drive_id) {
            let notifications = unsafe { read_file_notify_information(pending.overlapped.get_buffer(), pending.notify_information_class) };
            trace!(?drive_id, "received notifications: {notifications:#?}");

            if let Some(drive) = drives_by_id.get(drive_id) {
                if let Err(err) =
                    read_directory_changes_ex(&pending.handle, true, &pending.overlapped, &mut pending.notify_information_class)
                {
                    // TODO mark the drive failed? monitor drives? notify clients?
                    pending_io.remove(drive_id);
                    warn!(error = ?err, ?drive.prefix, "ReadDirectoryChangesExW has failed");
                };
            } else {
                trace!(?drive_id, "received packet for removed drive, dropping");
                pending_io.remove(drive_id);
            }

            // TODO conflate, it's useless without debounce
            // let events = conflate_events_by(|(event, _)| event, relevant_notifications.map(|(path, n)| event(path, n)));
            let events = if let Some(drive) = state.drives_by_id.get_mut(drive_id) {
                notifications
                    .into_iter()
                    .map(|n| n.unpack())
                    .filter_map(|n| {
                        let full_file_path = Path::new(drive.prefix.as_os_str()).join(&n.file_path);
                        let subscriptions = drive
                            .watches
                            .query(&full_file_path)
                            .log(|err| warn!(?err, "query failed"))
                            .unwrap_or_default();
                        if matches!(n.action, FileAction::FILE_ACTION_REMOVED | FileAction::FILE_ACTION_RENAMED_OLD_NAME) {
                            drive.watches.remove_path(&full_file_path);
                        }
                        if subscriptions.is_empty() {
                            None
                        } else {
                            if self.ignored_paths.iter().any(|ignored| full_file_path.starts_with(ignored)) {
                                None
                            } else {
                                Some(event(full_file_path, Audience::Some(subscriptions), n))
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            trace!("converted to events: {events:#?}");

            // don't forget to drop the lock
            drop(state);
            for DriveEvent {
                absolute_path,
                event,
                audience,
            } in events
            {
                if let BackendEvent::Changed { file_type, .. } = &event {
                    if *file_type == FileType::Directory {
                        // TODO AAAAAAAAAAA
                        // Any modification of a file emits an event for its parent,
                        // and looking at FILE_ITEM_MODIFIED I can't know whether this is a false positive event or a real change, e.g. to permissions.
                        // Dispatching such events would mean a constant flow of events for the watch roots below,
                        // so I choose to drop them for now.
                        continue;
                    }
                }

                let ctx = BackendEventContext {
                    audience: &audience,
                    event,
                    event_path: absolute_path.as_path(),
                    backend: self.shared.as_ref(),
                };
                (self.callback)(ctx);
            }
        } else {
            error!("received packet for unknown completion key");
        }
    }

    #[instrument(skip(self))]
    fn rescan_drive(&mut self, drive_path: PathBuf) {
        let audience = Audience::All;
        let ctx = BackendEventContext {
            audience: &audience,
            event: BackendEvent::Ambiguous,
            event_path: drive_path.as_path(),
            backend: self.shared.as_ref(),
        };
        (self.callback)(ctx);
    }

    #[instrument(skip(self))]
    fn completion_notification(&mut self, drive_id: &DriveId) {
        // It's either a completion notification for a closed handle
        // or a sudden buffer overflow.
        let mut state = self.shared.state.lock().expect("mutex is poisoned");

        let SharedState {
            pending_io, drives_by_id, ..
        } = &mut state.deref_mut();
        if let Some(pending_op) = pending_io.get_mut(drive_id) {
            if let Some(drive) = drives_by_id.get(drive_id) {
                warn!(?drive.prefix, "received buffer overflow notification");

                let drive_path = drive.prefix.to_path_buf();
                // TODO don't know if we are allowed to reuse the handle
                if let Err(err) = read_directory_changes_ex(
                    &pending_op.handle,
                    true,
                    &pending_op.overlapped,
                    &mut pending_op.notify_information_class,
                ) {
                    pending_io.remove(drive_id);
                    warn!(error = ?err, ?drive.prefix, "ReadDirectoryChangesExW has failed");
                }
                drop(state);
                self.rescan_drive(drive_path);
            } else {
                trace!(?drive_id, "received completion notification for removed drive");
                state.pending_io.remove(drive_id);
            }
        }
    }

    fn completed_with_error(&mut self, drive_id: &DriveId, error: io::Error) {
        let mut state = self.shared.state.lock().expect("mutex is poisoned");
        let SharedState {
            pending_io, drives_by_id, ..
        } = &mut state.deref_mut();
        if let Some(pending_op) = pending_io.get_mut(drive_id) {
            if let Some(drive) = drives_by_id.get(drive_id) {
                if error.raw_os_error() == Some(ERROR_INVALID_FUNCTION.0 as i32)
                    && matches!(pending_op.notify_information_class, NotifyInformationClass::ExtendedInformation)
                {
                    // Retry the operation with a lower information class.
                    warn!(?error, "reading directory changes failed, downgrading the information class");
                    pending_op.notify_information_class = NotifyInformationClass::Information;

                    if read_directory_changes_ex(
                        &pending_op.handle,
                        true,
                        &pending_op.overlapped,
                        &mut pending_op.notify_information_class,
                    )
                    .inspect_err(|err| warn!(error = ?err, "retrying read_directory_changes_ex has failed"))
                    .is_ok()
                    {
                        // The retry has been queued successfully.
                        return;
                    }
                }

                // We did everything we could, just remove the watch.
                pending_io.remove(drive_id);
                // TODO mark the drive failed? monitor drives? notify clients?
                warn!(?error, ?drive.prefix, "IO operation has failed");
            }
        }
    }

    #[instrument(skip(self))]
    fn cancellation_notification(&mut self, drive_id: &DriveId) {
        let mut state = self.shared.state.lock().expect("mutex is poisoned");
        trace!(?drive_id, "received cancellation notification for the drive");
        state.pending_io.remove(drive_id);
        if let Some(drive) = state.drives_by_id.get(drive_id) {
            info!(?drive.prefix, "received cancellation notification");
            let drive_path = drive.prefix.to_path_buf();
            drop(state);
            self.rescan_drive(drive_path);
        }
    }

    #[instrument(skip(self))]
    fn device_watcher_message_received(&mut self, devices: HashMap<DeviceId, DrivePrefix>) {
        let removed_devices: HashMap<_, _> = self
            .known_removable_drives
            .iter()
            .filter(|&(device_id, _)| !devices.contains_key(device_id))
            .map(|(device_id, prefix)| (device_id.clone(), prefix.clone()))
            .collect();
        let added_devices: HashMap<_, _> = devices
            .into_iter()
            .filter(|(device_id, _)| !self.known_removable_drives.contains_key(device_id))
            .collect();

        trace!(?added_devices, ?removed_devices, "Device watcher message received");

        let mut state = self.shared.state.lock().expect("mutex is poisoned");

        let SharedState {
            pending_io,
            drives_by_id,
            drives_by_name,
            io_events,
            ..
        } = &mut state.deref_mut();
        for (removed_id, removed_path) in removed_devices {
            let _span = trace_span!("removed_devices", path = %removed_path.display(), ?removed_id).entered();
            // TODO shouldn't it be removed from drives_by_name?
            if let Some(drive_id) = drives_by_name.get(&removed_path) {
                trace!("Canceling removed drive IO");
                if let Some(pending_op) = pending_io.get(drive_id) {
                    trace!("Canceling pending IO for the drive");
                    pending_op.cancel_io();
                }
            }
            self.known_removable_drives.remove(&removed_id);
        }
        let mut drives_to_rescan = HashSet::<PathBuf>::new();
        for (added_id, added_path) in added_devices {
            let _span = trace_span!("added_devices", path = %added_path.display(), ?added_id).entered();
            if let Some(&drive_id) = drives_by_name.get(&added_path) {
                if let Some(drive) = drives_by_id.get(&drive_id) {
                    trace!("Drive exists, restarting monitoring");
                    match watch_drive(&io_events, drive_id, &added_path, NotifyInformationClass::ExtendedInformation) {
                        Ok(new_pending) => {
                            pending_io.insert(drive_id, new_pending);
                            drives_to_rescan.insert(drive.prefix.to_path_buf());
                        }
                        Err(err) => {
                            error!(drive = ?added_path, ?err, "failed to restart the watcher for drive");
                        }
                    }
                }
            }
            self.known_removable_drives.insert(added_id, added_path);
        }
        drop(state);
        for drive_path in drives_to_rescan {
            self.rescan_drive(drive_path);
        }
    }

    fn abort_everything(&mut self) {
        self.shutdown_requested = true;
        let mut state = self.shared.state.lock().expect("mutex is poisoned");
        for drive_id in state.drives_by_id.keys() {
            state.cancel_drive_io(drive_id);
        }
        state.drives_by_id.clear();
        state.drives_by_name.clear();
        self.known_removable_drives.clear();
    }
}

pub struct PendingIO {
    overlapped: OverlappedIo,
    handle: FileHandle,
    notify_information_class: NotifyInformationClass,
}

impl PendingIO {
    fn cancel_io(&self) {
        unsafe { CancelIo(self.handle.as_handle()) }.unwrap_or_else(|err| error!(?err, "Failed to cancel drive IO"));
    }
}

fn watch_drive(
    sender: &mpsc::UnboundedSender<IoMessage>,
    drive_id: DriveId,
    prefix: &DrivePrefix,
    notify_information_class: NotifyInformationClass,
) -> io::Result<PendingIO> {
    let handle = create_file(Path::new(&prefix))?;

    // ReadDirectoryChangesEx requires a buffer aligned at the DWORD (u32) size boundary (4).
    //
    // Passing a misaligned pointer will cause the function to return an error:
    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesexw
    let buffer = OverlappedBuffer::alloc(READ_DIRECTORY_CHANGES_BUFFER_LAYOUT);
    let overlapped = OverlappedIo::new(&handle, drive_id, buffer, sender.clone())?;

    let mut pending_op = PendingIO {
        overlapped,
        handle,
        notify_information_class,
    };

    read_directory_changes_ex(
        &pending_op.handle,
        true,
        &pending_op.overlapped,
        &mut pending_op.notify_information_class,
    )?;

    Ok(pending_op)
}

fn event(full_file_path: PathBuf, audience: Audience, n: UnpackedNotification) -> DriveEvent {
    let UnpackedNotification { action, file_type, .. } = n;

    DriveEvent {
        event: match action {
            FileAction::FILE_ACTION_ADDED | FileAction::FILE_ACTION_RENAMED_NEW_NAME => {
                BackendEvent::RecentlyCreated {
                    // TODO don't force yourself
                    file_type: file_type.unwrap_or_else(|| {
                        fs::symlink_metadata(&full_file_path)
                            .map(|m| file_type_from_flags(m.is_dir(), m.is_symlink()))
                            .unwrap_or_else(|e| {
                                trace!(error = ?e, "failed to obtain file metadata");
                                FileType::Regular
                            })
                    }),
                }
            }
            FileAction::FILE_ACTION_MODIFIED => {
                BackendEvent::Changed {
                    // TODO don't force yourself
                    file_type: file_type.unwrap_or_else(|| {
                        fs::symlink_metadata(&full_file_path)
                            .map(|m| file_type_from_flags(m.is_dir(), m.is_symlink()))
                            .unwrap_or_else(|e| {
                                trace!(error = ?e, "failed to obtain file metadata");
                                FileType::Regular
                            })
                    }),
                }
            }
            FileAction::FILE_ACTION_RENAMED_OLD_NAME | FileAction::FILE_ACTION_REMOVED => BackendEvent::Removed,
        },
        audience,
        absolute_path: full_file_path,
    }
}

#[derive(Debug)]
enum IoMessage {
    IoCompleted {
        drive_id: DriveId,
    },
    IoFailed {
        drive_id: DriveId,
        err: io::Error,
    },
    IoCancelled {
        drive_id: DriveId,
    },
    IoPanicked {
        drive_id: DriveId,
        panic: Box<dyn Any + Send + 'static>,
    },
    Empty {
        drive_id: DriveId,
    },
}
