use std::sync::mpsc::Receiver;
use std::time::Duration;

pub struct TimeoutIterator<'r, T> {
    receiver: &'r Receiver<T>,
    timeout: Duration,
}

pub fn timeout_iterator<T>(receiver: &Receiver<T>, timeout: Duration) -> TimeoutIterator<'_, T> {
    TimeoutIterator { receiver, timeout }
}

impl<'r, T> Iterator for TimeoutIterator<'r, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        match self.receiver.recv_timeout(self.timeout) {
            Ok(item) => Some(item),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("recv_timeout has timed out")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }
}
