use async_stream::stream;
use futures::StreamExt;

use super::BoxedStream;

pub trait TokioStreamExt {
    type Item;
    fn into_stream(self) -> BoxedStream<Self::Item>;
}

impl<T> TokioStreamExt for tokio::sync::watch::Receiver<T>
where
    T: Clone + Unpin + Send + Sync + 'static,
{
    type Item = T;

    fn into_stream(self) -> BoxedStream<Self::Item> {
        watch_into_stream(self)
    }
}

fn watch_into_stream<T: Clone + Unpin + Send + Sync + 'static>(mut receiver: tokio::sync::watch::Receiver<T>) -> BoxedStream<T> {
    let stream = stream! {
        let item = { receiver.borrow().clone() };
        yield item;
        while receiver.changed().await.is_ok() {
            let item = { receiver.borrow().clone() };
            yield item;
        }
    };
    stream.boxed()
}
