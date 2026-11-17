use std::time::Duration;

pub use crate::util::Debug;

#[derive(Debug)]
pub struct WatcherOptions {
    pub client_buffer_size: usize,
    pub backend: BackendOptions,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        Self {
            client_buffer_size: 4096,
            backend: BackendOptions::default(),
        }
    }
}

#[derive(Default, Debug)]
pub struct BackendOptions {
    pub windows: WindowsOptions,
    pub macos: MacosOptions,
    pub linux: LinuxOptions,
}

#[derive(Debug)]
pub struct MacosOptions {
    pub fs_event_stream_latency: Duration,
    pub report_rescan: Option<Debug<Box<dyn FnMut() + Send>>>,
}

impl Default for MacosOptions {
    fn default() -> Self {
        Self {
            report_rescan: None,
            fs_event_stream_latency: Duration::from_millis(10),
        }
    }
}

/// INotify requires a file descriptor to add a watch, and the OS limits the total number of open file descriptors.
/// The default depends on the distribution, but in general it is rather low. Eventually we are going to hit it.
/// This struct controls the handling of such a situation.
pub struct UlimitStrategy(#[allow(unused)] Box<dyn FnMut() + Send>);

impl UlimitStrategy {
    /// The event loop will panic. Nothing works afterwards. The panic might be propagated to the clients via a poisoned mutex.
    pub fn panic() -> Self {
        Self(Box::new(|| panic!("Too many open files")))
    }

    /// Well, some watches have been added and they will continue to report events.
    /// Trying to add a new one will fail with an IO error (`BackendError::IO`).
    pub fn carry_on() -> Self {
        Self(Box::new(|| {}))
    }

    /// Invokes the callback *once* and then behaves the same as [UlimitStrategy::carry_on].
    ///
    /// Reporting the error to a human is the only thing that can fix the situation.
    /// It will not help the current instance though.
    /// Theoretically, we could wait for the response and start from scratch, but there is no demand for that right now.
    pub fn report_and_carry_on(f: impl FnMut() + Send + 'static) -> Self {
        Self(Box::new(f))
    }
}

#[derive(Debug)]
pub struct LinuxOptions {
    pub ulimit_strategy: Debug<UlimitStrategy>,
}

impl Default for LinuxOptions {
    fn default() -> Self {
        Self {
            ulimit_strategy: Debug::wrap(UlimitStrategy::carry_on()),
        }
    }
}

#[derive(Default, Debug)]
pub struct WindowsOptions {}

pub struct Builder {
    options: WatcherOptions,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            options: WatcherOptions::default(),
        }
    }

    /// The shared event loop will dispatch all incoming fs events to the corresponding consumer.
    /// A slow consumer should not affect others. As such, each of them has a separate buffer configured by this property.
    ///
    /// Configured to some reasonable value by default.
    pub fn client_buffer_size(mut self, size: usize) -> Self {
        self.options.client_buffer_size = size;
        self
    }

    /// See [UlimitStrategy].
    pub fn linux_on_ulimit(mut self, value: UlimitStrategy) -> Self {
        self.options.backend.linux.ulimit_strategy = Debug::wrap(value);
        self
    }

    /// The handler will be invoked each time FsEventStream receives "kFSEventStreamEventFlagMustScanSubDirs"
    ///
    /// It doesn't change the strategy. It is meant only as a reporter.
    pub fn macos_on_rescan(mut self, reporter: impl FnMut() + Send + 'static) -> Self {
        self.options.backend.macos.report_rescan = Some(Debug::wrap(Box::new(reporter)));
        self
    }

    /// Configures the `latency` parameter of `FSEventStreamCreate`.
    ///
    /// NB: it is created with the `kFSEventStreamCreateFlagNoDefer` flag.
    pub fn macos_latency(mut self, latency: Duration) -> Self {
        self.options.backend.macos.fs_event_stream_latency = latency;
        self
    }

    pub fn build(self) -> WatcherOptions {
        self.options
    }
}

impl From<Builder> for WatcherOptions {
    fn from(value: Builder) -> Self {
        value.options
    }
}
