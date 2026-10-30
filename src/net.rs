//! Small network helpers.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

/// Wraps a connection so a write that makes no progress for `timeout` fails. hyper has no
/// write timeout of its own, so a client that asks for a big file and then stops reading
/// would otherwise hold its connection (and its slot) forever. Zero disables the timeout.
pub struct WriteTimeout<T> {
    inner: T,
    timeout: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> WriteTimeout<T> {
    pub fn new(inner: T, timeout: Duration) -> Self {
        WriteTimeout {
            inner,
            timeout,
            stalled: None,
        }
    }

    /// A write returned `Pending`: start the clock if it is not running, and fail once it
    /// has run out. The clock is polled so that it wakes this task when it fires.
    fn stalled<R>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<R>> {
        if self.timeout.is_zero() {
            return Poll::Pending;
        }
        let timeout = self.timeout;
        let clock = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        if clock.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client stopped reading the response",
            )))
        } else {
            Poll::Pending
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteTimeout<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => self.stalled(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Pending => self.stalled(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending => self.stalled(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
