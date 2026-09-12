//! Session orchestration (`app_info.md` §17): the layer that turns the verified
//! pieces into a sync.
//!
//! It owns the order of operations and nothing else. Identity decisions live in
//! `core::inbound`, chunk mechanics in `core::chunk`, durability rules in
//! `core::session`. What is genuinely *here* is the sequencing, and two orderings
//! in particular that cannot be got wrong:
//!
//! * A chunk is acknowledged only after it is durable, and the offset is
//!   persisted only after the acknowledgement (§13.4).
//! * On the sender, recording delivery is one transaction covering both
//!   `sent_log` and `local_asset.full_hash`. A crash between those two would lose
//!   the fact that an asset arrived and send it again (§12.1).

use crate::link::{Error as LinkError, Link};
use photosync_core::chunk::{Chunk, ChunkVerdict, FinishVerdict, InboundAsset, OutboundAsset};
use photosync_core::model::{
    AssetDescriptor, Hash32, HaveVerdict, SessionRole, SessionState, TransferState,
};
use photosync_core::proto::{
    AbortReason, AssetOutcome, ChunkOutcome, Frame, HaveQueryEntry, HaveResponseEntry, Message,
    ResumePoint, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use photosync_core::provider::{LibrarySink, LibrarySource};
use photosync_core::{catalog, inbound, pairing, session};
use rusqlite::Connection;
use tokio::io::{AsyncRead, AsyncWrite};

/// Assets per manifest batch (§12.2).
pub const HAVE_BATCH: u32 = 500;

/// What a completed session reports to the UI (§19.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub items_done: u64,
    pub items_skipped: u64,
    pub items_failed: u64,
    pub bytes_done: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error(transparent)]
    Core(#[from] photosync_core::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("peer aborted: {reason:?} ({detail})")]
    PeerAborted { reason: AbortReason, detail: String },
    #[error("unexpected message: {0}")]
    Unexpected(String),
    #[error("authentication failed")]
    AuthFailed,
    #[error("protocol version {peer} is incompatible with {ours}")]
    VersionMismatch { peer: u16, ours: u16 },
}

type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Shared handshake
// ---------------------------------------------------------------------------

/// Outcome of a successful handshake.
pub struct Handshaken {
    pub peer_id: i64,
    pub peer_fingerprint: Hash32,
    pub peer_name: String,
}

/// How the receiver authenticates the peer.
pub enum ReceiverAuth<'a> {
    /// First contact: the peer must prove knowledge of the displayed code
    /// (§9.3). The only moment `Pinning::FirstContact` is acceptable.
    Code(&'a mut pairing::CodeSession),

    /// Already paired. Mutual TLS with the peer's fingerprint pinned **is** the
    /// credential, so no code is exchanged (§9.5). This is what makes resume and
    /// repeat syncs work without asking the user to pair again.
    ///
    /// The caller must have built the TLS config with
    /// `Pinning::Expect(expected)`. Passing this variant with a
    /// `FirstContact` config would leave the connection unauthenticated
    /// altogether, so the fingerprint is re-checked here as well.
    Paired { expected: Hash32 },
}

/// How the sender authenticates.
pub enum SenderAuth<'a> {
    Code(&'a pairing::PairingCode),
    /// Symmetric to [`ReceiverAuth::Paired`]. The pinned fingerprint pair is the
    /// credential.
    Paired,
}

/// Receiver half of the handshake. Verifies the sender's proof before answering
/// (see `proto::AuthOrder`).
async fn receiver_handshake<S>(
    link: &mut Link<S>,
    conn: &Connection,
    own_fingerprint: Hash32,
    peer_fingerprint: Hash32,
    auth: ReceiverAuth<'_>,
    device_name: &str,
    now_ms: i64,
) -> Result<Handshaken>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let peer_name = match link.read().await?.message {
        Message::Hello {
            major,
            device_name,
            cert_fingerprint,
            ..
        } => {
            if major != PROTOCOL_MAJOR {
                let _ = abort(link, AbortReason::VersionMismatch, "major mismatch").await;
                return Err(Error::VersionMismatch {
                    peer: major,
                    ours: PROTOCOL_MAJOR,
                });
            }
            // The claimed fingerprint is only a convenience. If it disagrees with
            // the handshake, the peer is confused or lying, and either way the
            // handshake wins (§9.1).
            if cert_fingerprint != peer_fingerprint {
                tracing::warn!(
                    claimed = %cert_fingerprint.short(),
                    actual = %peer_fingerprint.short(),
                    "peer claimed a fingerprint that is not its certificate"
                );
            }
            device_name
        }
        other => return Err(Error::Unexpected(format!("{other:?}"))),
    };

    link.send(Message::Hello {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
        device_name: device_name.to_string(),
        platform: std::env::consts::OS.to_string(),
        cert_fingerprint: own_fingerprint,
    })
    .await?;

    match auth {
        ReceiverAuth::Paired { expected } => {
            // Defence in depth. If the caller built the TLS config with
            // `FirstContact` but asked for `Paired`, nothing would have
            // authenticated this connection at all. Checking here costs one
            // comparison and removes a way to misuse the API catastrophically.
            if peer_fingerprint != expected {
                let _ = abort(link, AbortReason::AuthFailed, "unknown peer").await;
                return Err(Error::AuthFailed);
            }
            tracing::debug!(
                peer = %expected.short(),
                "paired reconnect, pinned fingerprint is the credential"
            );
        }

        ReceiverAuth::Code(code_session) => {
            let challenge = pairing::AuthChallenge::new(peer_fingerprint, own_fingerprint);
            link.send(Message::AuthChallenge {
                nonce: challenge.nonce,
            })
            .await?;

            let presented = match link.read().await?.message {
                Message::Authenticate { proof } => proof,
                other => return Err(Error::Unexpected(format!("{other:?}"))),
            };

            if let Err(rejection) =
                code_session.verify(&challenge, pairing::ProofRole::Sender, &presented, now_ms)
            {
                let reason = match rejection {
                    pairing::CodeRejection::Expired => AbortReason::CodeExpired,
                    pairing::CodeRejection::TooManyAttempts => AbortReason::TooManyAttempts,
                    pairing::CodeRejection::WrongProof { .. } => AbortReason::AuthFailed,
                };
                // No detail about *why*: that would leak information about the
                // code.
                let _ = abort(link, reason, "authentication failed").await;
                return Err(Error::AuthFailed);
            }

            link.send(Message::Authenticate {
                proof: challenge.proof(code_session.code(), pairing::ProofRole::Receiver),
            })
            .await?;
        }
    }

    let peer_id = catalog::upsert_peer(conn, &peer_fingerprint, Some(&peer_name), None)?;
    Ok(Handshaken {
        peer_id,
        peer_fingerprint,
        peer_name,
    })
}

/// Sender half of the handshake. Proves first, then checks the answer.
async fn sender_handshake<S>(
    link: &mut Link<S>,
    conn: &Connection,
    own_fingerprint: Hash32,
    peer_fingerprint: Hash32,
    auth: SenderAuth<'_>,
    device_name: &str,
) -> Result<Handshaken>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    link.send(Message::Hello {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
        device_name: device_name.to_string(),
        platform: std::env::consts::OS.to_string(),
        cert_fingerprint: own_fingerprint,
    })
    .await?;

    let peer_name = match link.read().await?.message {
        Message::Hello {
            major, device_name, ..
        } => {
            if major != PROTOCOL_MAJOR {
                return Err(Error::VersionMismatch {
                    peer: major,
                    ours: PROTOCOL_MAJOR,
                });
            }
            device_name
        }
        Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
        other => return Err(Error::Unexpected(format!("{other:?}"))),
    };

    if let SenderAuth::Code(code) = auth {
        let nonce = match link.read().await?.message {
            Message::AuthChallenge { nonce } => nonce,
            Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
            other => return Err(Error::Unexpected(format!("{other:?}"))),
        };

        // Both fingerprints come from the TLS handshake, never from Hello (§9.3).
        let challenge =
            pairing::AuthChallenge::from_nonce(nonce, own_fingerprint, peer_fingerprint);
        link.send(Message::Authenticate {
            proof: challenge.proof(code, pairing::ProofRole::Sender),
        })
        .await?;

        match link.read().await?.message {
            Message::Authenticate { proof } => {
                challenge
                    .verify(code, pairing::ProofRole::Receiver, &proof)
                    .map_err(|_| Error::AuthFailed)?;
            }
            Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
            other => return Err(Error::Unexpected(format!("{other:?}"))),
        }
    }

    let peer_id = catalog::upsert_peer(conn, &peer_fingerprint, Some(&peer_name), None)?;
    Ok(Handshaken {
        peer_id,
        peer_fingerprint,
        peer_name,
    })
}

async fn abort<S>(link: &mut Link<S>, reason: AbortReason, detail: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    link.send(Message::Abort {
        reason,
        detail: detail.to_string(),
    })
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Runs a receiving session to completion.
#[allow(clippy::too_many_arguments)]
pub async fn run_receiver<S, K>(
    link: &mut Link<S>,
    conn: &Connection,
    sink: &K,
    own_fingerprint: Hash32,
    peer_fingerprint: Hash32,
    auth: ReceiverAuth<'_>,
    device_name: &str,
    now_ms: i64,
    resume_session_id: Option<String>,
) -> Result<Totals>
where
    S: AsyncRead + AsyncWrite + Unpin,
    K: LibrarySink,
{
    let peer = receiver_handshake(
        link,
        conn,
        own_fingerprint,
        peer_fingerprint,
        auth,
        device_name,
        now_ms,
    )
    .await?;

    // Reuse an interrupted session so its in-flight offsets survive (§13.4).
    let sess = match resume_session_id.and_then(|id| session::get(conn, &id).ok().flatten()) {
        Some(existing) => {
            session::set_state(conn, &existing.id, SessionState::Active)?;
            existing
        }
        None => session::begin(conn, SessionRole::Receiver, Some(peer.peer_id), 0, 0)?,
    };

    let in_flight: Vec<ResumePoint> = inbound::in_flight(conn, &sess.id)?
        .into_iter()
        .map(|s| ResumePoint {
            quick_hash: s.quick_hash,
            bytes_received: s.bytes_received,
        })
        .collect();
    link.send(Message::SessionResume { in_flight }).await?;

    let mut totals = Totals::default();
    let mut current: Option<Active> = None;

    loop {
        let frame = match link.read().await {
            Ok(f) => f,

            // A dropped connection is a resumable condition, not a failure
            // (§20). Leave the in-flight row and its offset exactly as they are —
            // that is precisely what resume reads — mark the session
            // interrupted, and report what got through.
            //
            // An orderly EOF, a reset, and a heartbeat timeout all mean the same
            // thing here: the peer is gone. Only a protocol violation is a real
            // error, because that indicates a peer we cannot safely continue
            // with.
            Err(e) if e.is_disconnect() => {
                session::set_state(conn, &sess.id, SessionState::Interrupted)?;
                tracing::info!(
                    session = %sess.id,
                    done = totals.items_done,
                    "connection dropped, session left resumable"
                );
                return Ok(totals);
            }
            Err(e) => return Err(e.into()),
        };

        match frame.message {
            Message::Heartbeat => {}

            // The sender's estimate, used for the "of 12,431" in §19.3. Recorded
            // rather than ignored so the receiver's own progress screen has
            // something to show.
            Message::SessionBegin {
                est_items,
                est_bytes,
                ..
            } => {
                conn.execute(
                    "UPDATE session SET items_total = ?2, bytes_total = ?3 WHERE id = ?1",
                    rusqlite::params![&sess.id, est_items as i64, est_bytes as i64],
                )?;
            }

            Message::HaveQuery { items } => {
                let batch: Vec<inbound::HaveQueryItem> = items
                    .iter()
                    .map(|e| inbound::HaveQueryItem {
                        id: e.id,
                        quick_hash: e.quick_hash,
                        full_hash: e.full_hash,
                    })
                    .collect();
                let answers = inbound::answer_have_query(conn, &batch)?;
                link.send(Message::HaveResponse {
                    items: answers
                        .into_iter()
                        .map(|a| HaveResponseEntry {
                            id: a.id,
                            verdict: a.verdict,
                        })
                        .collect(),
                })
                .await?;
            }

            Message::AssetBegin { descriptor, .. } => {
                let quick = descriptor.quick_hash;
                let (staging_ref, staging) = sink.staging(&quick)?;
                let state = inbound::open(conn, &sess.id, &quick, &staging_ref, descriptor.size)?;

                // Rebuilding the digest re-reads the whole partial from disk and
                // can take a minute on a large one (§13.4). Tell the sender we are
                // alive before disappearing into it.
                //
                // Not sufficient on its own: a rehash longer than the read
                // deadline will still trip the sender's timeout, because this is a
                // single blocking call with no chance to heartbeat from inside.
                // The fix is to move it off the I/O path and keep the link alive
                // while it runs; until then the deadline bounds how large a
                // partial can be resumed.
                if state.bytes_received > 0 {
                    link.keepalive().await?;
                }

                let asset = if state.bytes_received > 0 {
                    InboundAsset::resume(staging, descriptor.size, state.bytes_received)?
                } else {
                    InboundAsset::begin(staging, descriptor.size)?
                };
                current = Some(Active {
                    descriptor,
                    staging_ref,
                    asset,
                });
            }

            Message::Chunk { offset, hash } => {
                let Some(active) = current.as_mut() else {
                    return Err(Error::Unexpected("chunk outside an asset".into()));
                };
                let chunk = Chunk {
                    offset,
                    payload: frame.payload,
                    hash,
                };
                let verdict = active.asset.accept_chunk(&chunk)?;
                let outcome = match verdict {
                    ChunkVerdict::Accepted { bytes_received } => {
                        // Durable first, then persisted, then acknowledged.
                        inbound::advance(
                            conn,
                            &sess.id,
                            &active.descriptor.quick_hash,
                            bytes_received,
                        )?;
                        ChunkOutcome::Accepted { bytes_received }
                    }
                    ChunkVerdict::Corrupt => ChunkOutcome::Corrupt,
                    ChunkVerdict::Misaligned { expected_offset } => {
                        ChunkOutcome::Misaligned { expected_offset }
                    }
                };
                link.send(Message::ChunkAck { offset, outcome }).await?;
            }

            Message::AssetEnd { full_hash } => {
                let Some(active) = current.take() else {
                    return Err(Error::Unexpected("asset end outside an asset".into()));
                };
                let Active {
                    descriptor,
                    staging_ref,
                    asset,
                } = active;
                let (verdict, _staging) = asset.finish(full_hash)?;

                let outcome = match verdict {
                    FinishVerdict::Verified { full_hash } => {
                        if inbound::holds_full_hash(conn, &full_hash)? {
                            // Streamed on a Probable verdict and turned out to be
                            // a duplicate. A success, not an error (§18).
                            sink.abandon(&staging_ref)?;
                            inbound::close(conn, &sess.id, &descriptor.quick_hash)?;
                            totals.items_skipped += 1;
                            AssetOutcome::AlreadyPresent
                        } else {
                            // Committing goes through the platform photo library,
                            // which can take seconds per asset (§10.2).
                            link.keepalive().await?;
                            let platform_id = sink.commit(&staging_ref, &descriptor)?;
                            inbound::record_received(
                                conn,
                                &full_hash,
                                &descriptor.quick_hash,
                                descriptor.size,
                                Some(&platform_id),
                                Some(peer.peer_id),
                            )?;
                            inbound::close(conn, &sess.id, &descriptor.quick_hash)?;
                            totals.items_done += 1;
                            totals.bytes_done += descriptor.size;
                            AssetOutcome::Committed
                        }
                    }
                    FinishVerdict::Mismatch { .. } | FinishVerdict::Short { .. } => {
                        // Discard everything: a chunk-level retry cannot help
                        // when every chunk already passed its own hash (§13.5).
                        sink.abandon(&staging_ref)?;
                        inbound::reset(conn, &sess.id, &descriptor.quick_hash)?;
                        tracing::warn!(?verdict, "asset failed verification");
                        AssetOutcome::HashMismatch
                    }
                };
                link.send(Message::AssetAck { outcome }).await?;
            }

            Message::SessionEnd { .. } => {
                session::set_state(conn, &sess.id, SessionState::Done)?;
                return Ok(totals);
            }

            Message::Abort { reason, detail } => {
                session::set_state(conn, &sess.id, SessionState::Interrupted)?;
                return Err(Error::PeerAborted { reason, detail });
            }

            other => return Err(Error::Unexpected(format!("{other:?}"))),
        }
    }
}

/// The asset currently being received.
struct Active {
    descriptor: AssetDescriptor,
    staging_ref: String,
    asset: InboundAsset<Box<dyn photosync_core::provider::StagingFile + Send>>,
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// Runs a sending session to completion.
#[allow(clippy::too_many_arguments)]
pub async fn run_sender<S, L>(
    link: &mut Link<S>,
    conn: &Connection,
    source: &L,
    own_fingerprint: Hash32,
    peer_fingerprint: Hash32,
    auth: SenderAuth<'_>,
    device_name: &str,
    include_videos: bool,
) -> Result<Totals>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: LibrarySource,
{
    let peer = sender_handshake(
        link,
        conn,
        own_fingerprint,
        peer_fingerprint,
        auth,
        device_name,
    )
    .await?;

    let filter = catalog::FilterArgs {
        peer_id: peer.peer_id,
        include_videos,
    };
    let counters = catalog::counters(conn, filter)?;
    let sess = session::begin(
        conn,
        SessionRole::Sender,
        Some(peer.peer_id),
        counters.remaining,
        0,
    )?;

    // The receiver's offsets, one message for the whole session (§18).
    let resume_points = match link.read().await?.message {
        Message::SessionResume { in_flight } => in_flight,
        Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
        other => return Err(Error::Unexpected(format!("{other:?}"))),
    };
    let resume_of = |quick: &Hash32| -> u64 {
        resume_points
            .iter()
            .find(|p| &p.quick_hash == quick)
            .map(|p| p.bytes_received)
            .unwrap_or(0)
    };

    link.send(Message::SessionBegin {
        role: SessionRole::Sender,
        session_id: sess.id.clone(),
        est_items: counters.remaining,
        est_bytes: 0,
    })
    .await?;

    let mut totals = Totals::default();

    loop {
        // The backlog is re-derived every round. Confirming an asset removes it,
        // so there is no cursor to keep and nothing to reconcile (§12.1).
        let batch = catalog::candidates(conn, filter, HAVE_BATCH)?;
        if batch.is_empty() {
            break;
        }

        let query: Vec<HaveQueryEntry> = batch
            .iter()
            .map(|c| HaveQueryEntry {
                id: c.id as u64,
                quick_hash: c.descriptor.quick_hash,
                full_hash: c.descriptor.full_hash,
            })
            .collect();
        link.send(Message::HaveQuery { items: query }).await?;

        let answers = match link.read().await?.message {
            Message::HaveResponse { items } => items,
            Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
            other => return Err(Error::Unexpected(format!("{other:?}"))),
        };

        let mut progressed = false;
        for answer in answers {
            let Some(candidate) = batch.iter().find(|c| c.id as u64 == answer.id) else {
                continue;
            };

            match answer.verdict {
                HaveVerdict::Skip => {
                    // Exact full-hash match. No bytes, and it is a success (§18).
                    let full = candidate
                        .descriptor
                        .full_hash
                        .expect("Skip is only possible when a full hash was supplied");
                    record_delivered(
                        conn,
                        peer.peer_id,
                        candidate.id,
                        &full,
                        &candidate.descriptor.quick_hash,
                    )?;
                    totals.items_skipped += 1;
                    progressed = true;
                }
                HaveVerdict::Send | HaveVerdict::Probable => {
                    let offset = resume_of(&candidate.descriptor.quick_hash);
                    match send_asset(link, conn, source, &sess.id, candidate, offset).await? {
                        Delivered::Committed { full_hash, bytes } => {
                            record_delivered(
                                conn,
                                peer.peer_id,
                                candidate.id,
                                &full_hash,
                                &candidate.descriptor.quick_hash,
                            )?;
                            session::settle(
                                conn,
                                &sess.id,
                                candidate.id,
                                TransferState::Committed,
                                bytes,
                            )?;
                            totals.items_done += 1;
                            totals.bytes_done += bytes;
                            progressed = true;
                        }
                        Delivered::AlreadyPresent { full_hash } => {
                            record_delivered(
                                conn,
                                peer.peer_id,
                                candidate.id,
                                &full_hash,
                                &candidate.descriptor.quick_hash,
                            )?;
                            session::settle(
                                conn,
                                &sess.id,
                                candidate.id,
                                TransferState::SkippedAlreadyPresent,
                                0,
                            )?;
                            totals.items_skipped += 1;
                            progressed = true;
                        }
                        Delivered::Failed => {
                            session::settle(
                                conn,
                                &sess.id,
                                candidate.id,
                                TransferState::Failed,
                                0,
                            )?;
                            totals.items_failed += 1;
                            progressed = true;
                        }
                    }
                }
            }
        }

        // Guard against a peer that answers without ever letting anything settle:
        // without this the outer loop would spin on the same batch forever.
        if !progressed {
            return Err(Error::Unexpected(
                "a full batch produced no progress".into(),
            ));
        }
    }

    link.send(Message::SessionEnd {
        items_done: totals.items_done,
        items_skipped: totals.items_skipped,
        items_failed: totals.items_failed,
        bytes_done: totals.bytes_done,
    })
    .await?;
    session::set_state(conn, &sess.id, SessionState::Done)?;
    Ok(totals)
}

enum Delivered {
    Committed { full_hash: Hash32, bytes: u64 },
    AlreadyPresent { full_hash: Hash32 },
    Failed,
}

/// Streams one asset, honouring the receiver's corrections.
async fn send_asset<S, L>(
    link: &mut Link<S>,
    conn: &Connection,
    source: &L,
    session_id: &str,
    candidate: &catalog::Candidate,
    proposed_offset: u64,
) -> Result<Delivered>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: LibrarySource,
{
    let size = candidate.descriptor.size;
    session::enqueue(conn, session_id, candidate.id, size)?;

    for attempt in 1..=session::MAX_ASSET_ATTEMPTS {
        link.send(Message::AssetBegin {
            descriptor: candidate.descriptor.clone(),
            resume_offset: proposed_offset,
        })
        .await?;

        // Opening an original can block on the platform library, and on iOS it
        // may have to materialise the file. Resuming additionally re-reads the
        // skipped prefix to rebuild the digest. Both are invisible to the peer.
        link.keepalive().await?;

        let reader = source.open_original(&candidate.platform_asset_id)?;
        let mut out = if proposed_offset > 0 {
            OutboundAsset::resume(reader, size, proposed_offset)?
        } else {
            OutboundAsset::begin(reader, size)?
        };

        let mut failed = false;
        while let Some(chunk) = out.next_chunk()? {
            let mut pending = chunk;
            let mut chunk_attempts = 0u32;

            loop {
                link.write(&Frame::with_payload(
                    Message::Chunk {
                        offset: pending.offset,
                        hash: pending.hash,
                    },
                    pending.payload.clone(),
                ))
                .await?;

                match link.read().await?.message {
                    Message::ChunkAck { outcome, .. } => match outcome {
                        ChunkOutcome::Accepted { bytes_received } => {
                            session::advance(conn, session_id, candidate.id, bytes_received)?;
                            break;
                        }
                        ChunkOutcome::Corrupt => {
                            chunk_attempts += 1;
                            if chunk_attempts >= photosync_core::chunk::MAX_CHUNK_ATTEMPTS {
                                failed = true;
                                break;
                            }
                            // Resend the same bytes. Re-reading would advance the
                            // sender's digest a second time (§13.1).
                            continue;
                        }
                        ChunkOutcome::Misaligned { expected_offset } => {
                            // The receiver is authoritative. Restart the asset
                            // from where it actually is.
                            tracing::info!(
                                proposed = pending.offset,
                                expected = expected_offset,
                                "receiver corrected the offset"
                            );
                            let reader = source.open_original(&candidate.platform_asset_id)?;
                            out = OutboundAsset::resume(reader, size, expected_offset)?;
                            let Some(next) = out.next_chunk()? else {
                                failed = true;
                                break;
                            };
                            pending = next;
                            continue;
                        }
                    },
                    Message::Abort { reason, detail } => {
                        return Err(Error::PeerAborted { reason, detail })
                    }
                    other => return Err(Error::Unexpected(format!("{other:?}"))),
                }
            }
            if failed {
                break;
            }
        }

        if failed {
            let more = session::record_attempt_failure(conn, session_id, candidate.id, "chunk")?;
            if !more {
                return Ok(Delivered::Failed);
            }
            continue;
        }

        let full_hash = out
            .full_hash()
            .ok_or_else(|| Error::Unexpected("asset finished without a hash".into()))?;
        link.send(Message::AssetEnd { full_hash }).await?;

        match link.read().await?.message {
            Message::AssetAck { outcome } => match outcome {
                AssetOutcome::Committed => {
                    catalog::set_full_hash(conn, candidate.id, &full_hash)?;
                    return Ok(Delivered::Committed {
                        full_hash,
                        bytes: size,
                    });
                }
                AssetOutcome::AlreadyPresent => {
                    catalog::set_full_hash(conn, candidate.id, &full_hash)?;
                    return Ok(Delivered::AlreadyPresent { full_hash });
                }
                AssetOutcome::HashMismatch => {
                    let more =
                        session::record_attempt_failure(conn, session_id, candidate.id, "hash")?;
                    if !more {
                        return Ok(Delivered::Failed);
                    }
                    tracing::warn!(attempt, "whole-file hash mismatch, retrying the asset");
                    continue;
                }
                AssetOutcome::Failed => return Ok(Delivered::Failed),
            },
            Message::Abort { reason, detail } => return Err(Error::PeerAborted { reason, detail }),
            other => return Err(Error::Unexpected(format!("{other:?}"))),
        }
    }
    Ok(Delivered::Failed)
}

/// Records delivery: `sent_log` **and** `local_asset.full_hash`, atomically.
///
/// One transaction because the candidate query reads `local_asset.full_hash` and
/// the anti-join reads `sent_log`. A crash between the two writes would leave an
/// asset that is known-delivered but still a candidate, or worse (§12.1, §26.10).
fn record_delivered(
    conn: &Connection,
    peer_id: i64,
    local_asset_id: i64,
    full_hash: &Hash32,
    quick_hash: &Hash32,
) -> Result<()> {
    conn.execute_batch("BEGIN")?;
    let outcome = catalog::set_full_hash(conn, local_asset_id, full_hash)
        .and_then(|()| catalog::mark_sent(conn, peer_id, full_hash, quick_hash));
    match outcome {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e.into())
        }
    }
}
