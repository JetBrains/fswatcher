use std::{
    fmt, mem,
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
    sync::{atomic::AtomicU32, Arc},
    thread,
};

use anyhow::Context;
use libc::{O_EVTONLY, O_SYMLINK};
use nix::{
    errno::Errno,
    sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue},
};
use nix::{
    fcntl::{open, OFlag},
    sys::stat::{fstat, Mode, SFlag},
};
use tracing::{debug, error, info_span, instrument, trace, warn};

use crate::util::result_util::ResultExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KEventId(u32);

pub type Flags = FilterFlag;

pub struct KQueueWorker {
    kq: Arc<Kqueue>,

    pipe_writer: OwnedFd,
    thread_handle: thread::JoinHandle<()>,

    id_source: AtomicU32,
}

impl KQueueWorker {
    #[instrument(skip(callback))]
    pub fn create(mut callback: impl FnMut(KQueueVNodeEvent) + Send + 'static) -> anyhow::Result<KQueueWorker> {
        use nix::unistd::pipe;

        trace!("spawning the kqueue worker");

        let kq = Arc::new(Kqueue::new().context("create kernel queue")?);
        let (pipe_reader, pipe_writer) = pipe().context("create termination signal delivery")?;
        kevent(
            &kq,
            &[KEvent::new(
                pipe_reader.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD | EvFlags::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            )],
            &mut [],
        )
        .context("register termination signal event filter")?;

        let thread_handle = thread::Builder::new()
            .name("kernel-queue-worker".to_owned())
            .spawn({
                let kq = Arc::clone(&kq);
                move || {
                    let _span = info_span!("kernel_queue_worker").entered();

                    let mut events_buffer = Vec::new();
                    events_buffer.resize(1024, unsafe { mem::zeroed() });

                    loop {
                        trace!("waiting for new events");
                        let count = match kevent(&kq, &[], &mut events_buffer[..]) {
                            Ok(count) => count,
                            Err(e) => {
                                // TODO propagate the error to the clients?
                                error!(error = ?e, "kevent syscall has failed");
                                return;
                            }
                        };

                        trace!(count, "received new events");
                        for event in &events_buffer[..count] {
                            if event.ident() == pipe_reader.as_raw_fd() as usize {
                                // We treat any event on the pipe as a termination signal.
                                debug!("terminating kernel queue event loop");
                                return;
                            }

                            if event.flags().contains(EvFlags::EV_ERROR) {
                                let error = Errno::from_raw(event.data() as i32).desc();
                                // TODO does it have a context which we can use to dispatch the error to the client?
                                // Hopefully it doesn't occur on the next kqueue call.
                                warn!(error, descriptor = event.ident(), "erroneous event received");
                                continue;
                            }

                            // If the path with this descriptor ID doesn't exist, it means the watch has
                            // already been dropped.
                            let id = KEventId(event.udata() as u32);
                            let flags = event.fflags();
                            trace!(?id, flags = ?flag_string(flags), "event");
                            callback(KQueueVNodeEvent { flags, id });
                        }
                    }
                }
            })
            .context("kernel queue worker thread")?;

        Ok(KQueueWorker {
            kq,
            pipe_writer,
            thread_handle,
            id_source: AtomicU32::new(0),
        })
    }

    #[instrument(skip_all, fields(canonical_path = %canonical_path.display()))]
    pub fn add_kernel_queue_watch(&self, canonical_path: &Path) -> nix::Result<KQueueWatch> {
        trace!("make_kernel_queue_watch");

        let descriptor = open(canonical_path, OFlag::from_bits_retain(O_EVTONLY | O_SYMLINK), Mode::empty())
            .log(|e| trace!(error = e.desc(), "open failed"))?;
        let file_info = fstat(&descriptor).log(|e| trace!(error = e.desc(), "fstat failed"))?;

        let st_mode = SFlag::from_bits_retain(file_info.st_mode) & SFlag::S_IFMT;
        if st_mode != SFlag::S_IFREG {
            // TODO actually, why not?
            trace!(?st_mode, "cannot watch a non-regular file");
            return Err(Errno::ENOTSUP);
        }

        let filter_flags =
            FilterFlag::NOTE_DELETE | FilterFlag::NOTE_WRITE | FilterFlag::NOTE_EXTEND | FilterFlag::NOTE_RENAME | FilterFlag::NOTE_REVOKE;
        let kevent_id = self.id_source.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let event = KEvent::new(
            descriptor.as_raw_fd() as usize,
            EventFilter::EVFILT_VNODE,
            EvFlags::EV_ADD | EvFlags::EV_CLEAR,
            filter_flags,
            0,
            kevent_id as isize,
        );
        kevent(&self.kq, &[event], &mut [])?;
        Ok(KQueueWatch {
            descriptor,
            id: KEventId(kevent_id),
        })
    }

    pub fn terminate_and_wait(self) -> thread::Result<()> {
        // Let the event loop thread know that it needs to terminate.
        let _ = nix::unistd::write(self.pipe_writer, &[0]);

        self.thread_handle.join()
    }
}

pub struct KQueueVNodeEvent {
    pub flags: Flags,
    pub id: KEventId,
}

const FLAG_NAMES: &[(FilterFlag, &str)] = &[
    (FilterFlag::NOTE_WRITE, "NOTE_WRITE"),
    (FilterFlag::NOTE_DELETE, "NOTE_DELETE"),
    (FilterFlag::NOTE_EXTEND, "NOTE_EXTEND"),
    (FilterFlag::NOTE_ATTRIB, "NOTE_ATTRIB"),
    (FilterFlag::NOTE_LINK, "NOTE_LINK"),
    (FilterFlag::NOTE_RENAME, "NOTE_RENAME"),
    (FilterFlag::NOTE_REVOKE, "NOTE_REVOKE"),
];

fn flag_string(flags: Flags) -> String {
    FLAG_NAMES
        .iter()
        .filter_map(|&(flag, name)| flags.contains(flag).then_some(name))
        .collect::<Vec<_>>()
        .join(" | ")
}

impl fmt::Debug for KQueueVNodeEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter
            .debug_struct("KQueueVNodeEvent")
            .field("id", &self.id)
            .field("flags", &flag_string(self.flags))
            .finish()
    }
}

#[derive(Debug)]
pub struct KQueueWatch {
    /// File descriptor that is registered with the kernel queue.
    ///
    /// Closing the descriptor clears all events associated with it in the kernel queue.
    #[expect(unused, reason = "holds a resource that is dropped when the struct is dropped")]
    descriptor: OwnedFd,
    id: KEventId,
}

impl KQueueWatch {
    pub fn id(&self) -> KEventId {
        self.id
    }
}

fn kevent(kq: &Kqueue, changelist: &[KEvent], eventlist: &mut [KEvent]) -> nix::Result<usize> {
    loop {
        match kq.kevent(changelist, eventlist, None) {
            Ok(count) => break Ok(count),
            Err(Errno::EINTR) => continue,
            Err(e) => break Err(e),
        }
    }
}

#[cfg(test)]
mod test {
    use std::{fs::OpenOptions, io::Write, time::Duration};

    use crate::test_helpers::{delete, enable_logging, files, write_all};

    use super::*;
    use crate::backend::fake::timeout_iterator::timeout_iterator;

    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    struct TestKQueue {
        events: std::sync::mpsc::Receiver<KQueueVNodeEvent>,
        worker: KQueueWorker,
    }

    impl TestKQueue {
        fn collect_events(&self) -> Vec<KQueueVNodeEvent> {
            let vault = files!({ "latch" => "" });
            let latch_path = vault.path().join("latch");
            let klatch = self.worker.add_kernel_queue_watch(&latch_path).unwrap();
            trace!("registered latch {:?}", klatch.id());
            write_all(latch_path, "finish");
            timeout_iterator(&self.events, RECV_TIMEOUT)
                .take_while(|evt| evt.id != klatch.id())
                .collect()
        }
    }

    fn test_kqueue() -> TestKQueue {
        let (events_tx, events_rx) = std::sync::mpsc::channel::<KQueueVNodeEvent>();
        let worker = KQueueWorker::create(move |event| events_tx.send(event).unwrap()).unwrap();
        TestKQueue { worker, events: events_rx }
    }

    fn flags(events: Vec<KQueueVNodeEvent>) -> Vec<Flags> {
        events.into_iter().map(|e| e.flags).collect()
    }

    fn flag_strings(flags: &[Flags]) -> String {
        format!("{:?}", flags.iter().map(|f| flag_string(*f)).collect::<Vec<_>>())
    }

    #[test]
    fn write_to_a_file_without_closing_it() {
        enable_logging();

        let dir = files!({
            "file" => "content",
        });
        let file_path = dir.path().join("file");
        let mut file_handle = OpenOptions::new().create_new(false).write(true).open(&file_path).unwrap();
        let t = test_kqueue();

        let _w = t.worker.add_kernel_queue_watch(&file_path).unwrap();
        file_handle.write_all("new content".as_bytes()).unwrap();

        let events = t.collect_events();
        let expected1 = vec![Flags::NOTE_EXTEND | Flags::NOTE_WRITE];
        let expected2 = vec![Flags::NOTE_EXTEND, Flags::NOTE_WRITE];
        let actual = flags(events);
        assert!(
            expected1.eq(&actual) || expected2.eq(&actual),
            "expected: {} or {}, actual: {}",
            flag_strings(&expected1),
            flag_strings(&expected2),
            flag_strings(&actual)
        );
        drop(file_handle);
    }

    #[test]
    fn delete_file() {
        enable_logging();

        let dir = files!({
            "file" => "content",
        });
        let file_path = dir.path().join("file");
        let t = test_kqueue();

        let _w = t.worker.add_kernel_queue_watch(&file_path).unwrap();
        delete(&file_path);

        let events = t.collect_events();
        let expected = vec![Flags::NOTE_DELETE | Flags::NOTE_LINK];
        let actual = flags(events);
        assert!(
            expected.eq(&actual),
            "expected: {}, actual: {}",
            flag_strings(&expected),
            flag_strings(&actual)
        );
    }
}
