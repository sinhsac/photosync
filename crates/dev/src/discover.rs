//! Discovery end to end: a receiver announces, a sender finds it and syncs
//! without anyone typing an address.
//!
//! This is the last piece that stood between the engine and something usable
//! (`app_info.md` §7). Everything before it required both sides to already know
//! where the other was.

use crate::fsprovider::{DirSink, DirSource};
use photosync_core::pairing::{CodeSession, PairingCode};
use photosync_core::proto::PROTOCOL_MAJOR;
use photosync_core::{db, session};
use photosync_net::discovery::{self, Announcement, Candidate};
use photosync_net::identity::Identity;
use photosync_net::link::{Link, Timeouts};
use photosync_net::orchestrator::{run_receiver, run_sender, ReceiverAuth, SenderAuth};
use photosync_net::tls::{client_config, server_config, Pinning};
use photosync_net::tls_stream::{ServerName, TlsAcceptor, TlsConnector};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Reports what discovery can see on this machine, without transferring
/// anything.
pub fn probe() -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        println!("multicast group : {}", discovery::MULTICAST_GROUP);
        println!("port            : {}", discovery::DEFAULT_PORT);
        println!("ttl             : {}", discovery::MULTICAST_TTL);

        let real = discovery::local_ipv4(false);
        println!("\ninterfaces (excluding loopback), best first:");
        if real.is_empty() {
            println!("  none — discovery would degrade to a typed address (§7.5)");
        }
        for ip in &real {
            println!("  {ip}");
        }
        println!(
            "\nsubnet scan would cover {} interface(s), {} probes each, {} in flight, {:?} per probe",
            real.len().min(discovery::MAX_INTERFACES),
            255,
            discovery::SCAN_CONCURRENCY,
            discovery::PROBE_TIMEOUT
        );
    });
    Ok(())
}

/// A receiver announces itself and waits; a sender discovers it and syncs.
///
/// Runs both roles in one process on the loopback interface, so it exercises the
/// real sockets and the real announcement format. What it cannot prove here is
/// the case that matters most — two devices on a phone hotspot (§7.6, criterion
/// 5) — because that needs two devices.
pub fn run(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { drive(src_dir, dst_dir).await })
}

async fn drive(src_dir: &Path, dst_dir: &Path) -> photosync_core::Result<()> {
    let work = std::env::temp_dir().join("psdev-discover");
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(dst_dir);
    fs::create_dir_all(&work)?;
    let sender_db = work.join("sender.db");
    let receiver_db = work.join("receiver.db");

    let scanned = crate::sync::catalogue_pub(&sender_db, src_dir)?;
    println!("catalogued      : {scanned} assets");

    let receiver_identity = Identity::generate().expect("identity");
    let sender_identity = Identity::generate().expect("identity");
    let receiver_fp = receiver_identity.fingerprint();
    let sender_fp = sender_identity.fingerprint();

    // A port of zero would be useless in an announcement, so bind the real one.
    // Consecutive harness runs collide on it while the previous listener drains,
    // which looks like a discovery failure rather than a scheduling one.
    if !wait_port_free(discovery::DEFAULT_PORT, Duration::from_secs(5)).await {
        return Err(photosync_core::Error::Protocol {
            detail: format!(
                "port {} is still in use; another PhotoSync is running or the last run has not \
                 released it",
                discovery::DEFAULT_PORT
            ),
        });
    }
    let listener = TcpListener::bind(("0.0.0.0", discovery::DEFAULT_PORT)).await?;
    let bound = listener.local_addr()?;
    println!("receiver listening on {bound}");

    let issued = CodeSession::issue(db::now_millis());
    let code = PairingCode::parse(issued.code().as_str()).expect("well formed");
    println!("code            : {}\n", issued.code().display_grouped());

    // --- receiver: announce, then serve one connection --------------------
    let announcement = Announcement {
        v: PROTOCOL_MAJOR,
        port: bound.port(),
        fp: receiver_fp,
        name: "psdev receiver".into(),
        platform: std::env::consts::OS.into(),
    };

    let announce_task = tokio::spawn(async move {
        // Loopback included so this works on one machine. On a device the real
        // interfaces are what matter, and loopback is excluded (§7.2).
        discovery::announce(&announcement, discovery::DEFAULT_PORT, true).await
    });

    let (receiver_cfg, receiver_observed) =
        server_config(&receiver_identity, Pinning::FirstContact).expect("server config");
    let receiver_db_path = receiver_db.clone();
    let dst = dst_dir.to_path_buf();
    let mut owned_code = CodeSession::with_code(code.clone(), db::now_millis());

    let receiver_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let (tcp, from) = listener.accept().await.map_err(|e| e.to_string())?;
            let tls = TlsAcceptor::from(receiver_cfg)
                .accept(tcp)
                .await
                .map_err(|e| format!("tls: {e}"))?;
            let peer_fp = receiver_observed.get().ok_or("no client certificate")?;

            let conn = db::open(&receiver_db_path).map_err(|e| e.to_string())?;
            let sink = DirSink::new(&dst).map_err(|e| e.to_string())?;
            session::sweep_stale_active(&conn).map_err(|e| e.to_string())?;

            let totals = run_receiver(
                &mut Link::new(tls, Timeouts::default()),
                &conn,
                &sink,
                receiver_fp,
                peer_fp,
                ReceiverAuth::Code(&mut owned_code),
                "psdev receiver",
                db::now_millis(),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok::<_, String>((totals, from))
        })
    });

    // --- sender: discover, then connect -----------------------------------
    let started = std::time::Instant::now();
    let candidates = discovery::discover_staged(discovery::DEFAULT_PORT, sender_fp, true)
        .await
        .map_err(|e| photosync_core::Error::Protocol {
            detail: format!("discovery: {e}"),
        })?;
    let elapsed = started.elapsed();

    println!("discovery took  : {elapsed:?}");
    println!("candidates      : {}", candidates.len());

    // One device announces on every interface it has, so the raw list contains
    // one entry per reachable path. Grouped by fingerprint it is one peer.
    let groups = discovery::group_by_peer(candidates.clone());
    println!("distinct peers  : {}", groups.len());
    for group in &groups {
        match group.first().and_then(|c| c.announced.as_ref()) {
            Some(a) => {
                println!(
                    "  {:?} on {} (fp {}) reachable at {} address(es):",
                    a.name,
                    a.platform,
                    a.fp.short(),
                    group.len()
                );
                for c in group {
                    println!("    {}", c.addr);
                }
            }
            None => println!(
                "  unidentified, found by subnet scan at {}",
                group
                    .first()
                    .map(|c| c.addr.to_string())
                    .unwrap_or_default()
            ),
        }
    }

    let Some(target) = pick(&candidates, receiver_fp) else {
        println!("\nno candidate matched — nothing to connect to");
        let _ = announce_task.await;
        return Ok(());
    };
    println!("\nconnecting to   : {}", target.addr);

    let (client_cfg, client_observed) =
        client_config(&sender_identity, Pinning::FirstContact).expect("client config");
    let tcp = TcpStream::connect(target.addr).await?;
    let tls = TlsConnector::from(client_cfg)
        .connect(ServerName::try_from("127.0.0.1").expect("name"), tcp)
        .await?;
    let peer_fp = client_observed.get().expect("server certificate");

    // The announcement is a hint. The fingerprint that counts is the one the
    // handshake presented (§9.1).
    println!(
        "announced fp    : {}",
        target
            .announced
            .as_ref()
            .map(|a| a.fp.short())
            .unwrap_or_else(|| "-".into())
    );
    println!("handshake fp    : {}", peer_fp.short());
    println!(
        "  match         : {}",
        match target.announced.as_ref().map(|a| a.fp == peer_fp) {
            Some(true) => "yes",
            Some(false) => "NO — the announcement was not from this peer",
            None => "n/a, no announcement",
        }
    );

    let conn = db::open(&sender_db)?;
    let source = DirSource;
    let sent = run_sender(
        &mut Link::new(tls, Timeouts::default()),
        &conn,
        &source,
        sender_fp,
        peer_fp,
        SenderAuth::Code(&code),
        "psdev sender",
        true,
    )
    .await
    .map_err(|e| photosync_core::Error::Protocol {
        detail: format!("sender: {e}"),
    })?;

    let received = receiver_thread
        .join()
        .expect("receiver thread")
        .map_err(|e| photosync_core::Error::Protocol {
            detail: format!("receiver: {e}"),
        })?;
    let interfaces = announce_task.await.unwrap_or(Ok(0)).unwrap_or(0);

    println!("\nannounced on {interfaces} interface(s)");
    println!("connection came from {}", received.1);
    println!(
        "transferred     : {} assets, {} bytes",
        sent.items_done, sent.bytes_done
    );

    let sink = DirSink::new(dst_dir)?;
    let arrived = sink.visible()?.len();
    println!(
        "receiver library: {arrived} of {scanned} {}",
        if arrived == scanned {
            "OK"
        } else {
            "INCOMPLETE"
        }
    );
    println!(
        "staging empty   : {}",
        if fs::read_dir(sink.staging_dir())?.count() == 0 {
            "yes OK"
        } else {
            "NO"
        }
    );
    Ok(())
}

/// Chooses which candidate to talk to.
///
/// In the product there is no fingerprint to match against yet — the user typed a
/// code, not a fingerprint — so the sender handshakes candidates in turn and lets
/// authentication decide (§7.2). Here the expected fingerprint is known, so it is
/// used to keep the harness deterministic when other things are on the network.
fn pick(candidates: &[Candidate], expected: photosync_core::model::Hash32) -> Option<&Candidate> {
    candidates
        .iter()
        .find(|c| c.announced.as_ref().is_some_and(|a| a.fp == expected))
        .or_else(|| candidates.first())
}

/// Waits for a port to become free, so consecutive harness runs do not collide
/// on the fixed discovery port.
async fn wait_port_free(port: u16, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if TcpListener::bind(("0.0.0.0", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
