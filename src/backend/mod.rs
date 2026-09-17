mod error;
#[cfg(test)]
pub mod fake;
mod file_type;
pub mod immediate;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
mod registry;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(test)]
mod test;

use std::{
    collections::HashSet,
    path::Path,
};

pub use error::BackendError;
pub use file_type::FileType;
pub use registry::SubscriptionId;

use crate::options::BackendOptions;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    DirectChildren,
    Recursive,
}

/// Controls whether [`WatcherBackend::add_watch`] verifies that the watch is being attached to an
/// already-watched chain.
///
/// The backend holds the entire canonical watch tree, so it is the only component that can tell
/// whether the parent of a canonical path is genuinely watched. The session relies on this to catch
/// gaps in the chain: when the client asks to watch a symbolic path, the topmost known symbolic
/// prefix is canonicalized and the suffix is appended verbatim, but an *unregistered* symlink hiding
/// in that suffix would make the resulting canonical path wrong.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParentPolicy {
    /// Attach the watch unconditionally.
    ///
    /// Required whenever a watched parent cannot exist yet: the first watch of a chain (roots and symlink traversal).
    Unchecked,
    /// Refuse to attach the watch unless the parent of `canonical_path` is already watched by the same `subscription_id`.
    ///
    /// A path without a parent (a root prefix / drive) is always accepted.
    RequireWatchedParent,
}

/// Common interface for platform-specific implementations.
///
/// Handling of symlinks is out of scope. All paths are assumed to be canonical.
pub trait WatcherBackend {
    /// Registers a subscription for the directory at `canonical_path` with the given scope.
    ///
    /// `canonical_path` is assumed to be a normalized absolute canonical path to an existing directory.
    /// Implementations are allowed to return an error if `canonical_path` does not exist, but they are not required to proactively check for existence.
    ///
    /// [subscription_id] controls the lifetime of the subscription.
    /// Multiple watches can use the same subscription id if they are destroyed together.
    ///
    /// It is idempotent: adding a watch with the same `subscription_id` and path is safe.
    /// The same path can be watched several times with a different `subscription_id`.
    ///
    /// If a directory is deleted, all the watches inside it are destroyed recursively. It is not necessary to explicitly destroy them in that case (but it is safe to do so).
    ///
    /// When `parent_policy` is [`ParentPolicy::RequireWatchedParent`], the watch is installed only
    /// if the parent of `canonical_path` is already watched by `subscription_id`; otherwise [`BackendError::DetachedParent`] is returned and nothing is registered.
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription_id: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError>;

    /// Removes the subscription registration for `canonical_path` and scope.
    ///
    /// If there are no other retainers, the watches will be destroyed.
    /// NB: if `subscription_id` itself has registered a recursive watch for an ancestor of `canonical_path`, no watches are destroyed while it is alive.
    fn remove_watch(&self, canonical_path: &Path, subscription_id: SubscriptionId, scope: Scope);

    /// Removes the subscription from all watch registrations and destroys any records that no longer have subscribers.
    ///
    /// This operation is idempotent and cannot fail.
    ///
    /// Because it is invoked on the drop of a subscription, the implementation must never panic.
    fn destroy_subscription(&self, subscription_id: SubscriptionId);

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()>;
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum BackendEvent {
    // TODO separate event for metadata? is it supported by everyone? it exists on linux and macos
    /// Emitted when the metadata of a file or a directory is changed, or when the content of a file is changed.
    /// Be aware that it is possible to overwrite a file with a symlink and vice versa; the only event produced in this case is Changed with the updated FileType.
    Changed {
        file_type: FileType,
    },
    /// It might be useful to distinguish between changes to a file and its creation, because we might save some IO this way.
    /// Unfortunately, macOS has a race; all changes to a recently created file are marked with kFSEventStreamEventFlagItemCreated.
    // TODO delete, not worth it, explain why it is a bad idea. Created might be issued on top of an existing file.
    RecentlyCreated {
        file_type: FileType,
    },
    /// The entry is no longer present at the path.
    /// It might mean that the file was physically deleted from the filesystem, but it is also used when an entry is moved to a different path or unmounted.
    Removed,
    /// There is not enough information to tell what has happened.
    /// For example, macOS can emit an event with the kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemCreated bit mask.
    /// If it is a file, we can pick an event to emit based on the current filesystem state, but if it is a directory, we cannot make any assumptions about its content.
    Ambiguous,
    /// Every operating system imposes a strict limit on the number of buffered events. If this buffer is filled, some events are lost.
    Overflow,
}

#[derive(Debug, Clone)]
pub enum Audience {
    All,
    Some(HashSet<SubscriptionId>),
}

impl Audience {
    pub fn contains(&self, subscription_id: SubscriptionId) -> bool {
        match self {
            Audience::All => true,
            Audience::Some(subscriptions) => subscriptions.contains(&subscription_id),
        }
    }
}

pub struct BackendEventContext<'a> {
    pub audience: &'a Audience,
    // TODO none of the backends separate the available information by the event kind, flattening it and making it optional won't force them to query metadata if they are not sure
    // pub file_type: Option<FileType>,
    pub event: BackendEvent,
    /// Always a canonical path.
    pub event_path: &'a Path,
    pub backend: &'a dyn WatcherBackend,
}

pub type BackendEventHandler = Box<dyn for<'a> FnMut(BackendEventContext<'a>) + Send + 'static>;

#[allow(unreachable_code)]
pub fn default_backend(options: BackendOptions, callback_fn: BackendEventHandler) -> anyhow::Result<impl WatcherBackend> {
    #[cfg(target_os = "macos")]
    {
        return Ok(macos::FSEventStreamBackend::create(options.macos, callback_fn)?);
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(linux::INotifyBackend::create(options.linux, callback_fn)?);
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(windows::WindowsBackend::create(options.windows, callback_fn));
    }
    todo!("only macos, linux and windows targets are supported")
}
