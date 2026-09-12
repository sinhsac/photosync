//! Running a session from the app.
//!
//! One session owns one thread, which owns one `rusqlite::Connection` — that is
//! not a convenience, it is forced: a `Connection` is not `Sync`, so a future
//! holding one across an await is not `Send` and cannot be moved between worker
//! threads. A SQLite connection belongs to a thread anyway, so this is the honest
//! arrangement rather than a workaround.
//!
//! The UI never blocks. Starting a session returns immediately with the pairing
//! code; progress arrives through [`crate::log`] and [`status`].

use crate::log;
use crate::store::{MediaStoreSink, MediaStoreSource};
use photosync_core::pairing::{CodeSession, PairingCode};
use photosync_core::proto::PROTOCOL_MAJOR;
use photosync_core::{catalog, db, identity, session as core_session};
use photosync_net::discovery::{self, Announcement};
use photosync_net::identity::Identity;
use photosync_net::link::{Link, Timeouts};
use photosync_net::orchestrator::{run_receiver, run_sender, ReceiverAuth, SenderAuth};
use photosync_net::tls::{client_config, server_config, Pinning};
use photosync_net::tls_stream::{ServerName, TlsAcceptor, TlsConnector};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::net::{TcpListener, TcpStream};

/// Whether a session already owns the port and the database.
///
/// Not defensive tidiness: two receivers cannot coexist, because the second one
/// fails to bind 53411 and reports "address already in use" — which is a confusing
/// thing to show a user whose only mistake was tapping the button twice. Worse, the
/// failure arrives on the second session's thread and overwrites the *first*
/// session's status, so a perfectly healthy receiver appears to have died.
///
/// Claiming here, before anything is started, makes the second tap a no-op with an
/// honest message.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Takes the session slot, or reports that it is taken.
fn claim() -> bool {
    !BUSY.swap(true, Ordering::SeqCst)
}

/// Releases the slot. Must run on every exit path, success or failure.
fn release() {
    BUSY.store(false, Ordering::SeqCst);
}

/// What the UI polls.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub running: bool,
    pub finished: bool,
    pub items_done: u64,
    pub items_skipped: u64,
    pub items_failed: u64,
    pub bytes_done: u64,
    pub error: Option<String>,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);

fn set_status(s: Status) {
    if let Ok(mut guard) = STATUS.lock() {
        *guard = Some(s);
    }
}

fn update_status(f: impl FnOnce(&mut Status)) {
    if let Ok(mut guard) = STATUS.lock() {
        if let Some(s) = guard.as_mut() {
            f(s);
        }
    }
}

/// Current status as JSON, for the UI.
pub fn status_json() -> String {
    let s = STATUS
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    serde_json::to_string(&s).unwrap_or_else(|_| "{}".into())
}

/// Returned by the two start functions.
#[derive(Serialize)]
struct StartInfo {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn failure(error: impl std::fmt::Display) -> String {
    serde_json::to_string(&StartInfo {
        ok: false,
        code: None,
        fingerprint: None,
        addresses: Vec::new(),
        error: Some(error.to_string()),
    })
    .unwrap_or_else(|_| "{\"ok\":false}".into())
}

// ---------------------------------------------------------------------------
// Scanning the real photo library (§10.3, §15.3)
// ---------------------------------------------------------------------------

/// Catalogues MediaStore into SQLite, then drains the hashing queue.
///
/// Returns JSON with the counts. Runs on the calling thread, so the app must call
/// it off the UI thread.
///
/// Paged deliberately: §21 forbids loading the library into memory, and a phone
/// with 100,000 assets would otherwise build one enormous list before doing any
/// work.
pub fn scan_library(db_path: &str) -> String {
    let outcome = (|| -> Result<(usize, usize), String> {
        let conn = db::open(db_path).map_err(|e| e.to_string())?;

        // Ask MediaStore how many assets it has *before* walking it, so the log can
        // compare the two. §1 promises the whole library, and "catalogued 3221" only
        // means something next to "MediaStore reports 3221".
        let expected = match crate::store::count() {
            Ok((images, videos)) => {
                log::line(format!("library holds {images} images and {videos} videos"));
                Some(images + videos)
            }
            Err(e) => {
                log::line(format!("cannot count the library: {e}"));
                None
            }
        };

        const PAGE: i32 = 500;
        let mut after_image = crate::store::NO_WATERMARK;
        let mut after_video = crate::store::NO_WATERMARK;
        let mut catalogued = 0usize;

        loop {
            let page = crate::store::enumerate(PAGE, after_image, after_video)
                .map_err(|e| e.to_string())?;
            let rows = &page.rows;
            if rows.is_empty() {
                break;
            }
            // Advance before the insert loop: a watermark that only moves on success
            // would replay the same page forever if one row failed.
            after_image = page.after_image_id;
            after_video = page.after_video_id;

            conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
            for row in rows {
                let asset = catalog::ScannedAsset {
                    platform_asset_id: row.id.clone(),
                    size: row.size,
                    media_type: if row.video {
                        photosync_core::model::MediaType::Video
                    } else {
                        photosync_core::model::MediaType::Image
                    },
                    mime: row.mime.clone(),
                    created_at: row.taken,
                    modified_at: row.modified,
                    width: row.width,
                    height: row.height,
                    duration_ms: row.duration,
                    display_name: Some(row.name.clone()),
                    resource_group_id: None,
                    is_local: true,
                };
                catalog::upsert(&conn, &asset).map_err(|e| e.to_string())?;
                catalogued += 1;
            }
            conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            log::line(format!("catalogued {catalogued} assets so far"));
            if rows.len() < PAGE as usize {
                break;
            }
        }

        // A mismatch is not fatal — zero-byte rows are dropped on purpose, so a small
        // shortfall is expected — but it must be visible. Silently cataloguing fewer
        // assets than the library holds is the one failure this app cannot afford to
        // look like success.
        if let Some(expected) = expected {
            if (catalogued as u64) != expected {
                log::line(format!(
                    "note: MediaStore reported {expected} assets, catalogued {catalogued} \
                     (zero-byte and unreadable rows are skipped)"
                ));
            }
        }

        // Drain the hash queue. `quick_hash IS NULL` *is* the queue (§15.3), so a
        // hash is computed once per asset ever and a crash costs one batch.
        let source = MediaStoreSource;
        let mut hashed = 0usize;
        loop {
            let batch = catalog::pending_hashes(&conn, 64).map_err(|e| e.to_string())?;
            if batch.is_empty() {
                break;
            }
            conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
            for pending in &batch {
                use photosync_core::provider::LibrarySource;
                match source.open_original(&pending.platform_asset_id) {
                    Ok(mut reader) => match identity::quick_hash(pending.size, &mut reader) {
                        Ok(quick) => {
                            catalog::set_quick_hash(&conn, pending.id, &quick)
                                .map_err(|e| e.to_string())?;
                            hashed += 1;
                        }
                        Err(e) => {
                            // One unreadable asset must not stop the scan.
                            log::line(format!("skipping {}: {e}", pending.platform_asset_id));
                            catalog::delete_by_platform_id(&conn, &pending.platform_asset_id)
                                .map_err(|e| e.to_string())?;
                        }
                    },
                    Err(e) => {
                        log::line(format!("cannot open {}: {e}", pending.platform_asset_id));
                        catalog::delete_by_platform_id(&conn, &pending.platform_asset_id)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            log::line(format!("hashed {hashed}"));
        }

        Ok((catalogued, hashed))
    })();

    match outcome {
        Ok((catalogued, hashed)) => {
            log::line(format!(
                "scan complete: {catalogued} assets, {hashed} hashed"
            ));
            format!("{{\"ok\":true,\"catalogued\":{catalogued},\"hashed\":{hashed}}}")
        }
        Err(e) => {
            log::line(format!("scan failed: {e}"));
            failure(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Binds, issues a code, and runs the receiving session on its own thread.
///
/// Returns as soon as the code exists so the UI can show it (§6). Everything
/// after that is reported through the log and [`status_json`].
pub fn start_receiver(db_path: &str) -> String {
    if !claim() {
        return failure("a session is already running");
    }
    let identity = match Identity::generate() {
        Ok(i) => i,
        Err(e) => {
            release();
            return failure(e);
        }
    };
    let issued = CodeSession::issue(db::now_millis());
    let code = match PairingCode::parse(issued.code().as_str()) {
        Some(c) => c,
        None => {
            release();
            return failure("generated code is malformed");
        }
    };
    let shown = issued.code().display_grouped();
    let fingerprint = identity.fingerprint().short();

    let addresses: Vec<String> = discovery::local_ipv4(false)
        .into_iter()
        .map(|ip| format!("{ip}:{}", discovery::DEFAULT_PORT))
        .collect();

    set_status(Status {
        running: true,
        ..Default::default()
    });
    log::line(format!("receiver ready, code {shown}"));

    let db_path = db_path.to_string();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                fail_session(e);
                return;
            }
        };
        rt.block_on(async move {
            if let Err(e) = receiver_loop(&db_path, identity, code).await {
                fail_session(e);
            }
        });
    });

    serde_json::to_string(&StartInfo {
        ok: true,
        code: Some(shown),
        fingerprint: Some(fingerprint),
        addresses,
        error: None,
    })
    .unwrap_or_else(|_| "{\"ok\":true}".into())
}

fn fail_session(e: impl std::fmt::Display) {
    let msg = e.to_string();
    log::line(format!("failed: {msg}"));
    update_status(|s| {
        s.running = false;
        s.finished = true;
        s.error = Some(msg);
    });
    release();
}

/// Records a completed session and frees the slot.
fn finish_session(totals: &photosync_net::orchestrator::Totals) {
    update_status(|s| {
        s.running = false;
        s.finished = true;
        s.items_done = totals.items_done;
        s.items_skipped = totals.items_skipped;
        s.items_failed = totals.items_failed;
        s.bytes_done = totals.bytes_done;
    });
    release();
}

async fn receiver_loop(db_path: &str, identity: Identity, code: PairingCode) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", discovery::DEFAULT_PORT))
        .await
        .map_err(|e| format!("cannot bind port {}: {e}", discovery::DEFAULT_PORT))?;

    let announcement = Announcement {
        v: PROTOCOL_MAJOR,
        port: discovery::DEFAULT_PORT,
        fp: identity.fingerprint(),
        name: android_model(),
        platform: "android".into(),
    };
    let announcer = tokio::spawn(async move {
        loop {
            let _ = discovery::announce(&announcement, discovery::DEFAULT_PORT, false).await;
            tokio::time::sleep(discovery::ANNOUNCE_PERIOD).await;
        }
    });

    let conn = db::open(db_path).map_err(|e| e.to_string())?;
    let sink = MediaStoreSink;
    core_session::sweep_stale_active(&conn).map_err(|e| e.to_string())?;
    let mut code_session = CodeSession::with_code(code, db::now_millis());

    // Accept in a loop. A connection that goes nowhere is normal — the peer's own
    // discovery scan produces one — and exiting on it closes the port the sender
    // is about to use (§9.6, §26.12).
    let totals = loop {
        if !code_session.is_live(db::now_millis()) {
            announcer.abort();
            return Err("the code expired or was used up".into());
        }

        let (tcp, from) = listener.accept().await.map_err(|e| e.to_string())?;
        let (cfg, observed) = server_config(&identity, Pinning::FirstContact)
            .map_err(|e| format!("tls config: {e}"))?;

        let tls = match TlsAcceptor::from(cfg).accept(tcp).await {
            Ok(tls) => tls,
            Err(_) => continue, // almost always a discovery probe
        };
        let Some(peer_fp) = observed.get() else {
            continue;
        };
        log::line(format!("connected: {from} ({})", peer_fp.short()));

        let resume_id = core_session::latest_resumable(&conn)
            .map_err(|e| e.to_string())?
            .map(|s| s.id);
        if resume_id.is_some() {
            log::line("resuming an interrupted session");
        }

        match run_receiver(
            &mut Link::new(tls, Timeouts::default()),
            &conn,
            &sink,
            identity.fingerprint(),
            peer_fp,
            ReceiverAuth::Code(&mut code_session),
            &android_model(),
            db::now_millis(),
            resume_id,
        )
        .await
        {
            Ok(totals) => break totals,
            Err(e) => {
                log::line(format!(
                    "attempt failed ({e}); attempts left {}",
                    code_session.attempts_left()
                ));
                continue;
            }
        }
    };
    announcer.abort();

    log::line(format!(
        "done: {} saved, {} already present, {} failed, {} bytes",
        totals.items_done, totals.items_skipped, totals.items_failed, totals.bytes_done
    ));
    finish_session(&totals);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// Discovers a peer (or uses `addr`) and sends the catalogued library.
pub fn start_sender(db_path: &str, code_text: &str, addr: Option<String>) -> String {
    let Some(code) = PairingCode::parse(code_text) else {
        return failure("that is not a 6-digit code");
    };
    if !claim() {
        return failure("a session is already running");
    }
    let identity = match Identity::generate() {
        Ok(i) => i,
        Err(e) => {
            release();
            return failure(e);
        }
    };

    set_status(Status {
        running: true,
        ..Default::default()
    });
    log::line("sender starting");

    let db_path = db_path.to_string();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                fail_session(e);
                return;
            }
        };
        rt.block_on(async move {
            if let Err(e) = sender_run(&db_path, identity, code, addr).await {
                fail_session(e);
            }
        });
    });

    serde_json::to_string(&StartInfo {
        ok: true,
        code: None,
        fingerprint: None,
        addresses: Vec::new(),
        error: None,
    })
    .unwrap_or_else(|_| "{\"ok\":true}".into())
}

async fn sender_run(
    db_path: &str,
    identity: Identity,
    code: PairingCode,
    addr: Option<String>,
) -> Result<(), String> {
    let target: std::net::SocketAddr = match addr {
        Some(a) => {
            let a = if a.contains(':') {
                a
            } else {
                format!("{a}:{}", discovery::DEFAULT_PORT)
            };
            a.parse().map_err(|_| format!("cannot parse address {a}"))?
        }
        None => {
            log::line("looking for the other device...");
            let found =
                discovery::discover_staged(discovery::DEFAULT_PORT, identity.fingerprint(), false)
                    .await
                    .map_err(|e| e.to_string())?;
            let groups = discovery::group_by_peer(found);
            let first = groups
                .first()
                .and_then(|g| g.first())
                .ok_or("nothing found on this network")?;
            if let Some(a) = &first.announced {
                log::line(format!("found {} at {}", a.name, first.addr));
            }
            first.addr
        }
    };

    log::line(format!("connecting to {target}"));
    let (cfg, observed) =
        client_config(&identity, Pinning::FirstContact).map_err(|e| format!("tls config: {e}"))?;
    let tcp = TcpStream::connect(target)
        .await
        .map_err(|e| format!("cannot reach {target}: {e}"))?;
    let name = ServerName::try_from(target.ip().to_string())
        .map_err(|_| "cannot build a server name".to_string())?;
    let tls = TlsConnector::from(cfg)
        .connect(name, tcp)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    let peer_fp = observed.get().ok_or("no server certificate")?;
    log::line(format!("peer {}", peer_fp.short()));

    let conn = db::open(db_path).map_err(|e| e.to_string())?;
    let source = MediaStoreSource;
    let totals = run_sender(
        &mut Link::new(tls, Timeouts::default()),
        &conn,
        &source,
        identity.fingerprint(),
        peer_fp,
        SenderAuth::Code(&code),
        &android_model(),
        true,
    )
    .await
    .map_err(|e| e.to_string())?;

    log::line(format!(
        "done: {} sent, {} already there, {} failed, {} bytes",
        totals.items_done, totals.items_skipped, totals.items_failed, totals.bytes_done
    ));
    finish_session(&totals);
    Ok(())
}

/// Device name for `Hello` and the announcement. Falls back cheaply rather than
/// reaching back into Java for `Build.MODEL`.
fn android_model() -> String {
    std::env::var("ANDROID_MODEL").unwrap_or_else(|_| "Android".into())
}
