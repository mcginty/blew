pub mod types;

pub use types::{L2capCloseReason, L2capConfig, L2capEncryption, Psm};

use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

type CloseHook = Box<dyn FnOnce() + Send + 'static>;
type DynTransport = dyn L2capTransport;

/// Shared slot a backend uses to record why a channel ended, read back by
/// [`L2capChannel::close_reason`] and by `poll_read` when the stream hits EOF.
///
/// First writer wins: the initiating cause is more useful than whatever
/// teardown step happened to run last.
#[derive(Clone, Default)]
pub(crate) struct CloseReasonSlot(Arc<Mutex<Option<L2capCloseReason>>>);

impl CloseReasonSlot {
    pub(crate) fn set(&self, reason: L2capCloseReason) {
        let mut slot = self.0.lock();
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    pub(crate) fn get(&self) -> Option<L2capCloseReason> {
        self.0.lock().clone()
    }
}

trait L2capTransport: Send {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>>;

    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>>;

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    /// Why the channel ended, if the backend recorded a reason.
    fn close_reason(&self) -> Option<L2capCloseReason> {
        None
    }
}

#[cfg_attr(not(any(target_os = "linux")), allow(dead_code))]
struct StreamTransport<T> {
    inner: T,
}

#[cfg_attr(not(any(target_os = "linux")), allow(dead_code))]
impl<T> StreamTransport<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> L2capTransport for StreamTransport<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }

    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Everything a bridged backend transport needs to hand to [`L2capChannel`].
pub(crate) struct DuplexBridge {
    pub(crate) inner: DuplexStream,
    pub(crate) close_hook: Option<CloseHook>,
    pub(crate) close_reason: CloseReasonSlot,
}

struct DuplexTransport {
    inner: DuplexStream,
    close_hook: Option<CloseHook>,
    close_reason: CloseReasonSlot,
}

impl DuplexTransport {
    fn new(bridge: DuplexBridge) -> Self {
        Self {
            inner: bridge.inner,
            close_hook: bridge.close_hook,
            close_reason: bridge.close_reason,
        }
    }

    fn trigger_close(&mut self) {
        if let Some(close_hook) = self.close_hook.take() {
            close_hook();
        }
    }
}

impl Drop for DuplexTransport {
    fn drop(&mut self) {
        self.trigger_close();
    }
}

impl L2capTransport for DuplexTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        // A bridged transport signals *any* ending by dropping its half of the
        // duplex, so an abnormal end is indistinguishable from a clean one here.
        // If the backend recorded a failure, report that instead of pretending
        // the peer hung up politely.
        if matches!(result, Poll::Ready(Ok(())))
            && buf.filled().len() == before
            && let Some(err) = self.close_reason.get().and_then(|r| r.as_io_error())
        {
            return Poll::Ready(Err(err));
        }
        result
    }

    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.close_reason.set(L2capCloseReason::Closed);
        self.trigger_close();
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn close_reason(&self) -> Option<L2capCloseReason> {
        self.close_reason.get()
    }
}

/// An open L2CAP CoC channel providing a reliable ordered byte stream.
///
/// Implements [`AsyncRead`] and [`AsyncWrite`]. The backing transport is
/// backend-specific; use [`pair`](Self::pair) for testing.
pub struct L2capChannel {
    inner: Pin<Box<DynTransport>>,
}

impl std::fmt::Debug for L2capChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2capChannel").finish_non_exhaustive()
    }
}

impl L2capChannel {
    /// Create a connected in-memory pair for testing.
    #[must_use]
    pub fn pair(max_buf_size: usize) -> (Self, Self) {
        let (a, b) = tokio::io::duplex(max_buf_size);
        (Self::from_duplex(a), Self::from_duplex(b))
    }

    /// Why the channel ended, once it has.
    ///
    /// `None` while the channel is still open, or when the backend recorded no
    /// reason. Anything other than [`L2capCloseReason::Closed`] also surfaces
    /// from [`AsyncRead`] as an `io::Error`.
    #[must_use]
    pub fn close_reason(&self) -> Option<L2capCloseReason> {
        self.inner.close_reason()
    }

    /// Close the channel.
    ///
    /// Shuts down the write side and hands the channel to the backend, which
    /// keeps draining whatever is still queued until it empties or
    /// [`L2capConfig::linger_timeout`] passes. This does not block on that
    /// drain, so it does not hang on a peer that has stopped granting L2CAP
    /// credits.
    ///
    /// Dropping the channel does exactly the same thing. There is deliberately
    /// no delivery advantage to calling this — an `AsyncWrite` that only kept
    /// its data when you remembered to close it explicitly would be a trap.
    /// What `close()` gives you is the `io::Result` from shutting down the
    /// write side.
    pub async fn close(&mut self) -> std::io::Result<()> {
        // Half-close first: this is what tells the backend's outbound task that
        // the bytes it still holds are the last of them.
        poll_fn(|cx| self.inner.as_mut().poll_shutdown(cx)).await?;
        poll_fn(|cx| self.inner.as_mut().poll_close(cx)).await
    }

    #[cfg_attr(not(any(target_os = "linux")), allow(dead_code))]
    pub(crate) fn from_stream<T>(inner: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            inner: Box::pin(StreamTransport::new(inner)),
        }
    }

    pub(crate) fn from_duplex(inner: DuplexStream) -> Self {
        Self::from_bridge(DuplexBridge {
            inner,
            close_hook: None,
            close_reason: CloseReasonSlot::default(),
        })
    }

    #[cfg_attr(
        not(any(target_os = "android", target_vendor = "apple", test)),
        allow(dead_code)
    )]
    pub(crate) fn from_bridge(bridge: DuplexBridge) -> Self {
        Self {
            inner: Box::pin(DuplexTransport::new(bridge)),
        }
    }
}

impl AsyncRead for L2capChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for L2capChannel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    /// Build a bridged channel the way a backend would, with only the pieces a
    /// given test cares about.
    fn bridged(
        inner: DuplexStream,
        close_hook: Option<CloseHook>,
    ) -> (L2capChannel, CloseReasonSlot) {
        let close_reason = CloseReasonSlot::default();
        let channel = L2capChannel::from_bridge(DuplexBridge {
            inner,
            close_hook,
            close_reason: close_reason.clone(),
        });
        (channel, close_reason)
    }

    #[tokio::test]
    async fn pair_communicates() {
        let (mut a, mut b) = L2capChannel::pair(1024);
        a.write_all(b"hello").await.unwrap();
        let mut buf = [0_u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn pair_bidirectional() {
        let (mut a, mut b) = L2capChannel::pair(1024);
        a.write_all(b"ping").await.unwrap();
        let mut buf = [0_u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        b.write_all(b"pong").await.unwrap();
        a.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn explicit_close_yields_peer_eof() {
        let (mut a, mut b) = L2capChannel::pair(1024);
        a.write_all(b"hello").await.unwrap();

        let mut buf = [0_u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        a.close().await.unwrap();

        let mut eof_buf = [0_u8; 1];
        let n = timeout(Duration::from_millis(100), b.read(&mut eof_buf))
            .await
            .expect("peer should observe EOF")
            .unwrap();
        assert_eq!(n, 0);

        a.close().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_close_triggers_hook_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (inner, _peer) = tokio::io::duplex(1024);
        let hook = {
            let counter = Arc::clone(&counter);
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }) as CloseHook
        };
        let (mut channel, _) = bridged(inner, Some(hook));

        channel.close().await.unwrap();
        channel.close().await.unwrap();
        drop(channel);

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn drop_triggers_hook_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (inner, _peer) = tokio::io::duplex(1024);
        let hook = {
            let counter = Arc::clone(&counter);
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }) as CloseHook
        };
        let (channel, _) = bridged(inner, Some(hook));

        drop(channel);

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn clean_close_reads_as_eof() {
        let (inner, peer) = tokio::io::duplex(1024);
        let (mut channel, reason) = bridged(inner, None);
        reason.set(L2capCloseReason::Closed);
        drop(peer);

        let mut buf = [0_u8; 1];
        assert_eq!(channel.read(&mut buf).await.unwrap(), 0);
        assert_eq!(channel.close_reason(), Some(L2capCloseReason::Closed));
    }

    #[tokio::test]
    async fn link_loss_reads_as_error_not_eof() {
        let (inner, peer) = tokio::io::duplex(1024);
        let (mut channel, reason) = bridged(inner, None);
        reason.set(L2capCloseReason::LinkLost);
        drop(peer);

        let mut buf = [0_u8; 1];
        let err = channel
            .read(&mut buf)
            .await
            .expect_err("must not look like EOF");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
        assert_eq!(channel.close_reason(), Some(L2capCloseReason::LinkLost));
    }

    #[tokio::test]
    async fn transport_error_reads_as_error() {
        let (inner, peer) = tokio::io::duplex(1024);
        let (mut channel, reason) = bridged(inner, None);
        reason.set(L2capCloseReason::TransportError("stream died".into()));
        drop(peer);

        let mut buf = [0_u8; 1];
        let err = channel
            .read(&mut buf)
            .await
            .expect_err("must not look like EOF");
        assert!(err.to_string().contains("stream died"), "{err}");
    }

    #[tokio::test]
    async fn buffered_data_is_still_readable_after_a_failure_is_recorded() {
        // A recorded failure must not swallow bytes already in the buffer --
        // only the EOF that follows them becomes an error.
        let (inner, mut peer) = tokio::io::duplex(1024);
        let (mut channel, reason) = bridged(inner, None);
        peer.write_all(b"tail").await.unwrap();
        reason.set(L2capCloseReason::LinkLost);
        drop(peer);

        let mut buf = [0_u8; 4];
        channel.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"tail");
        assert!(channel.read(&mut [0_u8; 1]).await.is_err());
    }

    #[tokio::test]
    async fn close_shuts_down_the_write_side_before_firing_the_hook() {
        // The backend's linger depends on this ordering: half-closing is what
        // tells its outbound task that the bytes it holds are the last ones, so
        // the hook must not arrive first.
        let (inner, mut peer) = tokio::io::duplex(1024);
        let saw_eof = Arc::new(AtomicUsize::new(0));
        let marker = Arc::clone(&saw_eof);
        let hook = Box::new(move || {
            marker.store(1, Ordering::SeqCst);
        }) as CloseHook;
        let (mut channel, _) = bridged(inner, Some(hook));

        channel.close().await.unwrap();

        // Peer sees EOF, and the hook ran.
        let n = peer.read(&mut [0_u8; 1]).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(saw_eof.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn close_does_not_block_on_a_peer_that_never_drains() {
        let (inner, _peer) = tokio::io::duplex(1024);
        let (mut channel, _) = bridged(inner, None);
        timeout(Duration::from_millis(500), channel.close())
            .await
            .expect("close must not wait on the backend")
            .unwrap();
    }
}
