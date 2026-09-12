//! A stream that dies after a byte budget.
//!
//! Success criterion 3 needs a connection that drops part-way through a large
//! asset. Waiting for a real Wi-Fi drop at the right moment is not a test, it is
//! a coincidence, so the drop is injected here instead. Everything else in the
//! path is the real engine.
//!
//! The cut is applied to **writes**, which is what a sender losing its uplink
//! looks like: some chunks were acknowledged, the next one never arrives.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct CutAfter<S> {
    inner: S,
    /// Bytes still allowed through. Once exhausted, every write fails.
    budget: u64,
    written: u64,
    cut: bool,
}

impl<S> CutAfter<S> {
    pub fn new(inner: S, budget: u64) -> Self {
        Self {
            inner,
            budget,
            written: 0,
            cut: false,
        }
    }

    pub const fn was_cut(&self) -> bool {
        self.cut
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CutAfter<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cut {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "injected cut",
            )));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CutAfter<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cut || self.written >= self.budget {
            self.cut = true;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "injected cut",
            )));
        }

        // Allow a partial write up to the budget, then cut on the next call. A
        // real uplink failure can land mid-frame, and the receiver must cope with
        // a truncated frame rather than only with a clean boundary.
        let remaining = (self.budget - self.written) as usize;
        let slice = &buf[..buf.len().min(remaining)];

        match Pin::new(&mut self.inner).poll_write(cx, slice) {
            Poll::Ready(Ok(n)) => {
                self.written += n as u64;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cut {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "injected cut",
            )));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
