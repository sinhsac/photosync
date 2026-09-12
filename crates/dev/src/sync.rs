//! Full end-to-end sync between a sender and a receiver over real TCP + TLS.
//!
//! Two independent databases, two independent library directories, one socket.
//! This is the first point at which `app_info.md` §24 criteria 1, 2 and 4 can be
//! observed rather than argued about.

use crate::cutstream::{CutAfter, Mode};
use crate::fsprovider::{DirSink, DirSource};
use photosync_core::catalog::{self, FilterArgs, ScannedAsset};
use photosync_core::identity as core_identity;
use photosync_core::model::MediaType;
use photosync_core::pairing::{CodeSession, PairingCode};
use photosync_core::{db, session};
use photosync_net::identity::Identity;
use photosync_net::link::{Link, Timeouts};
use photosync_net::orchestrator::{run_receiver, run_sender, ReceiverAuth, SenderAuth, Totals};
use photosync_net::tls::{client_config, server_config, Pinning};
use photosync_net::tls_stream::{ServerName, TlsAcceptor, TlsConnector};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use tokio::net::{TcpListener, TcpStream};

/// Runs a sync from `src_dir` into `dst_dir`, twice, and reports.
pub fn run(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { drive(src_dir, dst_dir).await })
}

/// Interrupts a sync mid-asset, then reconnects and resumes.
///
/// Success criterion 3 and criterion 7 at the session level: not just "a chunk
/// stream can resume" (which `psdev transfer cut` covers) but the whole path —
/// the session is left resumable in SQLite, the receiver reports its offsets, the
/// reconnect is authenticated by the **pinned fingerprint pair with no code**
/// (§9.5), and the result is byte-identical.
pub fn run_resume(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { drive_resume(src_dir, dst_dir).await })
}

async fn drive(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let work = std::env::temp_dir().join("psdev-sync");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let sender_db = work.join("sender.db");
    let receiver_db = work.join("receiver.db");

    println!("sender library  : {}", src_dir.display());
    println!("receiver library: {}", dst_dir.display());

    let scanned = catalogue(&sender_db, src_dir)?;
    println!("catalogued      : {scanned} assets\n");

    // --- first sync: criterion 1 -------------------------------------------
    let first = one_session(&sender_db, &receiver_db, dst_dir).await?;
    // Five assets, and two of them are the quick-key collision pair. The second
    // of that pair is streamed on a `Probable` verdict and must still be
    // committed, because their full hashes differ (§11.3).
    report("first sync", &first, scanned as u64, 0);

    let sink = DirSink::new(dst_dir)?;
    let arrived = sink.visible()?;
    println!("  files now in the receiver library: {}", arrived.len());
    let identical = verify_identical(src_dir, dst_dir)?;
    println!(
        "  byte-for-byte identical to the source: {}",
        if identical { "yes" } else { "NO" }
    );

    // Nothing may be left behind. A leftover partial means either a commit that
    // did not move its staging file or an abandon that did not delete it, and
    // both would quietly accumulate storage on a real phone (§13.3).
    let leftovers = fs::read_dir(sink.staging_dir())?.count();
    println!(
        "  staging directory empty: {}",
        if leftovers == 0 {
            "yes".to_string()
        } else {
            format!("NO, {leftovers} partial file(s) left behind")
        }
    );

    // --- second sync of an unchanged library: criterion 2 ------------------
    let second = one_session(&sender_db, &receiver_db, dst_dir).await?;
    report("second sync, unchanged", &second, 0, scanned as u64);
    println!(
        "  bytes on the wire: {} ({})",
        second.sender.bytes_done,
        if second.sender.bytes_done == 0 {
            "nothing re-sent"
        } else {
            "NO, something was re-sent"
        }
    );
    println!(
        "  all {} answered with an exact full-hash Skip: {}",
        scanned,
        if second.sender.items_skipped == scanned as u64 {
            "yes"
        } else {
            "NO"
        }
    );

    // --- add one asset and sync again: criterion 2 -------------------------
    // Unique per run. A fixed name survives in the source directory and is
    // already catalogued the next time the harness runs, which makes this case
    // silently pass zero assets and look like a regression.
    let extra = src_dir.join(format!("added-{}.bin", db::now_millis()));
    fs::write(
        &extra,
        format!("an asset created at {}", db::now_millis()).as_bytes(),
    )?;
    {
        let conn = db::open(&sender_db)?;
        if let Some(asset) = describe(&extra) {
            let out = catalog::upsert(&conn, &asset)?;
            let mut f = fs::File::open(&extra)?;
            let quick = core_identity::quick_hash(asset.size, &mut f)?;
            catalog::set_quick_hash(&conn, out.id, &quick)?;
        }
    }
    let third = one_session(&sender_db, &receiver_db, dst_dir).await?;
    report("third sync, one added", &third, 1, scanned as u64);
    println!(
        "  transferred exactly the new asset: {}",
        if third.sender.items_done == 1 {
            "yes"
        } else {
            "NO"
        }
    );

    // --- what the startup sweep does with clean sessions -------------------
    //
    // Deliberately understated: every session above finished cleanly, so this
    // only shows that the sweep leaves completed sessions alone. It does **not**
    // demonstrate resume. Criterion 7 needs a session killed mid-asset, which is
    // covered at the chunk level by `psdev transfer cut` and still needs a
    // session-level case here.
    let conn = db::open(&receiver_db)?;
    let swept = session::sweep_stale_active(&conn)?;
    println!(
        "\nstartup sweep   : {swept} session(s) were left active (expected 0 after clean runs)"
    );
    println!(
        "resume offered  : {}",
        match session::latest_resumable(&conn)? {
            Some(s) => format!("session {} ({} items done)", &s.id[..8], s.items_done),
            None => "none, as expected after clean completions".into(),
        }
    );

    Ok(())
}

fn report(label: &str, r: &SessionReport, expect_transferred: u64, expect_skipped: u64) {
    let ok = r.sender.items_done == expect_transferred
        && r.sender.items_skipped == expect_skipped
        && r.sender.items_failed == 0
        && r.receiver.items_done == expect_transferred;
    println!(
        "\n{label:<28} transferred={} skipped={} failed={} bytes={}  {}",
        r.sender.items_done,
        r.sender.items_skipped,
        r.sender.items_failed,
        r.sender.bytes_done,
        if ok { "OK" } else { "UNEXPECTED" }
    );
}

/// One paired session: fresh code, fresh TLS, both halves driven concurrently.
///
/// The receiver runs on its own OS thread with a current-thread runtime, not on
/// the shared pool. That is not a workaround for the harness — it is the shape the
/// engine has to take. A `rusqlite::Connection` is not `Sync`, so a future holding
/// one across an await is not `Send`, so it cannot be moved between worker
/// threads. Since a SQLite connection belongs to one thread anyway, the honest
/// arrangement is one thread per session owning its own connection, and that
/// carries over to the phone unchanged.
/// Both sides' totals. Kept separate because they are not the same quantity.
pub struct SessionReport {
    pub sender: Totals,
    pub receiver: Totals,
}

async fn drive_resume(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let work = std::env::temp_dir().join("psdev-resume");
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(dst_dir);
    fs::create_dir_all(&work)?;
    let sender_db = work.join("sender.db");
    let receiver_db = work.join("receiver.db");

    let scanned = catalogue(&sender_db, src_dir)?;
    println!("catalogued      : {scanned} assets");

    // Both identities persist across the two connections, which is the point:
    // §9.5 makes the fingerprint pair the credential after pairing.
    let receiver_identity = Identity::generate().expect("identity");
    let sender_identity = Identity::generate().expect("identity");

    // --- leg 1: pair with a code, then get cut off ------------------------
    let issued = CodeSession::issue(db::now_millis());
    let code = PairingCode::parse(issued.code().as_str()).expect("well formed");
    println!("code            : {}\n", issued.code().display_grouped());

    // Budget chosen to land inside an asset rather than on a boundary.
    let cut_at = 7 * 1024 * 1024;
    let leg1 = leg(
        &sender_db,
        &receiver_db,
        dst_dir,
        &receiver_identity,
        &sender_identity,
        Auth::Code(code.clone()),
        Some(cut_at),
    )
    .await?;
    println!(
        "leg 1 (cut at ~{} MB)        transferred={} skipped={}",
        cut_at / (1024 * 1024),
        leg1.receiver.items_done,
        leg1.receiver.items_skipped
    );

    // What survived, and in what state.
    {
        let conn = db::open(&receiver_db)?;
        let swept = session::sweep_stale_active(&conn)?;
        let resumable = session::latest_resumable(&conn)?;
        println!("  sessions swept to interrupted: {swept}");
        match &resumable {
            Some(s) => {
                let partials = photosync_core::inbound::in_flight(&conn, &s.id)?;
                println!(
                    "  resumable session {} with {} in-flight asset(s)",
                    &s.id[..8],
                    partials.len()
                );
                for p in &partials {
                    println!(
                        "    partial: {} of {} bytes ({:.0}%)",
                        p.bytes_received,
                        p.total_bytes,
                        (p.bytes_received as f64 / p.total_bytes as f64) * 100.0
                    );
                }
                if partials.is_empty() {
                    println!("    none — the cut landed between assets, not inside one");
                }
            }
            None => println!("  NO resumable session — the interruption was not recorded"),
        }
    }

    // --- leg 1b: a peer that freezes without closing -----------------------
    //
    // The failure mode the §8 heartbeat exists for. Nothing is closed, so the
    // receiver has no notification at all; only its own read deadline can free
    // it. Timed here, because "did not hang" is the whole assertion.
    let stall_started = std::time::Instant::now();
    let stalled = leg_with(
        &sender_db,
        &receiver_db,
        dst_dir,
        &receiver_identity,
        &sender_identity,
        Auth::Paired,
        Some((1024, Mode::Stall)),
        Timeouts::fast(),
    )
    .await;
    let stall_elapsed = stall_started.elapsed();
    let fast = Timeouts::fast();
    println!("\nleg 1b (peer freezes, socket left open)");
    match &stalled {
        Ok(r) => println!(
            "  receiver returned after {:?} (deadline {:?}) transferred={} — {}",
            stall_elapsed,
            fast.read,
            r.receiver.items_done,
            if stall_elapsed < fast.read * 8 {
                "OK, the deadline freed it"
            } else {
                "SLOW, check the deadline"
            }
        ),
        Err(e) => println!("  receiver errored instead of interrupting: {e}"),
    }

    // --- leg 2: reconnect with no code ------------------------------------
    let leg2 = leg(
        &sender_db,
        &receiver_db,
        dst_dir,
        &receiver_identity,
        &sender_identity,
        Auth::Paired,
        None,
    )
    .await?;
    println!(
        "\nleg 2 (reconnect, no code)   transferred={} skipped={} failed={}",
        leg2.receiver.items_done, leg2.sender.items_skipped, leg2.sender.items_failed
    );

    // --- the result must be complete and correct --------------------------
    let sink = DirSink::new(dst_dir)?;
    let arrived = sink.visible()?;
    let identical = verify_identical(src_dir, dst_dir)?;
    let leftovers = fs::read_dir(sink.staging_dir())?.count();
    let total_done = leg1.receiver.items_done + leg2.receiver.items_done;

    println!("\nfinal state");
    println!(
        "  assets in the receiver library : {} of {} {}",
        arrived.len(),
        scanned,
        if arrived.len() == scanned {
            "OK"
        } else {
            "INCOMPLETE"
        }
    );
    println!(
        "  byte-for-byte identical        : {}",
        if identical { "yes OK" } else { "NO" }
    );
    println!(
        "  staging empty                  : {}",
        if leftovers == 0 {
            "yes OK".to_string()
        } else {
            format!("NO, {leftovers} left")
        }
    );
    println!(
        "  committed across both legs     : {} + {} = {}",
        leg1.receiver.items_done, leg2.receiver.items_done, total_done
    );
    println!("  second leg needed no code      : yes OK, pinned fingerprint pair only (§9.5)");
    Ok(())
}

/// Which credential a leg uses.
enum Auth {
    /// First contact. The receiver rebuilds a `CodeSession` around this code, the
    /// same way the real receiver hands its displayed code down to the transport.
    Code(PairingCode),
    /// Reconnect. Nothing but the pinned fingerprints (§9.5).
    Paired,
}

/// Same as [`catalogue`], for the discovery harness.
pub fn catalogue_pub(sender_db: &Path, src_dir: &Path) -> photosync_core::Result<usize> {
    catalogue(sender_db, src_dir)
}

/// Catalogues and hashes a directory into a sender database.
fn catalogue(sender_db: &Path, src_dir: &Path) -> photosync_core::Result<usize> {
    let conn = db::open(sender_db)?;
    conn.execute_batch("BEGIN")?;
    for path in walk(src_dir) {
        if let Some(asset) = describe(&path) {
            catalog::upsert(&conn, &asset)?;
        }
    }
    conn.execute_batch("COMMIT")?;

    let mut hashed = 0usize;
    loop {
        let batch = catalog::pending_hashes(&conn, 512)?;
        if batch.is_empty() {
            break;
        }
        conn.execute_batch("BEGIN")?;
        for p in &batch {
            let mut f = BufReader::new(fs::File::open(&p.platform_asset_id)?);
            let quick = core_identity::quick_hash(p.size, &mut f)?;
            catalog::set_quick_hash(&conn, p.id, &quick)?;
            hashed += 1;
        }
        conn.execute_batch("COMMIT")?;
    }
    Ok(hashed)
}

/// One connection with an explicit credential and an optional injected cut.
///
/// Identities are supplied by the caller so they persist across legs — without
/// that, a "reconnect" would present a new certificate and §9.5 would not apply.
async fn leg(
    sender_db: &Path,
    receiver_db: &Path,
    dst_dir: &Path,
    receiver_identity: &Identity,
    sender_identity: &Identity,
    auth: Auth,
    cut_after: Option<u64>,
) -> photosync_core::Result<SessionReport> {
    leg_with(
        sender_db,
        receiver_db,
        dst_dir,
        receiver_identity,
        sender_identity,
        auth,
        cut_after.map(|n| (n, Mode::Cut)),
        Timeouts::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn leg_with(
    sender_db: &Path,
    receiver_db: &Path,
    dst_dir: &Path,
    receiver_identity: &Identity,
    sender_identity: &Identity,
    auth: Auth,
    fault: Option<(u64, Mode)>,
    timeouts: Timeouts,
) -> photosync_core::Result<SessionReport> {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    std_listener.set_nonblocking(true)?;
    let addr = std_listener.local_addr()?;

    let paired = matches!(auth, Auth::Paired);
    let receiver_pinning = if paired {
        // A paired reconnect is authenticated by TLS alone, so the expectation
        // has to be set here. `ReceiverAuth::Paired` re-checks it.
        Pinning::Expect(sender_identity.fingerprint())
    } else {
        Pinning::FirstContact
    };
    let sender_pinning = if paired {
        Pinning::Expect(receiver_identity.fingerprint())
    } else {
        Pinning::FirstContact
    };

    let (server_cfg, server_observed) =
        server_config(receiver_identity, receiver_pinning).expect("server config");
    let (client_cfg, client_observed) =
        client_config(sender_identity, sender_pinning).expect("client config");

    let receiver_db_path = receiver_db.to_path_buf();
    let dst = dst_dir.to_path_buf();
    let receiver_fp = receiver_identity.fingerprint();
    let sender_fp = sender_identity.fingerprint();

    // The code session has to move into the receiver thread; the sender keeps
    // the code itself.
    let mut owned_code_session = match &auth {
        Auth::Code(code) => Some(CodeSession::with_code(code.clone(), db::now_millis())),
        Auth::Paired => None,
    };

    let receiver_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("receiver runtime");
        rt.block_on(async move {
            let listener = TcpListener::from_std(std_listener).expect("listener");
            let (tcp, _) = listener.accept().await.expect("accept");
            let tls = TlsAcceptor::from(server_cfg)
                .accept(tcp)
                .await
                .map_err(|e| format!("tls accept: {e}"))?;
            let peer_fp = server_observed.get().ok_or("no client certificate")?;

            let conn = db::open(&receiver_db_path).map_err(|e| e.to_string())?;
            let sink = DirSink::new(&dst).map_err(|e| e.to_string())?;
            session::sweep_stale_active(&conn).map_err(|e| e.to_string())?;
            let resume_id = session::latest_resumable(&conn)
                .map_err(|e| e.to_string())?
                .map(|s| s.id);

            let auth = match owned_code_session.as_mut() {
                Some(cs) => ReceiverAuth::Code(cs),
                None => ReceiverAuth::Paired {
                    expected: sender_fp,
                },
            };

            run_receiver(
                &mut Link::new(tls, timeouts),
                &conn,
                &sink,
                receiver_fp,
                peer_fp,
                auth,
                "psdev receiver",
                db::now_millis(),
                resume_id,
            )
            .await
            .map_err(|e| e.to_string())
        })
    });

    let tcp = TcpStream::connect(addr).await?;
    let tls = TlsConnector::from(client_cfg)
        .connect(ServerName::try_from("127.0.0.1").expect("name"), tcp)
        .await?;
    let peer_fp = client_observed.get().expect("server certificate");

    let conn = db::open(sender_db)?;
    let source = DirSource;
    let sender_auth = match &auth {
        Auth::Code(code) => SenderAuth::Code(code),
        Auth::Paired => SenderAuth::Paired,
    };

    // Wrapping the TLS stream, so the cut lands on a protocol write rather than
    // inside a TLS record. That is what a lost uplink looks like to this layer.
    let (budget, mode) = fault.unwrap_or((u64::MAX, Mode::Cut));
    let mut wrapped = Link::new(CutAfter::with_mode(tls, budget, mode), timeouts);
    let sender_result = run_sender(
        &mut wrapped,
        &conn,
        &source,
        sender_identity.fingerprint(),
        peer_fp,
        sender_auth,
        "psdev sender",
        true,
    )
    .await;

    // Close the socket before waiting on the receiver.
    //
    // `CutAfter` only makes writes fail; the underlying TCP connection stays open
    // while the stream is alive, so the receiver would block on a read that never
    // completes. Dropping it turns the injected failure into a real disconnect.
    //
    // Worth noting rather than just fixing: in the field the same situation is a
    // half-open connection, and nothing at the socket layer resolves it. That is
    // what §8's heartbeat with a 30s timeout is for. Until that is wired up, a
    // peer that vanishes without closing will hang a session indefinitely.
    let cut_stream = wrapped.into_inner();
    let was_cut = cut_stream.was_cut();
    drop(cut_stream);

    let receiver_result = receiver_thread.join().expect("receiver thread");

    let sent = match sender_result {
        Ok(t) => t,
        Err(_) if was_cut => {
            // Expected: the cut was the point. The sender's own totals are lost
            // with the connection, which is realistic — the receiver's state is
            // the one that survives.
            Totals::default()
        }
        Err(e) => {
            if let Err(inner) = &receiver_result {
                println!("  receiver failed first: {inner}");
            }
            return Err(photosync_core::Error::Protocol {
                detail: format!("sender: {e}"),
            });
        }
    };

    let received = receiver_result.map_err(|e| photosync_core::Error::Protocol {
        detail: format!("receiver: {e}"),
    })?;

    Ok(SessionReport {
        sender: sent,
        receiver: received,
    })
}

async fn one_session(
    sender_db: &Path,
    receiver_db: &Path,
    dst_dir: &Path,
) -> photosync_core::Result<SessionReport> {
    let receiver_identity = Identity::generate().expect("identity");
    let sender_identity = Identity::generate().expect("identity");

    let mut code_session = CodeSession::issue(db::now_millis());
    let code = PairingCode::parse(code_session.code().as_str()).expect("well formed");

    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    std_listener.set_nonblocking(true)?;
    let addr = std_listener.local_addr()?;

    let (server_cfg, server_observed) =
        server_config(&receiver_identity, Pinning::FirstContact).expect("server config");
    let (client_cfg, client_observed) =
        client_config(&sender_identity, Pinning::FirstContact).expect("client config");

    let receiver_db = receiver_db.to_path_buf();
    let dst = dst_dir.to_path_buf();
    let receiver_fp = receiver_identity.fingerprint();

    let receiver_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("receiver runtime");
        rt.block_on(async move {
            let listener = TcpListener::from_std(std_listener).expect("listener");
            let (tcp, _) = listener.accept().await.expect("accept");
            let tls = TlsAcceptor::from(server_cfg)
                .accept(tcp)
                .await
                .expect("tls accept");
            let peer_fp = server_observed.get().expect("client certificate");

            let conn = db::open(&receiver_db).expect("receiver db");
            let sink = DirSink::new(&dst).expect("sink");
            // Resume the newest interrupted session if there is one, so in-flight
            // offsets survive a reconnect (§13.4).
            session::sweep_stale_active(&conn).expect("sweep");
            let resume_id = session::latest_resumable(&conn)
                .expect("resumable")
                .map(|s| s.id);

            run_receiver(
                &mut Link::new(tls, Timeouts::default()),
                &conn,
                &sink,
                receiver_fp,
                peer_fp,
                ReceiverAuth::Code(&mut code_session),
                "psdev receiver",
                db::now_millis(),
                resume_id,
            )
            .await
            .map_err(|e| e.to_string())
        })
    });

    let tcp = TcpStream::connect(addr).await?;
    let tls = TlsConnector::from(client_cfg)
        .connect(ServerName::try_from("127.0.0.1").expect("name"), tcp)
        .await?;
    let peer_fp = client_observed.get().expect("server certificate");

    let conn = db::open(sender_db)?;
    let source = DirSource;
    let sender_result = run_sender(
        &mut Link::new(tls, Timeouts::default()),
        &conn,
        &source,
        sender_identity.fingerprint(),
        peer_fp,
        SenderAuth::Code(&code),
        "psdev sender",
        true,
    )
    .await;

    // Always collect the receiver's result, even when the sender failed first.
    // A receiver that errors out closes the socket, and the sender then reports
    // only "connection reset" — which hides the actual cause.
    let receiver_result = receiver_thread.join().expect("receiver thread");

    let sent = match sender_result {
        Ok(t) => t,
        Err(e) => {
            if let Err(inner) = &receiver_result {
                println!("  receiver failed first: {inner}");
            }
            return Err(photosync_core::Error::Protocol {
                detail: format!("sender: {e}"),
            });
        }
    };
    let received = receiver_result.map_err(|e| photosync_core::Error::Protocol {
        detail: format!("receiver: {e}"),
    })?;

    // Only `items_done` is comparable, and it must match exactly: a divergence
    // there would surface as a sync that never reaches 100%.
    //
    // The skipped counters legitimately differ, and it is worth knowing why. A
    // `Skip` verdict means the asset was never transferred, so the receiver has
    // nothing to count; its own skipped counter only records assets that *were*
    // streamed on a `Probable` verdict and turned out to be duplicates (§11.3).
    // An earlier version of this check compared both and produced a false alarm.
    if sent.items_done != received.items_done {
        println!(
            "  WARNING items_done disagrees: sender {} vs receiver {}",
            sent.items_done, received.items_done
        );
    }
    Ok(SessionReport {
        sender: sent,
        receiver: received,
    })
}

/// Confirms every source file arrived with identical bytes (§16: originals are
/// transferred unmodified).
fn verify_identical(src: &Path, dst: &Path) -> photosync_core::Result<bool> {
    use std::collections::HashMap;
    let mut by_hash: HashMap<String, u64> = HashMap::new();
    for path in walk(src) {
        let size = fs::metadata(&path)?.len();
        let mut f = fs::File::open(&path)?;
        let h = core_identity::quick_hash(size, &mut f)?;
        *by_hash.entry(h.to_hex()).or_insert(0) += 1;
    }
    for path in walk(dst) {
        if path.components().any(|c| c.as_os_str() == ".staging") {
            continue;
        }
        let size = fs::metadata(&path)?.len();
        let mut f = fs::File::open(&path)?;
        let h = core_identity::quick_hash(size, &mut f)?;
        match by_hash.get_mut(&h.to_hex()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => return Ok(false),
        }
    }
    Ok(by_hash.values().all(|n| *n == 0))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p
                .file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false);
            match e.file_type() {
                Ok(t) if t.is_dir() && !hidden => stack.push(p),
                Ok(t) if t.is_file() && !hidden => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn describe(path: &Path) -> Option<ScannedAsset> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Some(ScannedAsset {
        platform_asset_id: path.to_string_lossy().into_owned(),
        size: meta.len(),
        media_type: MediaType::Image,
        mime: "application/octet-stream".into(),
        created_at: modified,
        modified_at: modified,
        width: None,
        height: None,
        duration_ms: None,
        display_name: path.file_name().map(|s| s.to_string_lossy().into_owned()),
        resource_group_id: None,
        is_local: true,
    })
}

/// Unused today, kept because the counters query needs a peer id and the harness
/// will want to inspect them between sessions.
#[allow(dead_code)]
fn counters_for(conn: &rusqlite::Connection, peer_id: i64) -> photosync_core::Result<()> {
    let c = catalog::counters(
        conn,
        FilterArgs {
            peer_id,
            include_videos: true,
        },
    )?;
    println!("  counters: {c:?}");
    Ok(())
}
