use std::sync::Arc;
use crate::test_helpers::enable_logging;

use crate::{backend::fake::FakeWatcherBackend, options::WatcherOptions, Watcher};

mod session_test;
mod event_stream_test;

fn test_options(client_buffer_size: usize, body: impl FnOnce(&FakeWatcherBackend, &Watcher)) {
    enable_logging();
    let fake = Arc::new(FakeWatcherBackend::new());
    let watcher = {
        let fake = fake.clone();
        Watcher::create_by(client_buffer_size, |handler| {
            fake.set_handler(handler);
            Ok(fake)
        })
        .unwrap()
    };
    body(&fake, &watcher);
}

fn test(body: impl FnOnce(&FakeWatcherBackend, &Watcher)) {
    test_options(WatcherOptions::default().client_buffer_size, body);
}
