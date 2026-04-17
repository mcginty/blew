pub mod types;

pub use types::Psm;

use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

type CloseHook = Box<dyn FnOnce() + Send + 'static>;

/// An open L2CAP CoC channel providing a reliable ordered byte stream.
///
/// Implements [`AsyncRead`] and [`AsyncWrite`]. The backing stream is platform-specific;
/// use [`pair`](Self::pair) for testing.
pub struct L2capChannel {
    inner: DuplexStream,
    // Platform backends can install a shutdown hook to tear down transport
    // resources that live outside the duplex stream.
    close_hook: Option<CloseHook>,
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
        (
            Self {
                inner: a,
                close_hook: None,
            },
            Self {
                inner: b,
                close_hook: None,
            },
        )
    }

    pub(crate) fn from_duplex(inner: DuplexStream) -> Self {
        Self {
            inner,
            close_hook: None,
        }
    }

    pub(crate) fn from_duplex_with_close_hook(
        inner: DuplexStream,
        close_hook: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            inner,
            close_hook: Some(Box::new(close_hook)),
        }
    }
}

impl Drop for L2capChannel {
    fn drop(&mut self) {
        if let Some(close_hook) = self.close_hook.take() {
            close_hook();
        }
    }
}

impl AsyncRead for L2capChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for L2capChannel {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
}
