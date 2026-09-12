//! The control link: framed I/O with liveness (`app_info.md` §8).
//!
//! Exists because a socket alone cannot tell "the peer is thinking" from "the
//! peer is gone". A peer that stops sending without closing leaves a half-open
//! connection, and a plain read on it blocks forever. That is not a corner case:
//! a phone going out of range, running out of battery, or being force-killed
//! produces exactly that shape, and it was reproduced accidentally while
//! building the resume harness.
//!
//! Two mechanisms, and they only work together:
//!
//! * **A read deadline.** No read waits longer than [`Timeouts::read`].
//! * **Heartbeats.** Both sides emit one when they have been quiet, so a peer
//!   that is legitimately busy is not mistaken for a dead one.
//!
//! Heartbeats are consumed here and never surface to the protocol layer, so no
//! state machine has to carry an arm for them.

use crate::frame::{read_frame, write_frame, Error as FrameError};
use photosync_core::proto::{Frame, Message};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};

/// Liveness settings (§8).
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Emit a heartbeat once the link has been quiet this long.
    pub heartbeat: Duration,
    /// Give up on a read after this long with nothing at all arriving.
    ///
    /// Must be comfortably larger than `heartbeat`, or a peer that is merely
    /// quiet gets dropped between its own keepalives.
    pub read: Duration,

    /// Give up on a write that cannot complete.
    ///
    /// A read deadline alone is not enough. If the peer freezes while its receive
    /// window is full, our writes block instead of failing, and no amount of
    /// waiting to *read* helps because we never get far enough to read. Found by
    /// injecting a frozen peer in the harness, where the sender parked forever on
    /// a write while the receiver correctly timed out.
    pub write: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(10),
            read: Duration::from_secs(30),
            write: Duration::from_secs(30),
        }
    }
}

impl Timeouts {
    /// Short values for the harness, so the behaviour can be observed without
    /// waiting half a minute.
    pub fn fast() -> Self {
        Self {
            heartbeat: Duration::from_millis(200),
            read: Duration::from_millis(1500),
            write: Duration::from_millis(1500),
        }
    }
}

/// A framed connection that knows whether the peer is still there.
pub struct Link<S> {
    stream: S,
    timeouts: Timeouts,
    last_write: Instant,
}

impl<S> Link<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S, timeouts: Timeouts) -> Self {
        Self {
            stream,
            timeouts,
            last_write: Instant::now(),
        }
    }

    pub const fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Reads the next protocol frame, transparently absorbing heartbeats.
    ///
    /// The deadline applies to each individual read, so a peer that keeps
    /// heartbeating stays connected indefinitely while it works. That is
    /// deliberate: the deadline is there to detect silence, not slowness.
    pub async fn read(&mut self) -> Result<Frame, Error> {
        loop {
            let frame = tokio::time::timeout(self.timeouts.read, read_frame(&mut self.stream))
                .await
                .map_err(|_| Error::Timeout)??;

            if matches!(frame.message, Message::Heartbeat) {
                continue;
            }
            return Ok(frame);
        }
    }

    /// Writes a frame, bounded by [`Timeouts::write`].
    pub async fn write(&mut self, frame: &Frame) -> Result<(), Error> {
        tokio::time::timeout(self.timeouts.write, write_frame(&mut self.stream, frame))
            .await
            .map_err(|_| Error::Timeout)??;
        self.last_write = Instant::now();
        Ok(())
    }

    /// Convenience for payload-free messages.
    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        self.write(&Frame::new(message)).await
    }

    /// Sends a heartbeat if the link has been quiet, otherwise does nothing.
    ///
    /// Cheap enough to call in a loop. Put it wherever this side is about to be
    /// busy without writing — between chunks, and around anything that touches
    /// the photo library or rebuilds a digest.
    pub async fn keepalive(&mut self) -> Result<(), Error> {
        if self.last_write.elapsed() >= self.timeouts.heartbeat {
            self.send(Message::Heartbeat).await?;
        }
        Ok(())
    }

    /// Unwraps the stream, for a caller that needs to close it explicitly.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing arrived within the read deadline. The peer is unreachable or
    /// stuck; either way this session is interrupted, not failed (§20).
    #[error("peer went silent")]
    Timeout,

    #[error(transparent)]
    Frame(#[from] FrameError),
}

impl Error {
    /// Whether this means "the connection is gone" rather than "the peer
    /// misbehaved". Callers use it to decide between a resumable interruption and
    /// a hard error.
    pub fn is_disconnect(&self) -> bool {
        match self {
            Error::Timeout => true,
            Error::Frame(FrameError::Closed) => true,
            Error::Frame(FrameError::Io(_)) => true,
            Error::Frame(FrameError::Core(_)) => false,
        }
    }
}
