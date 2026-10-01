use super::drive_prefix::DrivePrefix;
use crate::util::{option_util::OptionExt, tokio_util::TokioStreamExt, BoxedStream};
use std::{
    any::Any,
    collections::{hash_map::Entry, HashMap},
    ffi::OsString,
    panic,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use tracing::{error, instrument, trace, warn};
use windows::{
    core::HSTRING,
    Devices::{
        Enumeration::{DeviceClass, DeviceInformation, DeviceInformationUpdate, DeviceWatcher as WinDeviceWatcher, DeviceWatcherStatus},
        Portable::StorageDevice,
    },
    Foundation::TypedEventHandler,
    Win32::Foundation::E_FAIL,
};

#[derive(Debug, Clone, Eq, PartialEq, std::hash::Hash)]
pub struct DeviceId(OsString);

pub struct DeviceWatcher {
    watcher: WinDeviceWatcher,
    watcher_callbacks: DeviceWatcherCallbacks,
    receiver: watch::Receiver<DeviceWatcherMessage>,
}

#[derive(Debug, Clone)]
pub enum DeviceWatcherMessage {
    Event(HashMap<DeviceId, DrivePrefix>),
    Panic(Arc<Mutex<Option<Box<dyn Any + Send + 'static>>>>),
}

struct DeviceWatcherCallbacks {
    added: i64,
    updated: i64,
    removed: i64,
}

impl DeviceWatcher {
    pub fn new() -> windows::core::Result<Self> {
        let (sender, receiver) = watch::channel(DeviceWatcherMessage::Event(HashMap::<DeviceId, DrivePrefix>::new()));

        let watcher = DeviceInformation::CreateWatcherDeviceClass(DeviceClass::PortableStorageDevice)?;
        let watcher_callbacks = setup_device_watcher_callbacks(&watcher, &sender)?;
        watcher.Start()?;

        Ok(Self {
            watcher,
            watcher_callbacks,
            receiver,
        })
    }

    pub fn subscribe(&self) -> BoxedStream<DeviceWatcherMessage> {
        self.receiver.clone().into_stream()
    }

    #[instrument(skip(self))]
    fn shutdown(&self) {
        // see the Device Watcher lifecycle: https://learn.microsoft.com/en-us/uwp/api/windows.devices.enumeration.devicewatcher#remarks
        if let Ok(DeviceWatcherStatus::Started | DeviceWatcherStatus::EnumerationCompleted) = self.watcher.Status() {
            let DeviceWatcherCallbacks { added, updated, removed } = self.watcher_callbacks;
            self.watcher
                .Stop()
                .unwrap_or_else(|err| error!(error = ?err, "failed to stop the device watcher"));
            self.watcher
                .RemoveAdded(added)
                .unwrap_or_else(|err| error!(error = ?err, "failed to unsubscribe from the Added event"));
            self.watcher
                .RemoveUpdated(updated)
                .unwrap_or_else(|err| error!(error = ?err, "failed to unsubscribe from the Updated event"));
            self.watcher
                .RemoveRemoved(removed)
                .unwrap_or_else(|err| error!(error = ?err, "failed to unsubscribe from the Removed event"));
        }
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.shutdown()
    }
}

#[derive(Debug, Clone)]
#[repr(transparent)]
struct DeviceWatcherMessenger(watch::Sender<DeviceWatcherMessage>);

impl DeviceWatcherMessenger {
    fn new(sender: &watch::Sender<DeviceWatcherMessage>) -> Self {
        Self(sender.clone())
    }

    fn on_added(&self, device_id: DeviceId, drive_prefix: DrivePrefix) {
        self.0.send_if_modified(|message| match message {
            DeviceWatcherMessage::Event(devices) => match devices.entry(device_id) {
                Entry::Occupied(mut entry) => {
                    if entry.get() == &drive_prefix {
                        false
                    } else {
                        *entry.get_mut() = drive_prefix;
                        true
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(drive_prefix);
                    true
                }
            },
            DeviceWatcherMessage::Panic(_) => false,
        });
    }

    fn on_removed(&self, device_id: DeviceId) {
        self.0.send_if_modified(|message| match message {
            DeviceWatcherMessage::Event(devices) => devices.remove(&device_id).is_some(),
            DeviceWatcherMessage::Panic(_) => false,
        });
    }

    fn event(&self, evt: Option<DeviceWatcherEvent>) {
        match evt {
            Some(DeviceWatcherEvent::Added { device_id, drive_prefix }) => self.on_added(device_id, drive_prefix),
            Some(DeviceWatcherEvent::Removed { device_id }) => self.on_removed(device_id),
            None => { /* we've already logged the error, nothing to do here */ }
        }
    }

    fn error(&self, site: &'static str, err: Box<dyn Any + Send + 'static>) {
        error!("panic has occurred in the DeviceWatcher::{site} event handler");
        self.0.send_replace(DeviceWatcherMessage::Panic(Arc::new(Mutex::new(Some(err)))));
    }
}

enum DeviceWatcherEvent {
    Added { device_id: DeviceId, drive_prefix: DrivePrefix },
    Removed { device_id: DeviceId },
}

impl DeviceWatcherEvent {
    fn added(id: HSTRING, path: HSTRING) -> Option<Self> {
        let path = path.to_os_string();
        if path.is_empty() {
            warn!(?id, "the device doesn't have a path, skipping");
            return None;
        }

        let device_id = DeviceId(id.into());
        let drive_prefix = DrivePrefix::from_os_string(path);

        Some(Self::Added { device_id, drive_prefix })
    }

    fn removed(id: HSTRING) -> Self {
        Self::Removed {
            device_id: DeviceId(id.into()),
        }
    }
}

fn setup_device_watcher_callbacks(
    watcher: &WinDeviceWatcher,
    sender: &watch::Sender<DeviceWatcherMessage>,
) -> windows::core::Result<DeviceWatcherCallbacks> {
    let messenger = DeviceWatcherMessenger::new(sender);
    let added = watcher.Added(&TypedEventHandler::<WinDeviceWatcher, DeviceInformation>::new(move |_, info| {
        let evt = panic::catch_unwind(|| {
            let info = info
                .as_ref()
                .inspect_none(|| error!("added device doesn't have any DeviceInformation, skipping"))?;
            let id = info
                .Id()
                .inspect_err(|err| warn!(error = ?err, "added device doesn't have an ID, skipping"))
                .ok()?;
            let folder = StorageDevice::FromId(&id)
                .inspect_err(|err| warn!(error = ?err, "added device doesn't have a mount dir, skipping"))
                .ok()?;
            let path = folder
                .Path()
                .inspect_err(|err| warn!(error = ?err, "added device doesn't have a mount path, skipping"))
                .ok()?;
            trace!(?id, ?path, "device added");
            DeviceWatcherEvent::added(id, path)
        })
        .map_err(|err| {
            messenger.error("Added", err);
            windows::core::Error::from_hresult(E_FAIL)
        })?;

        Ok(messenger.event(evt))
    }))?;

    let messenger = DeviceWatcherMessenger::new(sender);
    let updated = watcher.Updated(&TypedEventHandler::<WinDeviceWatcher, DeviceInformationUpdate>::new(
        move |_, info| {
            let evt = panic::catch_unwind(|| {
                let info = info
                    .as_ref()
                    .inspect_none(|| error!("updated device doesn't have any DeviceInformationUpdate, skipping"))?;
                let id = info
                    .Id()
                    .inspect_err(|err| warn!(error = ?err, "updated device doesn't have an ID, skipping"))
                    .ok()?;
                match StorageDevice::FromId(&id) {
                    Ok(folder) => {
                        let path = folder
                            .Path()
                            .inspect_err(|err| warn!(error = ?err, "updated device doesn't have a mount path, skipping"))
                            .ok()?;
                        trace!(?id, ?path, "device updated");
                        DeviceWatcherEvent::added(id, path)
                    }
                    Err(err) => {
                        warn!(error = ?err, "updated device doesn't have a mount dir, removing it");
                        Some(DeviceWatcherEvent::removed(id))
                    }
                }
            })
            .map_err(|err| {
                messenger.error("Updated", err);
                windows::core::Error::from_hresult(E_FAIL)
            })?;

            Ok(messenger.event(evt))
        },
    ))?;

    let messenger = DeviceWatcherMessenger::new(sender);
    let removed = watcher.Removed(&TypedEventHandler::<WinDeviceWatcher, DeviceInformationUpdate>::new(
        move |_, info| {
            let evt = panic::catch_unwind(|| {
                let info = info
                    .as_ref()
                    .inspect_none(|| error!("removed device doesn't have any DeviceInformationUpdate, skipping"))?;
                let id = info
                    .Id()
                    .inspect_err(|err| warn!(error = ?err, "removed device doesn't have an ID, skipping"))
                    .ok()?;
                trace!(?id, "device removed");
                Some(DeviceWatcherEvent::removed(id))
            })
            .map_err(|err| {
                messenger.error("Removed", err);
                windows::core::Error::from_hresult(E_FAIL)
            })?;

            Ok(messenger.event(evt))
        },
    ))?;

    Ok(DeviceWatcherCallbacks { added, updated, removed })
}
