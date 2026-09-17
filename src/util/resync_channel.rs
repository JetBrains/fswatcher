use futures::channel::mpsc;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

pub struct Send<E> {
    buffer_size: usize,
    send: mpsc::Sender<E>,
    rollover: Arc<Rollover<E>>,
}

pub struct Receive<E> {
    current: mpsc::Receiver<E>,
    epoch: u32,
    rollover: Arc<Rollover<E>>,
}

/// On buffer overflow, it automatically clears the queue and returns BufferOverflow to the reader.
/// After that, it operates as usual until the next buffer overflow.
pub fn resync_channel<E>(buffer: usize) -> (Send<E>, Receive<E>) {
    let (send, receive) = mpsc::channel::<E>(buffer);
    let rollover = Arc::new(Rollover {
        epoch: AtomicU32::new(0),
        next: Mutex::new(None),
    });
    let event_send = Send {
        send,
        rollover: rollover.clone(),
        buffer_size: buffer,
    };
    let event_receive = Receive {
        current: receive,
        rollover,
        epoch: 0,
    };
    (event_send, event_receive)
}

struct Rollover<E> {
    epoch: AtomicU32,
    next: Mutex<Option<mpsc::Receiver<E>>>,
}

#[derive(Debug)]
pub struct Disconnected<E>(E);

impl<E> Send<E> {
    pub fn send(&mut self, event: E) -> Result<(), Disconnected<E>> {
        self.send.try_send(event).or_else(|err| {
            if err.is_full() {
                let (new_send, new_receive) = mpsc::channel::<E>(self.buffer_size);
                let mut guard = self.rollover.next.lock().expect("EventStream mutex is poisoned");
                let old = guard.replace(new_receive);
                self.rollover.epoch.fetch_add(1, Ordering::Relaxed);
                drop(old);
                self.send = new_send;
                Ok(())
            } else if err.is_disconnected() {
                Err(Disconnected(err.into_inner()))
            } else {
                panic!("Channel is neither full nor disconnected, but try_send has failed")
            }
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct BufferOverflow;

type ReceiveResult<E> = std::result::Result<E, BufferOverflow>;

impl<E> futures::Stream for Receive<E> {
    type Item = ReceiveResult<E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let epoch_before = self.rollover.epoch.load(Ordering::Relaxed);
            // fast path to avoid extra locking on each message
            if epoch_before == self.epoch {
                let r = unsafe { Pin::new_unchecked(&mut self.current) }
                    .poll_next(cx)
                    .map(|op| op.map(|i| ReceiveResult::Ok(i)));
                let epoch_after = self.rollover.epoch.load(Ordering::Relaxed);
                // The channel might have been dropped by the Sender because of an overflow, so we have to check if the result we got is still valid.
                if epoch_after == self.epoch {
                    break r;
                }
            } else {
                // None should not be possible
                let next = self.rollover.next.lock().expect("EventStream mutex is poisoned").take().unwrap();
                self.epoch = epoch_before;
                self.current = next;
                break Poll::Ready(Some(ReceiveResult::Err(BufferOverflow)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, FutureExt, Stream, StreamExt};

    fn get_one_now<S, E>(stream: &mut S) -> E
    where
        S: Stream<Item = E> + Unpin,
    {
        stream.next().now_or_never().unwrap().unwrap()
    }

    #[test]
    fn no_overflow() {
        block_on(async {
            let (mut send, mut receive) = resync_channel::<u32>(0);
            send.send(1).unwrap();
            assert_eq!(Ok(1), get_one_now(&mut receive));
            send.send(2).unwrap();
            assert_eq!(Ok(2), get_one_now(&mut receive));
        })
    }

    #[test]
    fn overflow() {
        block_on(async {
            let (mut send, mut receive) = resync_channel::<u32>(0);
            send.send(1).unwrap();
            send.send(2).unwrap();
            send.send(3).unwrap();
            assert_eq!(Err(BufferOverflow), get_one_now(&mut receive));
            assert_eq!(Ok(3), get_one_now(&mut receive));
        })
    }
}
