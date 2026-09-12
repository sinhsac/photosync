//! Reading and writing protocol frames over any async stream
//! (`app_info.md` §18).
//!
//! The read path is the only place in the product where a length supplied by a
//! peer decides how much memory to allocate, so it is the only place a hostile
//! peer can try to exhaust us. [`photosync_core::proto::parse_prefix`] bounds
//! both lengths *before* any buffer is sized, and this module never allocates
//! from a raw prefix.

use photosync_core::error::Error as CoreError;
use photosync_core::proto::{self, Frame, Message, FRAME_PREFIX_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Writes one frame and flushes it.
///
/// Flushing per frame is deliberate: the control stream is request/response, and
/// a buffered `Authenticate` or `ChunkAck` sitting in a socket buffer looks
/// exactly like a hung peer.
pub async fn write_frame<W>(w: &mut W, frame: &Frame) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    let bytes = proto::encode(frame)?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Convenience for messages with no payload.
pub async fn write_message<W>(w: &mut W, message: Message) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    write_frame(w, &Frame::new(message)).await
}

/// Reads one frame.
///
/// Returns [`Error::Closed`] when the peer closed cleanly at a frame boundary,
/// which is how a heartbeat timeout and an orderly shutdown are told apart from
/// a truncated frame.
pub async fn read_frame<R>(r: &mut R) -> Result<Frame, Error>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; FRAME_PREFIX_LEN];

    // Distinguish "nothing more is coming" from "half a frame arrived".
    match r.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
        Err(e) => return Err(Error::Io(e)),
    }

    // Bounds are enforced here, before the allocation below.
    let lengths = proto::parse_prefix(&prefix)?;

    let mut body = vec![0u8; lengths.total()];
    r.read_exact(&mut body).await?;

    Ok(proto::decode_body(lengths, &body)?)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("peer closed the connection")]
    Closed,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Core(#[from] CoreError),
}
