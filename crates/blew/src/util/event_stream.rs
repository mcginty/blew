use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Opaque event stream returned by [`Central::events`](crate::central::Central::events)
/// and [`Peripheral::events`](crate::peripheral::Peripheral::events).
pub struct EventStream<T, S: Stream<Item = T> + Unpin>(S, std::marker::PhantomData<T>);

impl<T, S: Stream<Item = T> + Unpin> EventStream<T, S> {
    pub(crate) fn new(stream: S) -> Self {
        Self(stream, std::marker::PhantomData)
    }
}

impl<T, S: Stream<Item = T> + Unpin> Stream for EventStream<T, S> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

impl<T, S: Stream<Item = T> + Unpin> Unpin for EventStream<T, S> {}
