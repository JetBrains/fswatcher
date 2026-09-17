use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

static LIFETIME_ID: AtomicU32 = AtomicU32::new(1);

struct Inner {
    debug_id: u32,
    termination: Termination,
}

pub struct LifetimeDefinition {
    inner: Arc<Inner>,
}

impl LifetimeDefinition {
    pub fn new() -> LifetimeDefinition {
        LifetimeDefinition {
            inner: Arc::new(Inner {
                debug_id: LIFETIME_ID.fetch_add(1, Ordering::Relaxed),
                termination: Termination::new(),
            }),
        }
    }

    pub fn lifetime(&self) -> Lifetime {
        Lifetime { inner: self.inner.clone() }
    }

    pub fn terminate(self) {
        self.inner.termination.signal()
    }
}

impl Default for LifetimeDefinition {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for LifetimeDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("LifetimeDefinition#")?;
        self.inner.debug_id.fmt(f)
    }
}

impl Drop for LifetimeDefinition {
    fn drop(&mut self) {
        self.inner.termination.signal()
    }
}

#[derive(Clone)]
pub struct Lifetime {
    inner: Arc<Inner>,
}

impl Debug for Lifetime {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lifetime#")?;
        self.inner.debug_id.fmt(f)
    }
}

impl Lifetime {
    #[cfg(target_os = "linux")]
    pub fn is_terminated(&self) -> bool {
        self.inner.termination.is_terminated()
    }

    pub async fn terminated(&self) {
        self.inner.termination.terminated().await
    }
}

struct Termination {
    terminated: AtomicBool,
    notify: Notify,
}

impl Termination {
    pub fn new() -> Termination {
        Termination {
            terminated: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.terminated.load(Ordering::Relaxed) {
                break;
            }
            notified.await;
        }
    }

    fn signal(&self) {
        self.terminated.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    #[cfg(target_os = "linux")]
    fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Relaxed)
    }

    async fn terminated(&self) {
        self.wait().await;
    }
}

#[cfg(test)]
mod lifetime_test {
    #![allow(clippy::bool_assert_comparison)]

    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn await_lifetime_termination() {
        test_with_timeout(|rt| async move {
            let lifetime_source = LifetimeDefinition::new();
            let lifetime1 = lifetime_source.lifetime();
            let lifetime2 = lifetime_source.lifetime();
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);

            rt.spawn(async move {
                tx.send(lifetime1.inner.termination.terminated.load(Ordering::Relaxed))
                    .await
                    .unwrap();
                lifetime1.terminated().await;
                tx.send(lifetime1.inner.termination.terminated.load(Ordering::Relaxed))
                    .await
                    .unwrap();
            });
            assert_eq!(false, rx.recv().await.unwrap());
            lifetime_source.terminate();
            assert_eq!(true, rx.recv().await.unwrap());

            //block on already terminated lifetime
            lifetime2.terminated().await;
        })
    }
}
