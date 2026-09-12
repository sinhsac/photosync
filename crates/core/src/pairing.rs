//! Pairing code and code-bound authentication (`app_info.md` §6, §9.3, §9.4).
//!
//! Pure computation: no sockets, no TLS. The certificate fingerprints arrive as
//! values, so this module can be reasoned about, and driven, without a network.
//!
//! What this achieves and what it does not is spelled out in [`AuthChallenge`]
//! and §9.4. It is deliberately not a PAKE, and the residual weakness of a short
//! code is real rather than hidden.

use crate::error::{Error, Result};
use crate::model::Hash32;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Digits in a pairing code (§6).
pub const CODE_DIGITS: u32 = 6;

/// Total failed proofs before the code is destroyed (§6, §9.4).
///
/// Counted globally for the session, never per source address, and never reset
/// by a success. LocalSend's limiter is per-IP and resets on success, which makes
/// a renewable four-attempt window and hands a free retry budget to anyone with
/// a second address (§26.3).
pub const MAX_ATTEMPTS: u32 = 3;

/// Lifetime of a displayed code (§6).
pub const CODE_TTL_MS: i64 = 5 * 60 * 1000;

/// A 6-digit pairing code.
///
/// Kept as a fixed-width string because that is how the user reads and types it,
/// and because leading zeros are significant.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// Generates a code from the OS CSPRNG.
    ///
    /// Rejection-samples so every value in `0..1_000_000` is equally likely; a
    /// plain modulo would bias the low end. The bias would be tiny, but the
    /// entire security argument rests on 20 bits being uniformly unguessable, so
    /// there is no reason to spend any of it.
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        const LIMIT: u32 = 1_000_000;
        // Largest multiple of LIMIT that fits in u32, used as the rejection bound.
        const BOUND: u32 = u32::MAX - (u32::MAX % LIMIT);
        loop {
            let v = rng.next_u32();
            if v < BOUND {
                return Self(format!("{:0width$}", v % LIMIT, width = CODE_DIGITS as usize));
            }
        }
    }

    /// Parses user input. Accepts only exactly [`CODE_DIGITS`] ASCII digits.
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        if trimmed.len() != CODE_DIGITS as usize {
            return None;
        }
        if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self(trimmed))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Grouped for display: `482 913` (§6).
    pub fn display_grouped(&self) -> String {
        let (a, b) = self.0.split_at(3);
        format!("{a} {b}")
    }
}

// Never let a code reach a log by accident.
impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(******)")
    }
}

/// Role label mixed into the transcript so a proof cannot be replayed back at
/// the peer that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofRole {
    Sender,
    Receiver,
}

impl ProofRole {
    const fn label(self) -> &'static [u8] {
        match self {
            ProofRole::Sender => b"photosync/v1/sender",
            ProofRole::Receiver => b"photosync/v1/receiver",
        }
    }
}

/// Everything both sides must agree on before either can prove anything.
///
/// # What binding the fingerprints buys
///
/// The transcript includes **both** certificate fingerprints (§9.3). A
/// man-in-the-middle necessarily terminates TLS with its own certificate, so its
/// transcript differs from the one the honest peers would compute, and its proof
/// cannot verify. This is what closes the first-contact gap that certificate
/// pinning alone cannot (§9.2): at first contact there is no pinned fingerprint
/// yet, so the code is the only thing standing in the way.
///
/// # What it does not buy
///
/// A 6-digit code is 20 bits. Anyone who obtains one valid proof together with
/// the transcript can recover the code by trying all million candidates offline,
/// which takes milliseconds. Ordering the exchange so the sender proves first
/// (see `proto::AuthOrder`) means an attacker cannot harvest a proof by merely
/// listening as a fake sender, but an attacker who successfully poses as a
/// *receiver* does obtain the real sender's proof and can recover the code.
///
/// The mitigations are procedural, not cryptographic, and they are the reason
/// this is acceptable for the job: the code is single-use, expires in five
/// minutes, dies after three failed proofs, and authenticates first contact
/// only — afterwards the pinned fingerprint pair is the credential (§9.5). A
/// recovered code is therefore worth little by the time it is recovered.
///
/// A PAKE such as SPAKE2 would remove even that. §9.3 consciously declines it;
/// if that trade is ever revisited, this is the type to replace.
#[derive(Clone, Debug)]
pub struct AuthChallenge {
    /// Fresh per session, from the CSPRNG. Salts the key derivation and makes
    /// every transcript unique, so a proof from one session is useless in
    /// another.
    pub nonce: [u8; 32],
    pub sender_fingerprint: Hash32,
    pub receiver_fingerprint: Hash32,
}

impl AuthChallenge {
    /// Builds a challenge with a fresh nonce. Called by the receiver.
    pub fn new(sender_fingerprint: Hash32, receiver_fingerprint: Hash32) -> Self {
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        Self {
            nonce,
            sender_fingerprint,
            receiver_fingerprint,
        }
    }

    /// Recreates a challenge from a received nonce. Called by the sender.
    pub const fn from_nonce(
        nonce: [u8; 32],
        sender_fingerprint: Hash32,
        receiver_fingerprint: Hash32,
    ) -> Self {
        Self {
            nonce,
            sender_fingerprint,
            receiver_fingerprint,
        }
    }

    /// `HMAC-SHA256(KDF(code, nonce), transcript ‖ role_label)`.
    ///
    /// The transcript is length-prefixed field by field so no two different field
    /// combinations can produce the same byte string. Concatenating fixed-width
    /// values would be safe here, but the habit is what keeps it safe when a
    /// field of variable length is added later.
    pub fn proof(&self, code: &PairingCode, role: ProofRole) -> Hash32 {
        let key = derive_key(code, &self.nonce);

        let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
        mac.update(&crate::proto::PROTOCOL_MAJOR.to_be_bytes());
        mac.update(self.sender_fingerprint.as_bytes());
        mac.update(self.receiver_fingerprint.as_bytes());
        mac.update(&self.nonce);
        mac.update(role.label());

        let out = mac.finalize().into_bytes();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&out);
        Hash32::from_bytes(bytes)
    }

    /// Verifies a peer's proof in constant time.
    ///
    /// Returns [`Error::AuthFailed`] with no detail: explaining *why* a proof was
    /// rejected would leak information about the code.
    pub fn verify(&self, code: &PairingCode, role: ProofRole, presented: &Hash32) -> Result<()> {
        let key = derive_key(code, &self.nonce);

        let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
        mac.update(&crate::proto::PROTOCOL_MAJOR.to_be_bytes());
        mac.update(self.sender_fingerprint.as_bytes());
        mac.update(self.receiver_fingerprint.as_bytes());
        mac.update(&self.nonce);
        mac.update(role.label());

        // `verify_slice` is constant-time. A plain `==` here would be a timing
        // oracle over a 20-bit secret.
        mac.verify_slice(presented.as_bytes())
            .map_err(|_| Error::AuthFailed)
    }
}

/// Derives the HMAC key from the code.
///
/// A deliberately cheap KDF. Stretching would be pointless: the code is
/// single-use and only 20 bits, so an attacker holding a transcript wins in
/// milliseconds either way, and slowing it down would only tax the honest phone.
/// The defence is the attempt budget and the single-use rule, not work factor
/// (§9.4).
fn derive_key(code: &PairingCode, nonce: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(nonce).expect("HMAC accepts any key length");
    mac.update(b"photosync/v1/code");
    mac.update(code.as_str().as_bytes());
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

// ---------------------------------------------------------------------------
// Attempt budget
// ---------------------------------------------------------------------------

/// A displayed code together with its expiry and remaining attempts (§6, §9.4).
///
/// Lives on the receiver, in memory only. Nothing here is persisted: a code must
/// not survive a restart, and an attempt counter that survives is a counter an
/// attacker can reset by crashing the app.
#[derive(Debug)]
pub struct CodeSession {
    code: PairingCode,
    issued_at_ms: i64,
    attempts_left: u32,
}

/// Why a code can no longer be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeRejection {
    Expired,
    TooManyAttempts,
    WrongProof { attempts_left: u32 },
}

impl CodeSession {
    /// Issues a fresh code.
    pub fn issue(now_ms: i64) -> Self {
        Self {
            code: PairingCode::generate(),
            issued_at_ms: now_ms,
            attempts_left: MAX_ATTEMPTS,
        }
    }

    /// Rebuilds a session around a code that is already on screen.
    ///
    /// For a receiver whose UI issued the code and is handing it down to the
    /// transport layer when a connection arrives. It does **not** reset the
    /// budget of an existing session: constructing a second session for the same
    /// code would hand out a fresh three attempts, which is exactly the renewable
    /// window §9.4 exists to prevent. Call this once per displayed code.
    pub fn with_code(code: PairingCode, now_ms: i64) -> Self {
        Self {
            code,
            issued_at_ms: now_ms,
            attempts_left: MAX_ATTEMPTS,
        }
    }

    /// The code, for display only (§6).
    pub const fn code(&self) -> &PairingCode {
        &self.code
    }

    pub const fn attempts_left(&self) -> u32 {
        self.attempts_left
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.issued_at_ms + CODE_TTL_MS
    }

    pub const fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms()
    }

    /// Whether the code is still usable at all.
    pub const fn is_live(&self, now_ms: i64) -> bool {
        self.attempts_left > 0 && !self.is_expired(now_ms)
    }

    /// Verifies a presented proof, consuming one attempt on failure.
    ///
    /// A success does **not** restore the budget, and the budget is not per peer.
    /// Both choices are deliberate (see [`MAX_ATTEMPTS`]).
    ///
    /// On the third failure the code is spent: [`is_live`](Self::is_live) turns
    /// false and the caller must issue a new one (§6).
    pub fn verify(
        &mut self,
        challenge: &AuthChallenge,
        role: ProofRole,
        presented: &Hash32,
        now_ms: i64,
    ) -> std::result::Result<(), CodeRejection> {
        if self.is_expired(now_ms) {
            return Err(CodeRejection::Expired);
        }
        if self.attempts_left == 0 {
            return Err(CodeRejection::TooManyAttempts);
        }

        match challenge.verify(&self.code, role, presented) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.attempts_left -= 1;
                if self.attempts_left == 0 {
                    Err(CodeRejection::TooManyAttempts)
                } else {
                    Err(CodeRejection::WrongProof {
                        attempts_left: self.attempts_left,
                    })
                }
            }
        }
    }
}
