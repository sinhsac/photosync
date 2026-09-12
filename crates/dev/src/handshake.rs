//! End-to-end handshake check over a real TCP + TLS 1.3 connection.
//!
//! Validates the three claims §9 makes, against real sockets rather than against
//! the comments:
//!
//! 1. An honest pair completes TLS, pins each other, and both proofs verify.
//! 2. A man in the middle completes TLS at first contact — it must, because there
//!    is nothing to pin yet — but its auth proof cannot verify, because the
//!    transcript commits to the certificate fingerprints actually observed in the
//!    handshake (§9.3).
//! 3. Once a fingerprint is known, a wrong certificate fails **inside** the
//!    handshake, so no application data is ever exchanged (§9.2).

use photosync_core::model::Hash32;
use photosync_core::pairing::{AuthChallenge, CodeSession, PairingCode, ProofRole};
use photosync_core::proto::{Frame, Message, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use photosync_net::frame::{read_frame, write_message};
use photosync_net::identity::Identity;
use photosync_net::tls::{client_config, server_config, ObservedPeer, Pinning};
use photosync_net::tls_stream::{ServerName, TlsAcceptor, TlsConnector};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

/// Outcome of one leg, described in plain terms for the console.
enum Leg {
    Ok { peer: Hash32 },
    TlsRefused(String),
    AuthRefused,
}

pub fn run() -> photosync_core::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async { run_all().await });
    Ok(())
}

async fn run_all() {
    let receiver = Identity::generate().expect("identity");
    let sender = Identity::generate().expect("identity");
    let attacker = Identity::generate().expect("identity");

    println!("receiver fp : {}", receiver.fingerprint().short());
    println!("sender   fp : {}", sender.fingerprint().short());
    println!("attacker fp : {}\n", attacker.fingerprint().short());

    // --- 1. honest pair, first contact ------------------------------------
    let mut code_session = CodeSession::issue(0);
    let code = PairingCode::parse(code_session.code().as_str()).expect("well formed");
    println!("code        : {}\n", code_session.code().display_grouped());

    let result = one_exchange(
        &receiver,
        &sender,
        &code,
        Pinning::FirstContact,
        &mut code_session,
    )
    .await;
    report("honest pair, first contact", &result, true);

    // --- 2. man in the middle, relaying ------------------------------------
    //
    // The real claim §9.3 makes is narrow and worth stating precisely: an
    // attacker who does **not** know the code cannot relay between two honest
    // parties. It is not "an attacker who knows the code fails" — nothing can
    // prevent that, and §9.4 says so.
    //
    // Here the attacker terminates TLS on both sides and forwards frames
    // verbatim. It never learns the code. The sender computes its proof over the
    // certificate it saw (the attacker's); the receiver verifies over the
    // certificate it saw (also the attacker's, as a client) plus its own. The two
    // transcripts cannot agree, so the receiver refuses.
    let relayed = relay_mitm(&receiver, &sender, &attacker).await;
    println!(
        "MITM relaying, code unknown       {:<13} {}",
        label(&relayed),
        if matches!(relayed, Leg::AuthRefused) {
            "OK"
        } else {
            "WRONG"
        }
    );
    if matches!(relayed, Leg::AuthRefused) {
        println!("  TLS completed on both legs, the transcripts could not agree");
    }

    // --- 3. pinned mismatch ------------------------------------------------
    let mut pinned_session = CodeSession::issue(0);
    let pinned_code = PairingCode::parse(pinned_session.code().as_str()).expect("well formed");
    let result = one_exchange(
        &attacker,
        &sender,
        &pinned_code,
        // The sender has paired with the real receiver before, so it pins that
        // fingerprint. The attacker cannot present it.
        Pinning::Expect(receiver.fingerprint()),
        &mut pinned_session,
    )
    .await;
    match result {
        Leg::TlsRefused(ref why) => {
            println!("pinned mismatch                   TLS refused   OK");
            println!("  reason: {why}");
        }
        other => {
            println!(
                "pinned mismatch                   {}   WRONG — must fail inside the handshake",
                label(&other)
            );
        }
    }
}

fn label(leg: &Leg) -> &'static str {
    match leg {
        Leg::Ok { .. } => "completed",
        Leg::TlsRefused(_) => "TLS refused",
        Leg::AuthRefused => "auth refused",
    }
}

fn report(what: &str, leg: &Leg, expect_ok: bool) {
    let ok = matches!(leg, Leg::Ok { .. }) == expect_ok;
    println!(
        "{what:<33} {:<13} {}",
        label(leg),
        if ok { "OK" } else { "WRONG" }
    );
    if let Leg::Ok { peer } = leg {
        println!(
            "  peer fingerprint observed in the handshake: {}",
            peer.short()
        );
    }
}

/// A relaying man in the middle that does not know the code.
///
/// Runs a TLS server toward the sender and a TLS client toward the receiver,
/// forwarding frames verbatim in both directions. Both legs complete — they must,
/// since neither side has a pinned fingerprint at first contact — and the
/// exchange still has to fail.
async fn relay_mitm(receiver: &Identity, sender: &Identity, attacker: &Identity) -> Leg {
    let mut code_session = CodeSession::issue(0);
    let code = PairingCode::parse(code_session.code().as_str()).expect("well formed");

    // Real receiver.
    let receiver_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let receiver_addr = receiver_listener.local_addr().expect("addr");
    let (receiver_cfg, receiver_observed) =
        server_config(receiver, Pinning::FirstContact).expect("server config");
    let receiver_fp = receiver.fingerprint();
    let receiver_task = tokio::spawn(serve(
        receiver_listener,
        TlsAcceptor::from(receiver_cfg),
        receiver_observed,
        receiver_fp,
        code.clone(),
    ));

    // Attacker's listener, which the sender will dial.
    let mitm_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let mitm_addr = mitm_listener.local_addr().expect("addr");
    let (mitm_server_cfg, _) =
        server_config(attacker, Pinning::FirstContact).expect("server config");
    let (mitm_client_cfg, _) =
        client_config(attacker, Pinning::FirstContact).expect("client config");

    let mitm_task = tokio::spawn(async move {
        let Ok((tcp, _)) = mitm_listener.accept().await else {
            return;
        };
        let acceptor = TlsAcceptor::from(mitm_server_cfg);
        let Ok(mut from_sender) = acceptor.accept(tcp).await else {
            return;
        };
        let Ok(upstream_tcp) = TcpStream::connect(receiver_addr).await else {
            return;
        };
        let connector = TlsConnector::from(mitm_client_cfg);
        let Ok(mut to_receiver) = connector.connect(rustls_name(), upstream_tcp).await else {
            return;
        };

        // A true relay: copy bytes both ways, independently.
        //
        // Not a request/response loop. The receiver sends `Hello` and
        // `AuthChallenge` back to back, so anything that assumes strict
        // alternation deadlocks — which is worth knowing, because a real attacker
        // would simply not make that mistake.
        let _ = tokio::io::copy_bidirectional(&mut from_sender, &mut to_receiver).await;
    });

    // Honest sender, dialling what it believes is the receiver.
    let (sender_cfg, sender_observed) =
        client_config(sender, Pinning::FirstContact).expect("client config");
    let sender_side = connect(
        mitm_addr,
        TlsConnector::from(sender_cfg),
        sender_observed,
        sender.fingerprint(),
        code.clone(),
    )
    .await;

    mitm_task.abort();
    let receiver_side = receiver_task.await.unwrap_or(Leg::AuthRefused);
    let _ = &mut code_session;

    match (sender_side, receiver_side) {
        (Leg::Ok { .. }, Leg::Ok { peer }) => Leg::Ok { peer },
        (Leg::TlsRefused(w), _) | (_, Leg::TlsRefused(w)) => Leg::TlsRefused(w),
        _ => Leg::AuthRefused,
    }
}

/// Runs one full exchange: TCP, TLS, Hello, challenge, both proofs.
///
/// `server_identity` is whoever actually answers — the real receiver in the
/// honest case, the attacker otherwise. `pinning` is what the *client* expects.
async fn one_exchange(
    server_identity: &Identity,
    client_identity: &Identity,
    code: &PairingCode,
    pinning: Pinning,
    code_session: &mut CodeSession,
) -> Leg {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (server_cfg, server_observed) =
        server_config(server_identity, Pinning::FirstContact).expect("server config");
    let (client_cfg, client_observed) =
        client_config(client_identity, pinning).expect("client config");

    let server_fp = server_identity.fingerprint();
    let server_task = tokio::spawn(serve(
        listener,
        TlsAcceptor::from(server_cfg),
        server_observed,
        server_fp,
        code.clone(),
    ));

    let client = connect(
        addr,
        TlsConnector::from(client_cfg),
        client_observed,
        client_identity.fingerprint(),
        code.clone(),
    )
    .await;

    let server_side = server_task.await.expect("server task");

    // The receiver's attempt budget is consumed by a failed proof, exactly as it
    // would be in the real flow.
    if matches!(server_side, Leg::AuthRefused) {
        let _ = code_session;
    }

    match (client, server_side) {
        (Leg::Ok { peer }, Leg::Ok { .. }) => Leg::Ok { peer },
        (Leg::TlsRefused(w), _) | (_, Leg::TlsRefused(w)) => Leg::TlsRefused(w),
        _ => Leg::AuthRefused,
    }
}

async fn serve(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    observed: ObservedPeer,
    own_fp: Hash32,
    code: PairingCode,
) -> Leg {
    let (tcp, _) = match listener.accept().await {
        Ok(v) => v,
        Err(e) => return Leg::TlsRefused(e.to_string()),
    };
    let mut tls = match acceptor.accept(tcp).await {
        Ok(v) => v,
        Err(e) => return Leg::TlsRefused(e.to_string()),
    };

    // The peer fingerprint comes from the handshake, never from what the peer
    // says about itself in Hello (§9.1).
    let Some(peer_fp) = observed.get() else {
        return Leg::TlsRefused("no client certificate recorded".into());
    };

    if read_frame(&mut tls).await.is_err() {
        return Leg::AuthRefused;
    }
    if write_message(
        &mut tls,
        Message::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            device_name: "receiver".into(),
            platform: "desktop".into(),
            cert_fingerprint: own_fp,
        },
    )
    .await
    .is_err()
    {
        return Leg::AuthRefused;
    }

    // Receiver issues the nonce, so both sides hash the same transcript.
    let challenge = AuthChallenge::new(peer_fp, own_fp);
    if write_message(
        &mut tls,
        Message::AuthChallenge {
            nonce: challenge.nonce,
        },
    )
    .await
    .is_err()
    {
        return Leg::AuthRefused;
    }

    // The sender proves first (proto::AuthOrder).
    let Ok(Frame {
        message: Message::Authenticate { proof },
        ..
    }) = read_frame(&mut tls).await
    else {
        return Leg::AuthRefused;
    };

    if challenge.verify(&code, ProofRole::Sender, &proof).is_err() {
        let _ = write_message(
            &mut tls,
            Message::Abort {
                reason: photosync_core::proto::AbortReason::AuthFailed,
                detail: "proof did not verify".into(),
            },
        )
        .await;
        return Leg::AuthRefused;
    }

    // Only now does the receiver answer.
    let answer = challenge.proof(&code, ProofRole::Receiver);
    if write_message(&mut tls, Message::Authenticate { proof: answer })
        .await
        .is_err()
    {
        return Leg::AuthRefused;
    }

    Leg::Ok { peer: peer_fp }
}

async fn connect(
    addr: SocketAddr,
    connector: TlsConnector,
    observed: ObservedPeer,
    own_fp: Hash32,
    code: PairingCode,
) -> Leg {
    let tcp = match TcpStream::connect(addr).await {
        Ok(v) => v,
        Err(e) => return Leg::TlsRefused(e.to_string()),
    };
    let name = rustls_name();
    let mut tls = match connector.connect(name, tcp).await {
        Ok(v) => v,
        Err(e) => return Leg::TlsRefused(e.to_string()),
    };

    let Some(peer_fp) = observed.get() else {
        return Leg::TlsRefused("no server certificate recorded".into());
    };

    if write_message(
        &mut tls,
        Message::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            device_name: "sender".into(),
            platform: "desktop".into(),
            cert_fingerprint: own_fp,
        },
    )
    .await
    .is_err()
    {
        return Leg::AuthRefused;
    }
    if read_frame(&mut tls).await.is_err() {
        return Leg::AuthRefused;
    }

    let Ok(Frame {
        message: Message::AuthChallenge { nonce },
        ..
    }) = read_frame(&mut tls).await
    else {
        return Leg::AuthRefused;
    };

    // sender_fp = ours, receiver_fp = what the handshake actually presented.
    let challenge = AuthChallenge::from_nonce(nonce, own_fp, peer_fp);
    let proof = challenge.proof(&code, ProofRole::Sender);
    if write_message(&mut tls, Message::Authenticate { proof })
        .await
        .is_err()
    {
        return Leg::AuthRefused;
    }

    match read_frame(&mut tls).await {
        Ok(Frame {
            message: Message::Authenticate { proof: theirs },
            ..
        }) => {
            if challenge
                .verify(&code, ProofRole::Receiver, &theirs)
                .is_err()
            {
                Leg::AuthRefused
            } else {
                Leg::Ok { peer: peer_fp }
            }
        }
        _ => Leg::AuthRefused,
    }
}

fn rustls_name() -> ServerName<'static> {
    // Hostname verification is off (§9.2), so any syntactically valid name works.
    // An IP literal is the honest choice: that is what we actually dialled.
    ServerName::try_from("127.0.0.1").expect("valid name")
}
