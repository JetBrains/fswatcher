use crate::{backend::{immediate::Changed, Scope}, event::Event, session::{session, WatchSession}, ClientOverflow, Watcher};
use crate::util::path_util::PathExt;
use futures::{stream::BoxStream, StreamExt};
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

pub struct EventStream<'a> {
    stream: BoxStream<'a, Event>,
}

impl<'a> futures::Stream for EventStream<'a> {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.stream) }.poll_next(cx)
    }
}

pub(crate) fn event_stream<'a>(watcher: impl Deref<Target = Watcher> + 'a + Send + Sync, path: PathBuf, scope: Scope) -> EventStream<'a> {
    assert_valid_path(&path);

    let mut session = session(watcher);
    session.watch_root(&path, scope);
    let stream = async_stream::stream! {
        loop {
            let event = session.next_event().await.unwrap_or_else(|_overflow| Event::Rescan { path: path.to_path_buf() });
            yield event;
        }
    };
    EventStream { stream: stream.boxed() }
}

pub(crate) fn immediate_stream<'a>(watcher: impl Deref<Target = Watcher> + 'a + Send + Sync, path: PathBuf) -> EventStream<'a> {
    if watcher.immediate.is_some() {
        assert_valid_path(&path);

        fn im_subscribe(session: &WatchSession, symbolic_path: &Path) -> Option<BoxStream<'static, Changed>> {
            let im = session
                .tx
                .inner
                .watcher
                .immediate
                .as_ref()
                .expect("it is not going anywhere")
                .as_ref();
            session
                .canonicalization(symbolic_path)
                .ok()
                .and_then(move |canonical_path| im.watch_immediate_changes(canonical_path).ok())
        }

        let mut session = session(watcher);
        session.watch_root(&path, Scope::DirectChildren);
        let initial = im_subscribe(&session, &path);

        let stream = async_stream::stream! {
            let mut immediate_stream = initial;
            loop {
                if let Some(is) = immediate_stream.as_mut() {
                    tokio::select! {
                        changed = is.next() => {
                            if changed.is_some() {
                                yield Event::Dirty {
                                    path: path.clone(),
                                    file_type: crate::FileType::Regular,
                                    // key
                                }
                            } else {
                                // The stream is closed because the file is deleted or moved.
                                // We ignore that, because the root watch handles removals and rescans.
                                // Otherwise, the event is duplicated.
                                immediate_stream = None;
                            }
                        }
                        root_event = session.next_event() => {
                            if let Ok(Event::Rescan { .. }) | Ok(Event::Removed { .. })  | Err(ClientOverflow) = root_event {
                                immediate_stream = im_subscribe(&session, &path);
                                yield root_event.unwrap_or_else(|_overflow| Event::Rescan { path: path.to_path_buf() });
                            }
                        }
                    }
                } else {
                    let root_event = session.next_event().await;
                    immediate_stream = im_subscribe(&session, &path);
                    yield root_event.unwrap_or_else(|_overflow| Event::Rescan { path: path.to_path_buf() });
                }
            }
        };
        EventStream { stream: stream.boxed() }
    } else {
        event_stream(watcher, path, Scope::DirectChildren)
    }
}

fn assert_valid_path(path: &Path) {
    if path.is_empty_path() {
        panic!("path is empty");
    }
    if path.is_relative() {
        panic!("only absolute paths are allowed: {:?}", path);
    }
}
