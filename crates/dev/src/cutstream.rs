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

/// What happens once the byte budget runs out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fail loudly. The stream errors, and dropping it closes the socket, so the
    /// peer sees a clean disconnect. This is the friendly kind of failure.
    Cut,

    /// Freeze with the socket open. Reads and writes never complete again and
    /// nothing is closed.
    ///
    /// This is the unfriendly kind, and the one that matters: a phone out of
    /// range, out of battery, or force-killed leaves the peer holding a half-open
    /// connection with no notification whatsoever. Only the §8 heartbeat deadline
    /// resolves it.
    Stall,
}

pub struct CutAfter<S> {
    inner: S,
    mode: Mode,
    /// Bytes still allowed through. Once exhausted, the mode takes over.
    budget: u64,
    written: u64,
    tripped: bool,
}

impl<S> CutAfter<S> {
    pub fn with_mode(inner: S, budget: u64, mode: Mode) -> Self {
        Self {
            inner,
            mode,
            budget,
            written: 0,
            tripped: false,
        }
    }

    pub const fn was_cut(&self) -> bool {
        self.tripped
    }
}

fn aborted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "injected cut")
}

impl<S: AsyncRead + Unpin> AsyncRead for CutAfter<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.tripped {
            return match self.mode {
                Mode::Cut => Poll::Ready(Err(aborted())),
                // Never wakes. The task simply parks here forever, which is what
                // a frozen peer looks like from this side.
                Mode::Stall => Poll::Pending,
            };
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
        if self.tripped || self.written >= self.budget {
            self.tripped = true;
            return match self.mode {
                Mode::Cut => Poll::Ready(Err(aborted())),
                Mode::Stall => Poll::Pending,
            };
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
        if self.tripped {
            return match self.mode {
                Mode::Cut => Poll::Ready(Err(aborted())),
                Mode::Stall => Poll::Pending,
            };
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
