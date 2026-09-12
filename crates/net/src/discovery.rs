//! Device discovery (`app_info.md` §7).
//!
//! **Announce over UDP multicast, answer over TCP.** The receiver announces and
//! never replies over UDP; the sender hears it and opens the control connection
//! to the address and port the announcement carried. Two consequences:
//!
//! * Everything that reaches the candidate list has completed a TCP connection,
//!   so it is known reachable.
//! * The 6-digit code is **not** in the announcement. Revision 1 put it in an
//!   mDNS TXT record, which broadcasts a 20-bit secret to every device on the
//!   LAN including the attacker it exists to stop (§26.2). Candidate selection is
//!   therefore done by trying to authenticate, not by matching the code.
//!
//! This is modelled on LocalSend, which deliberately does not use mDNS. §26.1
//! records why that was chosen over a DNS-SD responder.

use photosync_core::model::Hash32;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;

/// IPv4 multicast group.
///
/// **Must stay inside `224.0.0.0/24`.** On some Android devices that is the only
/// range which reliably receives UDP multicast — a constraint LocalSend
/// documents in source after presumably paying for it. Non-negotiable.
///
/// Different from LocalSend's `224.0.0.167` so the two apps never answer each
/// other's announcements on a shared LAN.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 187);

/// One port for both UDP discovery and the TCP control stream (§7.1).
pub const DEFAULT_PORT: u16 = 53411;

/// Local subnet only.
pub const MULTICAST_TTL: u32 = 1;

/// Delays before each datagram of an announcement burst (§7.1).
///
/// A single datagram is easily lost, and a device that just joined the network
/// may not be listening yet. Three copies cost nothing and take ~2.6s in total.
pub const ANNOUNCE_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_millis(2000),
];

/// Per-probe timeout during a subnet scan (§7.4). LAN peers answer fast or not
/// at all.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Probes in flight during a subnet scan.
pub const SCAN_CONCURRENCY: usize = 50;

/// How many interfaces to scan before asking the user to choose (§7.4).
pub const MAX_INTERFACES: usize = 3;

/// How often a waiting receiver repeats its announcement burst.
///
/// One burst is not enough: the user reads a code off one device and walks to the
/// other, so the sender starts listening long after the receiver started
/// announcing.
pub const ANNOUNCE_PERIOD: Duration = Duration::from_secs(2);

/// Grace period before escalating from announce to subnet scan (§7.3).
///
/// **Must exceed [`ANNOUNCE_PERIOD`], with margin.** An earlier value of one
/// second was shorter than the announce period, so a sender could easily start
/// listening just after a burst and hear nothing before escalating — on two real
/// devices that happened on the first attempt. Escalating unnecessarily is not
/// merely wasteful; see [`probe`] for why it used to be actively harmful.
pub const ESCALATION_GRACE: Duration = Duration::from_millis(2_500);

/// Largest announcement we will parse. Bounded because it arrives from the
/// network.
const MAX_DATAGRAM: usize = 2048;

/// What a receiver broadcasts about itself (§7.2).
///
/// Deliberately contains no secret. The fingerprint is public — it is the hash of
/// a certificate the peer presents to anyone who connects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    /// Protocol major, so a peer speaking a different version can be ignored
    /// before a connection is wasted on it.
    pub v: u16,
    /// TCP port to connect back to.
    pub port: u16,
    /// SHA-256 of the announcer's certificate DER.
    pub fp: Hash32,
    pub name: String,
    pub platform: String,
}

/// A candidate worth handshaking with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub addr: SocketAddr,
    /// Present when the candidate came from an announcement, absent when it came
    /// from a subnet scan. Either way authentication decides (§7.2).
    pub announced: Option<Announcement>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("no usable network interface")]
    NoInterface,
}

type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Interfaces
// ---------------------------------------------------------------------------

/// Local IPv4 addresses worth binding to, best first.
///
/// Loopback is excluded for real use but kept behind `include_loopback` so the
/// harness can exercise the path on one machine.
///
/// Addresses ending in `.1` are ranked last: those are usually the gateway, and
/// on a tethering phone the hotspot address is more interesting than the router.
pub fn local_ipv4(include_loopback: bool) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|ifaces| {
            ifaces
                .into_iter()
                .filter_map(|i| match i.addr.ip() {
                    IpAddr::V4(v4) => {
                        if v4.is_loopback() && !include_loopback {
                            None
                        } else {
                            Some(v4)
                        }
                    }
                    IpAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();

    out.sort();
    out.dedup();
    out.sort_by_key(|ip| u8::from(ip.octets()[3] == 1));
    out
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

/// Binds one multicast socket for a specific interface (§7.2).
///
/// Two of these options are subtle and both matter:
///
/// * **Bind the wildcard address, not the interface address.** Some platforms
///   match a datagram's destination against the bound address, and binding the
///   interface address makes multicast silently never arrive.
/// * **Pin `IP_MULTICAST_IF`.** This, plus one socket per interface, is the
///   entire reason the hotspot case works: the tethering interface is announced
///   on independently of any other, rather than whatever the routing table
///   prefers.
///
/// Loopback is left on and our own datagrams are filtered by fingerprint
/// instead — cheaper than turning it off and getting the semantics wrong per
/// platform.
fn bind_multicast(interface: Ipv4Addr, port: u16) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    socket.join_multicast_v4(&MULTICAST_GROUP, &interface)?;
    socket.set_multicast_if_v4(&interface)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(MULTICAST_TTL)?;

    Ok(UdpSocket::from_std(socket.into())?)
}

/// Binds a socket per interface, skipping any that fail.
///
/// A failing interface is logged, never fatal: one broken virtual adapter must
/// not disable discovery (§7.2).
fn bind_all(port: u16, include_loopback: bool) -> Vec<(Ipv4Addr, UdpSocket)> {
    local_ipv4(include_loopback)
        .into_iter()
        .filter_map(|ip| match bind_multicast(ip, port) {
            Ok(sock) => Some((ip, sock)),
            Err(e) => {
                tracing::debug!(interface = %ip, "skipping interface: {e}");
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Announcing (receiver side)
// ---------------------------------------------------------------------------

/// Broadcasts an announcement burst on every usable interface.
///
/// Returns how many interfaces it reached. Zero is not an error: the sender can
/// still find us by subnet scan or a typed address (§7.4, §7.5), and discovery
/// that cannot start must not prevent pairing.
pub async fn announce(
    announcement: &Announcement,
    port: u16,
    include_loopback: bool,
) -> Result<usize> {
    let sockets = bind_all(port, include_loopback);
    if sockets.is_empty() {
        tracing::warn!("no interface accepted a multicast socket; discovery degraded to scan only");
        return Ok(0);
    }

    let payload = serde_json::to_vec(announcement).expect("announcement is serialisable");
    let target = SocketAddr::from(SocketAddrV4::new(MULTICAST_GROUP, port));

    for delay in ANNOUNCE_DELAYS {
        tokio::time::sleep(delay).await;
        for (ip, sock) in &sockets {
            if let Err(e) = sock.send_to(&payload, target).await {
                tracing::debug!(interface = %ip, "announce failed: {e}");
            }
        }
    }
    Ok(sockets.len())
}

// ---------------------------------------------------------------------------
// Listening (sender side)
// ---------------------------------------------------------------------------

/// Listens for announcements until `budget` elapses.
///
/// `own_fingerprint` filters our own datagrams, which arrive because multicast
/// loopback is on.
///
/// Malformed datagrams are skipped rather than fatal: anything at all can be
/// sent to a multicast group, and one bad packet from an unrelated app must not
/// stop discovery.
pub async fn listen(
    port: u16,
    own_fingerprint: Hash32,
    budget: Duration,
    include_loopback: bool,
) -> Result<Vec<Candidate>> {
    let sockets = bind_all(port, include_loopback);
    if sockets.is_empty() {
        return Err(Error::NoInterface);
    }

    // One reader task per socket, all feeding one channel. Racing the sockets by
    // hand would mean either polling them in turn — which starves every socket
    // after the first — or pulling in a combinator library for one call site.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr)>(64);
    let mut readers = Vec::with_capacity(sockets.len());

    for (ip, sock) in sockets {
        let tx = tx.clone();
        readers.push(tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            loop {
                match sock.recv_from(&mut buf).await {
                    Ok((len, from)) => {
                        if tx.send((buf[..len].to_vec(), from)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(interface = %ip, "receive failed: {e}");
                        break;
                    }
                }
            }
        }));
    }
    drop(tx);

    let mut found: Vec<Candidate> = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;

    while let Ok(Some((bytes, from))) = tokio::time::timeout_at(deadline, rx.recv()).await {
        // Anything at all can be sent to a multicast group. A datagram we cannot
        // parse is another app's, not an error.
        let Ok(ann) = serde_json::from_slice::<Announcement>(&bytes) else {
            continue;
        };

        // Our own datagram, arriving because loopback is on.
        if ann.fp == own_fingerprint {
            continue;
        }
        if ann.v != photosync_core::proto::PROTOCOL_MAJOR {
            tracing::debug!(peer_version = ann.v, "ignoring a peer on another protocol");
            continue;
        }

        let addr = SocketAddr::new(from.ip(), ann.port);
        if found.iter().any(|c| c.addr == addr) {
            continue;
        }
        tracing::info!(%addr, peer = %ann.name, "discovered");
        found.push(Candidate {
            addr,
            announced: Some(ann),
        });
    }

    for reader in readers {
        reader.abort();
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// Subnet scan (fallback)
// ---------------------------------------------------------------------------

/// Probes one address by opening a TCP connection and closing it.
///
/// # Why this is not harmless
///
/// A probe is indistinguishable, at the socket level, from a real peer that
/// connects and then vanishes before TLS. A receiver that treats any failed
/// connection as a failed session and gives up is therefore **taken down by its
/// own peer's discovery scan**. That is not hypothetical: it is what happened the
/// first time this ran between two real devices, and the sender then found the
/// receiver's port closed.
///
/// The scan cannot avoid connecting — that is the only way to find a listener
/// without multicast. So the obligation sits on the receiver: it must survive
/// connections that go nowhere and keep listening (§9.6). Both halves of that
/// contract are needed, and only one of them is in this file.
async fn probe(addr: SocketAddr) -> Option<SocketAddr> {
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Some(addr),
        _ => None,
    }
}

/// Probes every address in the /24 around `interface` (§7.4).
///
/// Anything that accepts is a candidate, and authentication decides which one is
/// right. No fingerprint is learned here; it comes from the TLS handshake that
/// follows.
///
/// Exists specifically for the hotspot case, where multicast is least reliable.
pub async fn scan_subnet(interface: Ipv4Addr, port: u16) -> Vec<Candidate> {
    let base = interface.octets();
    let mut found = Vec::new();
    let mut in_flight = Vec::new();

    for host in 0..=255u8 {
        let ip = Ipv4Addr::new(base[0], base[1], base[2], host);
        if ip == interface {
            continue;
        }
        let addr = SocketAddr::from(SocketAddrV4::new(ip, port));

        in_flight.push(tokio::spawn(probe(addr)));

        if in_flight.len() >= SCAN_CONCURRENCY {
            drain(&mut in_flight, &mut found).await;
        }
    }
    drain(&mut in_flight, &mut found).await;
    found
}

async fn drain(
    in_flight: &mut Vec<tokio::task::JoinHandle<Option<SocketAddr>>>,
    found: &mut Vec<Candidate>,
) {
    for handle in in_flight.drain(..) {
        if let Ok(Some(addr)) = handle.await {
            found.push(Candidate {
                addr,
                announced: None,
            });
        }
    }
}

/// Groups candidates by peer, since one device announces on every interface it
/// has and therefore appears once per reachable path.
///
/// Observed immediately on a normal laptop: a single receiver produced four
/// candidates (`192.168.1.16`, `172.30.64.1`, `192.168.56.1`, `127.0.0.1`), all
/// the same fingerprint. Presenting those as four devices would be nonsense, and
/// §19 has no room for a device list in the first place.
///
/// The extra addresses are not noise, though, so they are kept rather than
/// discarded: if the first path fails to connect, the next one is worth trying,
/// and on a phone that is exactly the difference between the Wi-Fi interface and
/// the hotspot interface.
///
/// Candidates from a subnet scan have no fingerprint yet and cannot be grouped;
/// each is returned on its own.
pub fn group_by_peer(candidates: Vec<Candidate>) -> Vec<Vec<Candidate>> {
    let mut groups: Vec<Vec<Candidate>> = Vec::new();
    for candidate in candidates {
        let fp = candidate.announced.as_ref().map(|a| a.fp);
        match fp.and_then(|fp| {
            groups
                .iter_mut()
                .find(|g| g.first().and_then(|c| c.announced.as_ref()).map(|a| a.fp) == Some(fp))
        }) {
            Some(group) => group.push(candidate),
            None => groups.push(vec![candidate]),
        }
    }
    groups
}

/// Staged discovery (§7.3): listen first, escalate to scanning only if nothing
/// answered.
///
/// Any announcement proves the cheap stage works on this network, so the
/// expensive stage is skipped entirely. On a normal Wi-Fi network that means one
/// burst and no scan; on a hotspot it means a scan after one second.
pub async fn discover_staged(
    port: u16,
    own_fingerprint: Hash32,
    include_loopback: bool,
) -> Result<Vec<Candidate>> {
    let announced = listen(port, own_fingerprint, ESCALATION_GRACE, include_loopback).await?;
    if !announced.is_empty() {
        return Ok(announced);
    }

    let interfaces: Vec<Ipv4Addr> = local_ipv4(include_loopback)
        .into_iter()
        .take(MAX_INTERFACES)
        .collect();
    if interfaces.is_empty() {
        return Err(Error::NoInterface);
    }

    tracing::info!(
        interfaces = interfaces.len(),
        "no announcement heard, falling back to subnet scan"
    );

    let mut found = Vec::new();
    for ip in interfaces {
        found.extend(scan_subnet(ip, port).await);
    }
    Ok(found)
}
