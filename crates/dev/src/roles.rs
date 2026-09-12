//! The two roles as separate processes (`app_info.md` §5).
//!
//! Everything before this ran both halves in one process. These commands are the
//! real shape: one device receives and shows a code, another sends after the code
//! is typed in. The code travels out of band — read off one screen, typed on the
//! other — which is the entire point of §6.

use crate::fsprovider::{DirSink, DirSource};
use photosync_core::pairing::{CodeSession, PairingCode};
use photosync_core::proto::PROTOCOL_MAJOR;
use photosync_core::{db, session};
use photosync_net::discovery::{self, Announcement};
use photosync_net::identity::Identity;
use photosync_net::link::{Link, Timeouts};
use photosync_net::orchestrator::{run_receiver, run_sender, ReceiverAuth, SenderAuth};
use photosync_net::tls::{client_config, server_config, Pinning};
use photosync_net::tls_stream::{ServerName, TlsAcceptor, TlsConnector};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::net::{TcpListener, TcpStream};

/// Receives into `dst`, announcing itself until a sender connects.
///
/// Prints the pairing code and then waits. Announces repeatedly rather than once,
/// because the sender may not be listening yet when the receiver starts — the
/// user reads the code, walks to the other phone, and types it.
pub fn serve(dst: &Path, db_path: &Path) -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { serve_inner(dst, db_path).await })
}

async fn serve_inner(dst: &Path, db_path: &Path) -> photosync_core::Result<()> {
    std::fs::create_dir_all(dst)?;

    let identity = Identity::generate().expect("identity");
    let listener = TcpListener::bind(("0.0.0.0", discovery::DEFAULT_PORT)).await?;

    let issued = CodeSession::issue(db::now_millis());
    let code = PairingCode::parse(issued.code().as_str()).expect("well formed");

    println!("PhotoSync receiver");
    println!("  library     : {}", dst.display());
    println!("  fingerprint : {}", identity.fingerprint().short());
    println!("  listening   : 0.0.0.0:{}", discovery::DEFAULT_PORT);
    for ip in discovery::local_ipv4(false) {
        println!("  address     : {ip}:{}", discovery::DEFAULT_PORT);
    }
    println!("\n  CODE: {}\n", issued.code().display_grouped());
    println!("Waiting for a sender. Enter that code on the sending device.");

    // Keep announcing until someone connects. One burst is not enough when a
    // human is walking between two phones.
    let announcement = Announcement {
        v: PROTOCOL_MAJOR,
        port: discovery::DEFAULT_PORT,
        fp: identity.fingerprint(),
        name: hostname(),
        platform: std::env::consts::OS.into(),
    };
    let announcer = tokio::spawn(async move {
        loop {
            let _ = discovery::announce(&announcement, discovery::DEFAULT_PORT, false).await;
            tokio::time::sleep(discovery::ANNOUNCE_PERIOD).await;
        }
    });

    let conn = db::open(db_path)?;
    let sink = DirSink::new(dst)?;
    session::sweep_stale_active(&conn)?;

    let mut code_session = CodeSession::with_code(code, db::now_millis());

    // **Accept in a loop.** A single accept was wrong in a way that only showed
    // up between two real devices: the sender's own subnet scan connects and
    // drops, that looked like a failed session, and the receiver exited — so by
    // the time the sender tried to connect properly, the port was closed.
    //
    // A connection that goes nowhere is normal. Discovery probes, port scanners,
    // and a peer whose Wi-Fi drops mid-handshake all produce one. The receiver
    // keeps listening until a session completes or the code is spent (§6, §9.6).
    let (totals, elapsed) = loop {
        if !code_session.is_live(db::now_millis()) {
            println!("\nThe code expired or was used up. Restart to get a new one.");
            announcer.abort();
            return Ok(());
        }

        let (tcp, from) = listener.accept().await?;

        let (cfg, observed) =
            server_config(&identity, Pinning::FirstContact).expect("server config");
        let tls = match TlsAcceptor::from(cfg).accept(tcp).await {
            Ok(tls) => tls,
            Err(e) => {
                // Almost always a discovery probe. Not worth alarming the user.
                tracing::debug!(%from, "connection closed before TLS completed: {e}");
                continue;
            }
        };
        let Some(peer_fp) = observed.get() else {
            continue;
        };

        println!(
            "\nconnection from {from}, peer fingerprint {}",
            peer_fp.short()
        );

        let resume_id = session::latest_resumable(&conn)?.map(|s| s.id);
        if resume_id.is_some() {
            println!("resuming an interrupted session");
        }

        let started = std::time::Instant::now();
        match run_receiver(
            &mut Link::new(tls, Timeouts::default()),
            &conn,
            &sink,
            identity.fingerprint(),
            peer_fp,
            ReceiverAuth::Code(&mut code_session),
            &hostname(),
            db::now_millis(),
            resume_id,
        )
        .await
        {
            Ok(totals) => break (totals, started.elapsed()),
            Err(e) => {
                // A wrong code costs one attempt and the loop continues (§6).
                println!("that attempt failed: {e}");
                println!("attempts left: {}", code_session.attempts_left());
                continue;
            }
        }
    };
    announcer.abort();

    println!("\nreceived : {} committed", totals.items_done);
    println!("skipped  : {}", totals.items_skipped);
    println!("failed   : {}", totals.items_failed);
    println!("bytes    : {}", totals.bytes_done);
    if elapsed.as_secs_f64() > 0.0 {
        println!(
            "rate     : {:.1} MB/s over {:.1}s",
            totals.bytes_done as f64 / 1_048_576.0 / elapsed.as_secs_f64(),
            elapsed.as_secs_f64()
        );
    }
    println!("library  : {} file(s)", sink.visible()?.len());
    Ok(())
}

/// Sends `src` to whoever answers the given code.
///
/// With no address, discovery finds the peer (§7). With one, it is used directly,
/// which is §7.5's escape hatch and also what makes this usable when multicast is
/// blocked.
pub fn send(
    src: &Path,
    db_path: &Path,
    code_text: &str,
    addr: Option<&str>,
) -> photosync_core::Result<()> {
    let Some(code) = PairingCode::parse(code_text) else {
        eprintln!("that is not a 6-digit code");
        return Ok(());
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { send_inner(src, db_path, code, addr).await })
}

async fn send_inner(
    src: &Path,
    db_path: &Path,
    code: PairingCode,
    addr: Option<&str>,
) -> photosync_core::Result<()> {
    // A fresh identity every run, because the harness has nowhere safe to keep a
    // private key. The product persists it in the OS keystore (§9.1), and that
    // difference matters: with a new certificate each run the peer sees a new
    // device, so §9.5's code-free reconnect cannot be exercised across separate
    // invocations here. `psdev resume-session` covers that path instead, by
    // holding one identity across both legs.
    let identity = Identity::generate().expect("identity");

    println!("PhotoSync sender");
    println!("  library     : {}", src.display());
    println!("  fingerprint : {}", identity.fingerprint().short());

    // Reports assets *newly hashed*, not assets catalogued. On a second run this
    // is zero because a hash is computed once and cached forever (§15.3) — which
    // read as "nothing found" until the label was fixed.
    let hashed = crate::sync::catalogue_pub(db_path, src)?;
    println!("  newly hashed: {hashed} (already-hashed assets are reused)");

    let target: SocketAddr = match addr {
        Some(a) => {
            let a = if a.contains(':') {
                a.to_string()
            } else {
                format!("{a}:{}", discovery::DEFAULT_PORT)
            };
            a.parse().map_err(|_| photosync_core::Error::Protocol {
                detail: format!("cannot parse address {a:?}"),
            })?
        }
        None => {
            println!("\ndiscovering...");
            let candidates =
                discovery::discover_staged(discovery::DEFAULT_PORT, identity.fingerprint(), false)
                    .await
                    .map_err(|e| photosync_core::Error::Protocol {
                        detail: format!("discovery: {e}"),
                    })?;

            let groups = discovery::group_by_peer(candidates);
            if groups.is_empty() {
                println!("nothing found. Make sure both devices are on the same Wi-Fi, or pass an address.");
                return Ok(());
            }
            for group in &groups {
                if let Some(a) = group.first().and_then(|c| c.announced.as_ref()) {
                    println!(
                        "  {:?} on {} (fp {}) at {}",
                        a.name,
                        a.platform,
                        a.fp.short(),
                        group
                            .iter()
                            .map(|c| c.addr.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            // Any candidate will do: authentication decides whether it is the one
            // the code belongs to (§7.2). Trying them in turn is the product
            // behaviour; here the first is enough.
            groups[0][0].addr
        }
    };

    println!("\nconnecting to {target}");
    let (cfg, observed) = client_config(&identity, Pinning::FirstContact).expect("client config");
    let tcp = TcpStream::connect(target).await?;
    let tls = TlsConnector::from(cfg)
        .connect(
            ServerName::try_from(target.ip().to_string()).map_err(|_| {
                photosync_core::Error::Protocol {
                    detail: "cannot build a server name".into(),
                }
            })?,
            tcp,
        )
        .await?;
    let peer_fp = observed.get().expect("server certificate");
    println!("peer fingerprint {}", peer_fp.short());

    let conn = db::open(db_path)?;
    let source = DirSource;
    let started = std::time::Instant::now();
    let totals = run_sender(
        &mut Link::new(tls, Timeouts::default()),
        &conn,
        &source,
        identity.fingerprint(),
        peer_fp,
        SenderAuth::Code(&code),
        &hostname(),
        true,
    )
    .await
    .map_err(|e| photosync_core::Error::Protocol {
        detail: format!("sender: {e}"),
    })?;
    let elapsed = started.elapsed();

    println!("\nsent     : {} committed", totals.items_done);
    println!("skipped  : {}", totals.items_skipped);
    println!("failed   : {}", totals.items_failed);
    println!("bytes    : {}", totals.bytes_done);
    if elapsed.as_secs_f64() > 0.0 {
        println!(
            "rate     : {:.1} MB/s over {:.1}s",
            totals.bytes_done as f64 / 1_048_576.0 / elapsed.as_secs_f64(),
            elapsed.as_secs_f64()
        );
    }
    Ok(())
}

/// Best-effort device name for the `Hello` and the announcement.
fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| format!("psdev-{}", std::env::consts::OS))
}

/// Default database beside the library, so two roles on one machine do not share
/// one file.
pub fn default_db(dir: &Path, role: &str) -> PathBuf {
    dir.join(format!(".photosync-{role}.db"))
}
