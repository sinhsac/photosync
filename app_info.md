# PhotoSync — Product & Technical Specification

> Single source of truth for what PhotoSync is and how it is built.
> Revision 2. This revision incorporates findings from a code-level survey of LocalSend and Immich (see Appendix A). Three assumptions in revision 1 were wrong and have been corrected: LocalSend does not use mDNS, Immich has no chunked or resumable upload, and Immich has no cheap/partial hash.
> Sections marked **Open** (§27) are unresolved. Everything else is settled and may be implemented as written. §26 records why each contested decision went the way it did, including the alternatives that were rejected.

---

## 1. Product in one sentence

PhotoSync copies an entire phone photo/video library to another phone over the local network — no cloud, no server, no account — and saves the received media into the destination phone's native Photos/Gallery.

Pairing is as easy as LocalSend (a numeric code). The transfer is as complete as an Immich backup (the whole library, incrementally, resumable, verified).

---

## 2. What we borrow, and what we do not

Revision 1 claimed more prior art than actually exists. Corrected:

**Genuinely reusable from LocalSend** (Apache-2.0, mechanisms verified in source):

- Direct device-to-device over LAN, no account, no server
- UDP multicast announcement plus a callback-over-TCP handshake — *not* mDNS (§7)
- Per-interface socket binding, which is what makes the hotspot case work
- Subnet-scan fallback with concrete, field-tested concurrency and timeout values
- Certificate fingerprint as device identity, pinned during the TLS handshake
- Explicit Send / Receive roles, extremely simple UI

**Genuinely reusable from Immich** (AGPL-3.0, mechanisms verified in source):

- Whole-library sync instead of manual file picking
- Content-based asset identity, not filename, not device asset id
- Single-pass streaming hash: digest fed from the same buffer being written to disk
- Staging file then atomic commit, so a partial file never becomes a gallery entry
- "Which of these do you already have" batch query, and treating a duplicate as success rather than as an error
- Deriving the work queue from a SQL query instead of materialising it
- Platform change tokens for cheap library rescan
- Persistent per-asset hash cache, so a file is hashed once ever

**Claimed in revision 1 but not actually present in either project:**

| Belief | Reality |
|---|---|
| LocalSend discovers peers via mDNS/Bonjour | No mDNS, DNS-SD, NsdManager or service type anywhere. Hand-rolled UDP multicast + HTTP callback. |
| Immich does resumable chunked transfer | No chunking, no resume, no TUS. One whole-file multipart POST; a failure re-sends from byte 0. |
| Immich uses a cheap partial hash | Full-file SHA-1 on every asset. The only partial-ish variant hashes the *path* and is deprecated. |
| Immich verifies with SHA-256 | SHA-1. |

So chunking, byte-offset resume, and the quick-key identity are PhotoSync's own design. There is no reference implementation to lean on, and those are exactly the parts §24 says decide whether the product is real.

PhotoSync is **not** a general file-sharing app, and **not** a photo management platform. It is a one-way library copier with a LocalSend-grade UX.

---

## 3. Explicit non-goals

Not in this product at all:

- Cloud storage, accounts, login, central server
- PostgreSQL, Redis, any backend service
- Photo gallery / timeline / album management UI
- AI, face recognition, semantic search
- Video transcoding
- Social features, sharing links, comments

Not in MVP (see §23):

- Two-way sync
- Deletion propagation / tombstones
- Album and favorite preservation
- Background / automatic sync
- Desktop (Windows / Linux)

---

## 4. Platforms

MVP: **iOS ↔ Android**, in all four direction combinations (iOS→Android, Android→iOS, iOS→iOS, Android→Android).

Desktop is out of scope, but the sync engine and wire protocol must stay platform-independent so it remains possible later. No engine code may reference PhotoKit or MediaStore directly (see §17).

---

## 5. Roles: Send and Receive

There is no peer negotiation. The user picks the role.

```
Phone A                          Phone B
[ Send ] tab                     [ Receive ] tab
   │                                │
   │  enters the code B shows       │  shows a 6-digit code, waits
   └────────────► connect ──────────┘
   sender                           receiver
```

Rules:

- The **receiver** is the passive side. It displays a code and listens.
- The **sender** is the active side. It enters the code, then chooses what to send (default: everything).
- A device is never both at once. Switching tabs cancels any session in the other role.
- Only the sender's library is read. Only the receiver's library is written to.

To copy in both directions, the users swap roles and run it again. That is an accepted MVP limitation, not a bug.

---

## 6. Pairing with a numeric code

The receiver generates a **6-digit code**, displayed large on screen.

```
┌─────────────────────────────┐
│                             │
│        Ready to receive     │
│                             │
│          4 8 2  9 1 3       │
│                             │
│   Enter this code on the     │
│   sending phone.             │
│                             │
│   Saving to: Photos          │
│                             │
│         [ Cancel ]           │
│                             │
└─────────────────────────────┘
```

Code properties:

- 6 digits, generated from a cryptographically secure RNG, per session
- Single use. It authenticates **first contact only** and is discarded afterwards (§9.4)
- Expires 5 minutes after being displayed if no connection completes
- **3 failed attempts total, counted globally, never reset by a success.** On the third failure the code is destroyed and a new one generated
- Case-free and readable out loud, so it works across the room

The code serves two purposes at once: it identifies **which** device to connect to (§7) and it proves the connection is the intended one (§9).

Optional (not MVP-blocking): show a QR encoding the same code plus the address, so the sender can scan instead of type.

---

## 7. Discovery and network topology

The receiver, once listening, is findable in three ways. The sender escalates through them.

The mechanism is **announce over UDP multicast, answer over TCP** — modelled on LocalSend, which deliberately does not use mDNS. §26.1 records why mDNS was rejected.

### 7.1 Fixed network parameters

| Parameter | Value | Constraint |
|---|---|---|
| Multicast group (IPv4) | `224.0.0.187` | **Must stay inside `224.0.0.0/24`.** On some Android devices this is the only range that reliably receives UDP multicast. Non-negotiable. |
| Port (UDP and TCP) | `53411` | One port for both, as LocalSend does. Configurable; a non-default value warns the user that other devices may not find them. |
| Multicast TTL | `1` | Local subnet only. |
| Announce burst | `100ms, 500ms, 2000ms` | Three copies of the same datagram. A single one is easily lost, and a device that just joined may not be listening yet. Worst case ~2.6s per scan. |
| Probe timeout | `500ms` | Per TCP handshake attempt. LAN peers answer fast or not at all. |
| Scan concurrency | `50` in flight | Across 255 candidate addresses. |
| Interface cap | `3` | More than that, ask the user to pick one. |

Both the group and the port must differ from LocalSend's (`224.0.0.167` / `53317`) so the two apps never answer each other's announcements on a shared LAN.

### 7.2 Multicast announcement (primary)

The receiver announces itself; it does **not** answer over UDP. The reply travels back over TCP to the address and port carried in the announcement.

```
Receiver ──UDP multicast──►  "I am here: port 53411, fingerprint X, protocol 1"
Receiver ◄──TCP connect────   sender opens the control stream and handshakes
```

Announcement payload (compact JSON or CBOR, one datagram):

```
{ v: 1, port: 53411, fp: "<cert fingerprint>", name: "Ann's iPhone",
  platform: "ios", role: "receiver" }
```

The **6-digit code is never in the announcement.** Revision 1 put the session code in an mDNS TXT record; that leaks a low-entropy secret to every device on the LAN, including the attacker the code exists to stop. Instead the announcement carries only the certificate fingerprint, and code matching happens inside the authenticated handshake (§9.3). The consequence is that the sender cannot tell from the announcement alone which receiver matches the typed code — so it handshakes each candidate and lets authentication decide. With at most a handful of receivers on a LAN this costs nothing.

Socket setup, one socket **per interface address**, not one shared socket:

```
SO_REUSEADDR = 1                    (and SO_REUSEPORT on Unix)
bind to wildcard 0.0.0.0:53411      NOT the interface address
IP_ADD_MEMBERSHIP  group, interface
IP_MULTICAST_IF    interface        pin egress, do not let the routing table choose
IP_MULTICAST_LOOP  1                own datagrams filtered by fingerprint instead
IP_MULTICAST_TTL   1
```

Two of these are subtle and both were learned the hard way in LocalSend:

- **Bind the wildcard address, not the interface address.** Some platforms match a datagram's destination against the bound address, and binding the interface address makes multicast silently never arrive.
- **Pin `IP_MULTICAST_IF` per socket.** This, plus one socket per interface, is the entire reason the hotspot case works: the tethering interface is announced on independently of any other.

Leave loopback on and discard datagrams whose fingerprint equals our own. An interface that fails to bind is skipped and logged, never fatal — one broken virtual adapter must not disable discovery. Tolerate up to **10 consecutive receive errors** before declaring a socket dead; on Windows an ICMP error from a previous send surfaces on the next receive.

Discovery start must not be able to fail. If no socket binds at all, return a working handle that reports the error, and let manual address entry and subnet scan still function.

### 7.3 Staged escalation

```
t=0     announce + probe all known peers (favourites) concurrently
t=1s    if zero confirmations so far → subnet scan every local IPv4
```

Any confirmation, whether a new device or a previously known one, proves the cheap stage works on this network, so the expensive stage is skipped. Discovery events must be delivered non-blocking: a subnet scan can confirm dozens of devices in a burst, and blocking on a full event queue would stall the scan and leave its interface locked against future scans.

### 7.4 Subnet scan (fallback)

Derive the /24 from each local IPv4 address, probe all 255 addresses on the fixed port, 50 in flight, 500ms each. Any responder gets a handshake, and authentication decides which one is right. Guard against overlapping scans of the same interface, releasing the guard even if the scan is cancelled.

Candidate local addresses come from enumerating non-loopback interfaces. Deprioritise addresses ending in `.1` — those are usually the gateway. Cap at 3 interfaces; beyond that, ask the user which network to scan.

This fallback exists specifically for the hotspot case, where multicast is least reliable.

### 7.5 Manual address (last resort)

Hidden behind "Can't find it?". The user types the IP shown on the receiver screen. Same handshake, no discovery involved.

### 7.6 Hotspot mode

The required scenario "one phone shares Wi-Fi, the other joins" must work with no internet present.

```
Phone A (hotspot, 192.168.43.1)
        ▲
        │  Wi-Fi, no internet
        ▼
Phone B (client, 192.168.43.x)
```

Either phone may be the hotspot; either may be the sender.

The app must **never consult reachability or connectivity state to decide whether it may run.** LocalSend does not, and that is deliberate: the OS marking an interface as having no internet route is not the same as having no network, and treating it as such is the classic way this feature breaks. Bind to local interfaces and try.

> An iPhone cannot join its own Personal Hotspot, and iOS restricts what hotspot clients can do. The reliable configuration is **Android as hotspot, iPhone as client**, or both on a normal Wi-Fi network. Verify on real devices early (§24, §25 step 12).

Also outside our control: router AP isolation blocks all peer-to-peer traffic. Detect nothing, but say so in Help (§20).

---

## 8. Transport

- TCP. One control stream plus N data streams (start with N=1, make it configurable; 3–6 is the range field-tested by Immich's mobile uploader for phone-class hardware).
- Length-prefixed binary frames with a small JSON/CBOR header per message.
- The transport sits behind a `TransportProvider` interface. Wi-Fi Direct or USB can be added later without touching the sync engine.
- A heartbeat on the control stream, 10s interval, 30s timeout. This is what frees a receiver slot after an unclean disconnect (§9.6).

  **Not optional, and the reason is concrete.** A peer that stops sending without closing its socket leaves a half-open connection, and nothing at the TCP layer resolves that: the other side blocks on a read forever. This was reproduced accidentally while building `psdev resume-session` — the injected cut only made writes fail, the socket stayed open, and the receiver hung indefinitely until the stream was explicitly dropped. A phone going out of range, running out of battery, or being force-killed produces exactly that shape. Until the heartbeat is implemented, an interrupted session can hang rather than becoming resumable.
- On iOS, rebind all sockets when the app returns to the foreground. Sockets die silently on suspend and cannot be probed for liveness, so rebinding unconditionally is the only reliable option.

---

## 9. Security

The LAN is untrusted. A phone hotspot in a café is a hostile network.

### 9.1 Keys and identity

- **TLS 1.3**, mutual. Both sides hold a self-signed keypair generated on first launch.
- **ECDSA P-256** (Ed25519 acceptable if every target TLS stack supports it). LocalSend uses RSA-2048 purely for byte-compatibility with its own older Dart implementation; PhotoSync has no such legacy and should not inherit a slower, larger key.
- **Certificate stored in the OS keystore**: iOS Keychain with `kSecAttrAccessibleAfterFirstUnlock`, Android Keystore. Not in shared preferences. LocalSend keeps its private key as plaintext JSON in SharedPreferences, which means a device backup yields the identity — do not copy that.
- **Device identity = SHA-256 over the certificate DER, uppercase hex.** This is the only identity that matters. Do not use a self-reported id field.
- No SAN, no CA, no expiry rotation. Peers are addressed by IP, so hostname verification is meaningless and is deliberately disabled.

### 9.2 Fingerprint pinning

Certificate verification happens **inside the TLS handshake**, not after it:

- Verify the certificate is well-formed and self-consistent.
- If a fingerprint is expected, compare it and fail the handshake on mismatch.

Enforcing this during the handshake, rather than after the connection is up, means a wrong peer never receives even request metadata, let alone photo bytes. This is the single most valuable thing to copy from LocalSend.

During first contact the expected fingerprint is not yet known, so it cannot be pinned — that gap is closed by §9.3, not by pinning.

### 9.3 Code-bound authentication

After the TLS handshake and before anything else, both sides prove they know the same 6-digit code:

```
transcript = protocol_version ‖ sender_cert_fp ‖ receiver_cert_fp ‖ session_nonce
key        = KDF(code, salt = session_nonce)
proof      = HMAC-SHA256(key, transcript ‖ role_label)
```

Each side sends its own proof and verifies the other's, using a **constant-time comparison**. Role labels differ per direction so a proof cannot be replayed back at its sender.

The transcript binds the code to **both** certificate fingerprints. A man-in-the-middle terminating TLS presents its own certificate, so its transcript differs and its proof cannot verify. The code itself never appears on the wire in any form.

This is deliberately simpler than a full PAKE and is adequate for a short-lived, single-use LAN pairing. Two things it must not degrade into:

- **Never disable certificate verification.** "TLS with verification off" provides no protection whatsoever and is the exact failure mode this section exists to prevent.
- **Never send the code as a request parameter.** LocalSend transmits its PIN as a plaintext `?pin=` query parameter, unhashed and unbound to the certificate. It survives in logs, it grants nothing durable, and it stops no attacker who can complete a TLS handshake.

### 9.4 Why 6 digits is enough here

20 bits of entropy is small, so it is protected procedurally rather than cryptographically:

- Single use, for first contact only
- 5-minute expiry
- **3 attempts total, global, never reset by a success**
- Code destroyed and regenerated after the third failure

LocalSend's attempt limiter is the anti-pattern to avoid on every count: counted per source IP in an evictable cache, reset to zero on a correct entry, no time component, not persisted, and compared with plain string equality. Each of those is separately exploitable. Ours is one counter, in the session, decremented for any failed proof, and the session dies at zero.

**State the property precisely, because it is easy to overclaim.** What §9.3 achieves is:

> An attacker who does not know the code cannot relay a pairing between two honest devices.

It does **not** achieve, and nothing at this design point could:

> An attacker who knows the code cannot impersonate a receiver.

At first contact the code is the only authenticator, so anyone holding it is by definition indistinguishable from the intended peer. That is why the code is single-use, short-lived, spoken aloud rather than transmitted, and replaced by the pinned fingerprint pair immediately afterwards (§9.5).

The distinction matters because a short code can be recovered offline from one captured proof (20 bits, milliseconds). Ordering the exchange so the sender proves first means an attacker cannot harvest a proof by merely posing as a sender; an attacker who successfully poses as a receiver does obtain one. By the time the code is recovered it is already spent. A PAKE would close even this; §9.3 declines it deliberately.

Both statements above are exercised against real sockets by `psdev handshake` (§22.3).

### 9.5 After pairing: the fingerprint pair is the credential

Once a pairing succeeds, both sides persist the peer's certificate fingerprint (`peer` table, §14).

**Resume and repeat syncs require no code.** Reconnection is mutual TLS with both fingerprints pinned, plus the session id. This is what makes "Wi-Fi dropped, reconnect, resume" (§24 criterion 3) work without asking the user to re-pair, and it is why the code can be strictly single-use.

A consequence worth stating plainly: a paired device can reconnect and send again without further confirmation. That is the intended trade — it is the same trust model as a paired Bluetooth device. Settings must offer "Forget this device".

### 9.6 Sessions

- One active session per receiver. A second connection attempt is refused with a clear reason, not a generic error.
- **Pending sessions have a 60s TTL.** A connection that authenticates and then never begins transferring releases the slot.
- **An unclean disconnect frees the slot immediately** (TCP close, or heartbeat timeout at 30s). LocalSend has no session expiry at all and a single stuck sender blocks the receiver until the app intervenes; that is unacceptable for multi-hour library transfers.
- Session *state* outlives the slot. It stays in SQLite for resume (§14) and is reclaimable only by a peer whose pinned fingerprint matches.
- Per-asset transfer tokens are bound to `(session_id, peer fingerprint)`. Every authorization failure returns one indistinguishable error, so nothing is leaked about which check failed.

---

## 10. Photo library integration

The engine never touches platform APIs directly. It talks to a `PhotoLibraryProvider`:

```
read side:   enumerate(cursor) → page of AssetDescriptor
             openOriginal(assetId) → byte stream
             changesSince(token) → delta or "needs full rescan"
write side:  beginReceive(descriptor) → staging handle
             commit(stagingHandle, descriptor) → assetId
             abandon(stagingHandle)
```

### 10.1 iOS read (sender)

- PhotoKit `PHAsset` fetch, paged, sorted by creation date. Never load the whole library into memory.
- Original bytes via `PHAssetResource` + `PHAssetResourceManager.requestData` (streaming). Never round-trip through `UIImage`, which recompresses.
- `PHAsset.localIdentifier` is device-local and unstable across reinstall and across iCloud re-sync. **Local cache key only, never cross-device identity** (§11).
- Requires `NSPhotoLibraryUsageDescription`.

### 10.2 iOS write (receiver)

- `PHPhotoLibrary.performChanges` with `PHAssetCreationRequest.addResource(with: .photo, fileURL:)`.
- The staging file must be fully written and hash-verified before this call. Partial files are never handed to Photos.
- Assets land in the library (Recents) and are additionally added to a PhotoSync album the app creates.
- Requires `NSPhotoLibraryAddUsageDescription`.

### 10.3 Android read (sender)

- `MediaStore.Images` / `MediaStore.Video` via `ContentResolver`, paged.
- Permissions: `READ_MEDIA_IMAGES` + `READ_MEDIA_VIDEO` on API 33+, `READ_EXTERNAL_STORAGE` below.

### 10.4 Android write (receiver)

- Insert into MediaStore with `IS_PENDING = 1`, stream bytes into the returned URI, verify, then clear `IS_PENDING`. A crash mid-transfer leaves an invisible pending row, not a corrupt gallery entry. The pending URI *is* the staging file, so no copy is needed at commit.
- `RELATIVE_PATH = Pictures/PhotoSync` for images, `Movies/PhotoSync` for video.
- No storage permission is needed to write the app's own inserts on API 29+.
- Transfers of very large files should run under a foreground service. Immich uses a 256 MB threshold for this; adopt the same number until measurement says otherwise.

---

## 11. Asset identity

Filename is not identity. `IMG_1234.HEIC` exists on every iPhone in the world. Neither is the platform asset id: Immich shipped `deviceAssetId + deviceId` as identity, hit exactly the instability described in §10.1, built a remapping layer, abandoned it, and eventually **dropped both columns**. Their scar tissue is our starting point.

Identity is content-derived, at two levels.

### 11.1 Quick key — the candidate filter

```
quick_hash = SHA256( size ‖ first 64 KB ‖ middle 64 KB ‖ last 64 KB )
```

Reads 192 KB instead of the whole file. For an 18 GB library this is the difference between seconds and tens of minutes. Immich, by contrast, streams every byte of every asset through SHA-1 and accepts the cost; §26.5 explains why we do not.

Revision 1 sampled only head and tail. The middle sample at `size/2` was added because head+tail+size collisions are not far-fetched: same camera, same settings, same resolution produces files of identical length with identical EXIF headers. One extra seek and 64 KB is a cheap way to make that much less likely.

**The quick key is a filter, never a verdict.** It may cause an asset to be *offered*; it may not, on its own, cause an asset to be *skipped*. See §11.3.

### 11.2 Full hash — the authority

`SHA-256` over the complete file, computed **incrementally while the bytes stream during transfer**, on both sides. No separate read pass ever happens. Once known, it is cached in `local_asset.full_hash` forever and never recomputed.

Additionally, each 4 MB chunk carries its own SHA-256 (§13).

### 11.3 How a skip decision is actually made

The sender knows the full hash of any asset it has previously streamed to anyone, and only the quick hash of assets it has never sent. So the query carries both when available:

| Sender has | Receiver compares against | Result |
|---|---|---|
| `full_hash` (asset sent before) | `received_asset.full_hash` | Exact. Skip is safe. |
| `quick_hash` only (never sent) | `received_asset.quick_hash` | Candidate match only. |

For the second case the receiver answers "probably present" and the sender **streams the asset anyway**, with the receiver comparing the arriving full hash before committing. If it matches something already held, the asset is discarded at commit time and reported as skipped — bytes were spent, correctness was not.

That sounds wasteful, and it is, exactly once: the first sync of a library to a peer that already contains some of it. Every subsequent sync has full hashes cached and skips exactly. The alternative — trusting a 192 KB sample to declare files identical — risks silently never transferring a photo, which is the one failure mode this product cannot have.

MVP simplification: on a **first** sync to a peer whose `received_asset` table is empty, the quick-hash path cannot produce a false skip anyway, because there is nothing to collide with. The expensive case only arises when syncing to a partially-populated receiver.

### 11.4 Descriptor on the wire

```
quick_hash, full_hash?, size, media_type, mime, created_at, modified_at,
width, height, duration_ms, display_name, resource_group_id
```

---

## 12. Incremental sync

### 12.1 "What is left to send" is a query, not a queue

The strongest idea in Immich's mobile client: it never materialises a pending-upload list. The candidate set is a SQL anti-join, evaluated fresh each time.

```sql
SELECT la.*
FROM   local_asset la
LEFT JOIN sent_log sl
       ON sl.peer_id = :peer_id AND sl.full_hash = la.full_hash
WHERE  la.quick_hash IS NOT NULL     -- hashed, so it can be offered
  AND  la.is_local  = 1              -- original present on device
  AND  sl.full_hash IS NULL          -- not already confirmed sent to this peer
  AND  (:include_videos OR la.media_type = 'image')
ORDER BY la.created_at DESC;
```

**The anti-join is on `full_hash`, never on `quick_hash`.** This is not a stylistic choice; joining on the quick key is a data-loss bug. The quick key is allowed to collide by design (§11.1), so two distinct files can share one. Confirming either of them would then remove *both* from the candidate set, and the second file is silently never transferred — the exact failure §11.3 exists to prevent. §11.3 disciplines the receiver's answer; this join disciplines the sender's own bookkeeping, and both are needed. Reproduced and fixed during implementation (§26.10).

An asset that has never been streamed has `full_hash IS NULL`, so the join matches nothing and it is correctly treated as pending. Confirming an asset inserts one row into `sent_log` and sets `local_asset.full_hash`, after which the asset falls out of the query. **Restart-safety comes for free and there is no queue reconciliation code to get wrong.** Revision 1's design kept a materialised `transfer` row per asset for the whole library; that is 500,000 rows to reconcile after a crash, and a second source of truth to drift.

`transfer` still exists, but only for assets currently in flight (§14) — the rows that carry a byte offset worth resuming. It holds tens of rows, not hundreds of thousands.

Progress counters come from one aggregate query, never from reading the candidate set:

```sql
SELECT COUNT(*)                                          AS total,
       COUNT(*) FILTER (WHERE la.quick_hash IS NULL)     AS hashing,
       COUNT(*) FILTER (WHERE sl.quick_hash IS NULL
                          AND la.quick_hash IS NOT NULL
                          AND la.is_local = 1
                          AND (:include_videos
                               OR la.media_type = 'image')) AS remaining,
       COUNT(*) FILTER (WHERE la.is_local = 0)           AS not_on_device
FROM local_asset la
LEFT JOIN sent_log sl ON sl.peer_id = :peer_id AND sl.quick_hash = la.quick_hash;
```

**`remaining` must apply exactly the same filters as the candidate query above.** The first draft of this section omitted `is_local` and the video filter, which makes the counter report assets that will never be transferred — the progress bar then stops short of 100% and the sync looks stuck at the very end. Any future filter added to §12.1 must be added here in the same commit.

`not_on_device` is the count reported in the completion summary (§19.4) and is the reason §27.2 can stay honest. It is deliberately *not* subtracted from `total`, so the user sees the whole library size and the shortfall explained separately.

(`FILTER` requires SQLite 3.30+, present on all supported OS versions.)

### 12.2 The manifest exchange

```
sender                                          receiver
  │  HAVE_QUERY  [500 × {id, quick_hash, full_hash?}]  ─────►│
  │◄──── HAVE_RESPONSE [per id: send | skip | probable]      │
  │  transfer the send + probable ones                       │
```

- Batch size 500, pipelined against the transfer so the network is never idle.
- The response echoes back the sender's own ids, so no positional coupling.
- Three verdicts, not two: `send`, `skip` (exact full-hash match), `probable` (quick-hash match, stream it and decide at commit — §11.3).
- The receiver's index is authoritative. If the user deleted something on the receiver, it will be sent again. That is the conservative, correct behaviour for MVP.

Second sync of a library that gained 100 photos transfers exactly those 100.

### 12.3 Batch acknowledgement and resume

Borrowed from Immich's sync stream, which is the one part of its protocol that is genuinely resumable:

- Work is applied **then** acknowledged, one ack per batch, carrying the last item in that batch.
- An interruption leaves the checkpoint at the last fully-applied batch.
- Redelivery of a batch tail is harmless because every write is an upsert.
- A checkpoint that is too old to be honoured must force a full re-enumeration rather than silently resume from a stale position.

---

## 13. Chunked transfer, resume, verification

```
Asset (8 GB video)
├── chunk 0   ✓ 4 MB  + SHA-256
├── chunk 1   ✓ 4 MB  + SHA-256
├── ...
└── chunk N   pending
```

This section has no prior art in either reference project. Immich uploads whole files and deletes the partial on disconnect; we keep the partial, which is the entire point.

### 13.1 Chunks

- Fixed 4 MB chunks, sequential within an asset, so resume is a single byte offset.
- **Each chunk carries its own SHA-256.** A corrupt chunk is detected on arrival and retried alone — 4 MB re-sent, not 8 GB. Revision 1 verified only at the end, which turns any corruption in a large video into a full re-transfer. Cost of per-chunk hashes: 32 bytes per 4 MB frame.
- The whole-file SHA-256 is still computed and still authoritative for identity and for the commit decision. Chunk hashes protect the transfer; the file hash proves the result.

### 13.2 Single-pass hashing

Feed the digest from the **same buffer** being written to disk, in one pass. Never re-read a file to hash it. This is the shape Immich's upload path uses, and the discipline that keeps memory flat:

```
on chunk received:
    verify chunk digest
    file_digest.update(buffer)          ← same bytes
    staging.write(buffer)               ← same bytes
    persist bytes_received (see 13.4)
```

Accumulate the byte count from the stream itself. Do not trust a declared length, and reject a zero-byte result.

### 13.3 Staging and commit

- Received bytes go to an app-private temp file, or on Android to the `IS_PENDING` MediaStore URI, which serves as the staging file directly.
- Staging files are named by a random id, so two concurrent transfers of identical content never collide on disk.
- Commit only after the whole-file hash matches. Then, and only then, insert into Photos / clear `IS_PENDING`.
- **There is no path by which a partial or corrupt file becomes a gallery entry.**
- Prefer a rename/flag-clear over a copy at commit. If a copy is unavoidable, re-verify the hash at the destination.

### 13.4 Resume

- On reconnect the receiver reports `bytes_received` per in-flight asset, and the sender resumes from that offset. An 8 GB video interrupted at 6.4 GB resumes at 6.4 GB.
- `bytes_received` is only ever advanced to a **chunk boundary that has been verified and flushed**. Persist it after the flush, never before.
- **Rebuilding the hash state costs one local read.** Standard platform crypto APIs (`MessageDigest`, CommonCrypto, CryptoKit) cannot export or import a partially-consumed digest, so the receiver re-reads its own staging file from byte 0 through a fresh SHA-256 before accepting further chunks. This is local flash I/O, no network cost — roughly 30–60s for a 6.4 GB partial on phone-class storage. Show it as "Resuming…" and never as a stall.
- If the staging file is longer than the persisted `bytes_received`, truncate to that offset first. Trailing bytes from an interrupted write are not trustworthy.

### 13.5 Failure handling

- Chunk hash mismatch → retry that chunk, up to 3 times.
- Whole-file hash mismatch → discard the staging file, retry the whole asset, up to 3 attempts, then mark `FAILED` and continue with the rest of the library.
- One failed asset never fails a session.

### 13.6 State machine

Persisted, survives app kill:

```
DISCOVERED → QUEUED → TRANSFERRING → VERIFYING → COMMITTED
                          │
                          ├─► FAILED → RETRY → TRANSFERRING
                          ├─► SKIPPED_ALREADY_PRESENT
                          └─► CANCELLED
```

`SKIPPED_ALREADY_PRESENT` is a **success** terminal state, not an error (§18).

---

## 14. Persistent state (SQLite)

One database per device. No server, no Redis, no Postgres. The database holds catalog and state only — never media binaries.

Pragmas, following Immich's mobile configuration:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA cache_size   = -32000;   -- 32 MB
PRAGMA temp_store   = MEMORY;
```

### 14.1 Sender side

```sql
-- Library catalog. quick_hash IS NULL is the hashing work queue (§15.3).
CREATE TABLE local_asset (
  id                INTEGER PRIMARY KEY,
  platform_asset_id TEXT    NOT NULL UNIQUE,  -- localIdentifier / MediaStore _ID. Local key only.
  quick_hash        BLOB,                     -- NULL until hashed
  full_hash         BLOB,                     -- NULL until streamed at least once
  size              INTEGER NOT NULL,
  media_type        TEXT    NOT NULL,         -- 'image' | 'video'
  mime              TEXT    NOT NULL,
  created_at        INTEGER NOT NULL,
  modified_at       INTEGER NOT NULL,
  width             INTEGER,
  height            INTEGER,
  duration_ms       INTEGER,
  display_name      TEXT,
  resource_group_id TEXT,                     -- Live Photo pairing
  is_local          INTEGER NOT NULL DEFAULT 1, -- 0 = original not on device (§27.2)
  scanned_at        INTEGER NOT NULL
);
CREATE INDEX idx_local_asset_quick   ON local_asset(quick_hash);
CREATE INDEX idx_local_asset_unhashed ON local_asset(id) WHERE quick_hash IS NULL;
CREATE INDEX idx_local_asset_group   ON local_asset(resource_group_id)
       WHERE resource_group_id IS NOT NULL;

CREATE TABLE peer (
  id               INTEGER PRIMARY KEY,
  cert_fingerprint BLOB NOT NULL UNIQUE,      -- the identity (§9.1)
  device_name      TEXT,
  platform         TEXT,
  paired_at        INTEGER NOT NULL,
  last_seen_at     INTEGER
);

-- Confirmed-sent index. Drives the §12.1 anti-join.
-- KEYED ON full_hash, NEVER ON quick_hash — see §12.1 and §26.10.
-- Every path here already knows the full hash, so it is NOT NULL.
CREATE TABLE sent_log (
  peer_id    INTEGER NOT NULL REFERENCES peer(id) ON DELETE CASCADE,
  full_hash  BLOB    NOT NULL,
  quick_hash BLOB    NOT NULL,   -- diagnostics only, not part of any key
  sent_at    INTEGER NOT NULL,
  PRIMARY KEY (peer_id, full_hash)
) WITHOUT ROWID;

-- IN-FLIGHT ONLY. A row exists while an asset is mid-transfer and is deleted on
-- commit, at which point sent_log gains a row. Tens of rows, not hundreds of thousands.
CREATE TABLE transfer (
  session_id       TEXT    NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  local_asset_id   INTEGER NOT NULL REFERENCES local_asset(id) ON DELETE CASCADE,
  state            TEXT    NOT NULL,
  bytes_transferred INTEGER NOT NULL DEFAULT 0,
  total_bytes      INTEGER NOT NULL,
  retry_count      INTEGER NOT NULL DEFAULT 0,
  error_code       TEXT,
  updated_at       INTEGER NOT NULL,
  PRIMARY KEY (session_id, local_asset_id)
);
```

### 14.2 Receiver side

```sql
-- Everything ever accepted. full_hash is the primary identity; quick_hash is
-- indexed for the candidate lookup in §11.3.
CREATE TABLE received_asset (
  full_hash         BLOB PRIMARY KEY,
  quick_hash        BLOB    NOT NULL,
  size              INTEGER NOT NULL,
  platform_asset_id TEXT,
  peer_id           INTEGER REFERENCES peer(id) ON DELETE SET NULL,
  received_at       INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX idx_received_quick ON received_asset(quick_hash);

-- In-flight inbound assets. This is what makes resume possible.
CREATE TABLE inbound_transfer (
  session_id     TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  quick_hash     BLOB NOT NULL,
  staging_ref    TEXT NOT NULL,               -- temp path, or pending MediaStore URI
  bytes_received INTEGER NOT NULL DEFAULT 0,  -- verified, flushed chunk boundary only
  total_bytes    INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY (session_id, quick_hash)
);
```

### 14.3 Both sides

```sql
CREATE TABLE session (
  id            TEXT PRIMARY KEY,
  role          TEXT    NOT NULL,             -- 'sender' | 'receiver'
  peer_id       INTEGER REFERENCES peer(id) ON DELETE SET NULL,
  state         TEXT    NOT NULL,             -- active | interrupted | done | cancelled
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER,
  items_total   INTEGER NOT NULL DEFAULT 0,
  items_done    INTEGER NOT NULL DEFAULT 0,
  items_skipped INTEGER NOT NULL DEFAULT 0,
  items_failed  INTEGER NOT NULL DEFAULT 0,
  bytes_total   INTEGER NOT NULL DEFAULT 0,
  bytes_done    INTEGER NOT NULL DEFAULT 0
);

-- Library change-detection checkpoints (§15).
CREATE TABLE scan_state (key TEXT PRIMARY KEY, value BLOB, updated_at INTEGER NOT NULL);
```

Migrations are stepwise and transactional: disable `foreign_keys`, run the steps, re-enable in a `finally`, and run `PRAGMA foreign_key_check` in debug builds.

On launch: load any `session` in state `interrupted` and offer to resume (§20).

---

## 15. Library scanning and change detection

Revision 1 had no story here beyond a `scanned_at` column, which implied re-enumerating and re-hashing the whole library on every sync. Both platforms provide change tokens that make a rescan nearly free.

### 15.1 Delta detection

| Platform | Mechanism | Fallback |
|---|---|---|
| Android API 30+ | `MediaStore.getVersion()` to detect a store reset; per-volume `MediaStore.getGeneration()` with `GENERATION_ADDED > ? OR GENERATION_MODIFIED > ?` | — |
| Android ≤ 29 | none available | Always full rescan |
| iOS 16+ | `PHPersistentChangeToken`, archived in `scan_state` | — |
| iOS < 16 | none available | Always full rescan |

Checkpoint after a successful scan, never before. A missing or unreadable token means full rescan — that must be a normal, tested path, not an error.

PhotoSync is spared the two platform gaps that complicate Immich's implementation: it cannot detect album deletion on Android, and iOS excludes shared albums from the change stream. Neither matters here, because PhotoSync reads the library as a flat set and has no album concept on the read side.

### 15.2 Detecting a *modified* asset without hashing it

Compare cheap metadata, deliberately not the hash:

- Android: `modified_at`, `size`, `width`, `height`, `duration_ms`
- iOS: `modificationDate` / adjustment timestamp, `size`, `width`, `height`, `duration_ms`

If any differ, the asset is treated as new content: **set `quick_hash` and `full_hash` back to `NULL`**. That single write re-enrols it in the hashing queue (§15.3) and, once hashed, it reappears in the §12.1 candidate query with a new quick hash. The loop closes itself with no invalidation logic anywhere else.

### 15.3 Hashing pipeline

- `WHERE quick_hash IS NULL` **is** the work queue. No separate table.
- Hash off the UI thread, natively where the platform provides a streaming digest. Buffer 2 MB per read (Immich's measured choice).
- Batch and commit incrementally, so a crash loses at most one batch. Suggested batch: 32 assets on iOS, 512 on Android, or 1 GB of bytes, whichever comes first — iOS pays more per asset because originals may need materialising.
- Cancellable end to end: a cancel flag checked between batches plus a cancel call into the native hasher.
- A hash, once stored, is never recomputed. This is what makes the second sync cheap.
- Hashing is opportunistic: run it after launch, after permission grant, and when the Send tab is opened, so the library is usually already hashed by the time the user taps Sync.

---

## 16. Media, metadata, Live Photos

- Photos: JPEG, HEIC/HEIF, PNG, WebP. Videos: MP4, MOV, HEVC.
- **Transfer the original bytes, unmodified.** No recompression, no resizing, no re-muxing. EXIF, GPS, orientation, camera info and creation date are preserved because the file is preserved. The only filesystem metadata we set is the timestamp.
- Set the destination asset's creation date from the source `created_at`, so the receiver's timeline is not all "today".
- **Live Photos** are one logical asset made of two resources sharing a `resource_group_id`. The wire format carries resource groups from day one so this is not a later rewrite.

Ordering rule, adopted from Immich's motion-photo handling: when a group is transferred, **send the video resource first, then the still**, and let the still's descriptor carry the group reference. The second item is what creates the link, so this order is what makes phase-2 reassembly (`PHAssetCreationRequest` with `.photo` + `.pairedVideo`) possible without a protocol change. In MVP a Live Photo arrives as a still plus a short video (§27.4).

---

## 17. Architecture

```
┌──────────────────────────────────┐
│  UI Layer  (Send tab / Receive)  │
├──────────────────────────────────┤
│  Session Orchestrator            │
├──────────────────────────────────┤
│  Sync Engine                     │  ← platform-agnostic, unit-testable
│  identity · diff · queue · state │
├──────────────────────────────────┤
│  Providers (interfaces)          │
│  PhotoLibrary · Storage          │
│  Discovery   · Transport         │
└──────────────────────────────────┘
```

The Sync Engine must not know which OS it runs on. It knows: Asset, Manifest, Transfer, State, Session.

Each provider needs an in-memory fake so the engine can be tested end to end without a phone, without a network, and without a photo library. Build those fakes first (§25 step 2). The fakes must be able to simulate: a dropped connection mid-chunk, a corrupt chunk, a duplicate asset, a full disk, and a library that changes between scans.

---

## 18. Wire protocol

Versioned from the first byte. Reject a mismatched major with a human-readable message, not a crash.

```
CONNECT            TLS 1.3, mutual, fingerprint pinned when known
HELLO              version, device name, platform, cert fingerprint
AUTHENTICATE       code-bound HMAC proof, both directions (§9.3)
SESSION_BEGIN      role, session id, estimated item count, estimated bytes
HAVE_QUERY         batch of {id, quick_hash, full_hash?}
HAVE_RESPONSE      per id: send | skip | probable
ASSET_BEGIN        descriptor, total size, resume offset
CHUNK              4 MB payload + chunk SHA-256
CHUNK_ACK          chunk index accepted | retry
ASSET_END          full SHA-256
ASSET_ACK          committed | already_present | hash_mismatch | error
BATCH_ACK          checkpoint after an applied batch (§12.3)
SESSION_END        totals, including skipped and failed counts
HEARTBEAT          both directions, 10s
```

Two semantic rules that matter more than the frame layout:

**A duplicate is a success, not an error.** `already_present` travels the same path as `committed` and increments the skipped counter. Immich goes as far as downgrading its HTTP status so clients cannot accidentally treat a duplicate as a failure, and its client handles both codes identically. Copy the intent: no error path, no retry, no user-visible warning for an asset the receiver already has.

**`RESUME_OFFSET` is receiver-authoritative.** The sender proposes nothing; it asks, and continues from whatever the receiver reports. The receiver derives that value only from verified, flushed chunk boundaries (§13.4).

---

## 19. UI specification

The complexity belongs in the sync engine, never on screen. Inspired by LocalSend and AirDrop, not by Google Photos or the Immich dashboard.

No login. No account. No server settings. No technical vocabulary anywhere in the interface.

### 19.1 Home — two tabs, nothing else

```
┌─────────────────────────────┐
│                             │
│        PhotoSync            │
│                             │
│    12,431 photos            │
│     1,203 videos            │
│                             │
│  ┌──────────┬──────────┐    │
│  │   Send   │ Receive  │    │
│  └──────────┴──────────┘    │
│                             │
│   Enter the code shown on    │
│   the other phone            │
│                             │
│      ┌───────────────┐       │
│      │  _ _ _  _ _ _ │       │
│      └───────────────┘       │
│                             │
│   Include videos       [on] │
│                             │
│         [ Sync ]             │
│                             │
└─────────────────────────────┘
```

The default selection is **the entire library**. There is no file picker in the primary flow. "Include videos" is the only filter in MVP.

If hashing (§15.3) is still running, the counts show as "preparing" rather than blocking the Sync button — the engine simply has fewer candidates ready, and picks up the rest as they hash.

### 19.2 Receive — see §6.

### 19.3 In progress

```
┌─────────────────────────────┐
│                             │
│         Syncing...          │
│                             │
│           1,284             │
│         of 12,431           │
│                             │
│     ████████████░░░░        │
│                             │
│        235 MB/s             │
│      2.1 GB / 18.4 GB       │
│                             │
│         [ Pause ]            │
│                             │
└─────────────────────────────┘
```

Keep the screen awake during a session and say so once. Never show chunk indices, hashes, socket state, queue ids or SQL.

During a resume, show "Resuming…" while the receiver rebuilds its hash state (§13.4). It can take a minute on a large partial and must not look like a hang.

### 19.4 Complete

```
┌─────────────────────────────┐
│                             │
│             ✓               │
│                             │
│       Sync complete         │
│                             │
│       12,431 items          │
│      18.4 GB transferred    │
│                             │
│   Saved to your Photos       │
│                             │
│         [ Done ]             │
│                             │
└─────────────────────────────┘
```

When anything was skipped or left behind, the summary says so plainly, one line each:

```
   412 already on that phone
   38 not on this phone (in iCloud)
```

The iCloud line is required, not optional — it is what keeps the offline promise honest (§27.2).

### 19.5 Not found

```
│      Can't find that code    │
│                              │
│   Make sure both phones are  │
│   on the same Wi-Fi.         │
│                              │
│   [ Try again ]  [ Help ]    │
```

Help explains the hotspot setup in plain language, and mentions guest-network isolation, which is a real cause we cannot detect or fix.

### 19.6 Wording

The button says **Sync**, never Send. The product is not "send me these files", it is "make the other phone have my library".

### 19.7 Settings — deliberately tiny

```
Save location
Include videos
Keep screen on during sync
Device name
Paired devices        (with Forget — required by §9.5)
About
```

---

## 20. Errors

Human-readable, actionable, resumable. Never an error code.

Wrong:
```
ERR_TRANSFER_CHUNK_HASH_MISMATCH
```

Right:
```
Couldn't finish syncing.
The connection was interrupted.

8,421 of 12,431 items are done.

[ Resume ]        [ Cancel ]
```

Resume must continue, never restart. Both sides reload state from SQLite (§14) and reconnect using pinned fingerprints, with no code re-entry (§9.5).

Cases needing their own copy: code expired, wrong code, too many wrong attempts, receiver ran out of space, permission denied, both phones on different networks, guest-network isolation, phone too hot or battery critical, originals not downloaded from iCloud.

---

## 21. Scale constraints

Designed for 10,000 / 100,000 / 500,000 assets.

- Never load the library into RAM. Page everything, on both the platform enumeration and the SQL side.
- Stream file bytes; never read a whole video into memory.
- Batch the manifest queries; pipeline them with transfer.
- Derive the candidate set from SQL (§12.1) and **apply a LIMIT** — the query is ordered, so paging it is trivial. Immich's equivalent query is unbounded and materialises the full candidate list, which is a real limitation at library scale; do not repeat it.
- The UI reads aggregate counters (§12.1), never the transfer table.
- Keep `transfer` and `inbound_transfer` small by construction: rows exist only while in flight.

---

## 22. Platform prerequisites

These gate the schedule and at least one of them is outside our control. Resolve them before writing transport code, not after.

### 22.1 iOS

| Requirement | Note |
|---|---|
| `com.apple.developer.networking.multicast` entitlement | **Requires Apple approval via a request form.** Without it, sending or receiving multicast fails on iOS 14+. This is the hardest external dependency in the project and it blocks §7.2 entirely. Submit the request on day one. |
| `NSLocalNetworkUsageDescription` | Required for the local-network permission prompt. |
| `NSBonjourServices` | Needed to unlock the local-network prompt even without using Bonjour for discovery. Declare the service type we would use. |
| `NSPhotoLibraryUsageDescription` | Read (sender). |
| `NSPhotoLibraryAddUsageDescription` | Write (receiver). |
| Socket rebind on foreground | Sockets die silently on suspend and cannot be probed. Rebind unconditionally on resume. |
| `PHPersistentChangeToken` | iOS 16+ only; below that, full rescan. |

Contingency if the multicast entitlement is refused or delayed: subnet scan (§7.4) and manual address (§7.5) still work without it, and pairing still works. Discovery becomes slower, not impossible. Ship behind that fallback rather than blocking.

### 22.2 Android

| Requirement | Note |
|---|---|
| `READ_MEDIA_IMAGES`, `READ_MEDIA_VIDEO` | API 33+. `READ_EXTERNAL_STORAGE` below. |
| `ACCESS_LOCAL_NETWORK` | Runtime permission, API 37+. Local network traffic is blocked without it. Not present in older compile SDKs, so the permission string and API level may need hardcoding. |
| No multicast lock | Not required when binding our own sockets and joining the group directly, as verified in LocalSend. |
| Foreground service | For large transfers. 256 MB is a reasonable threshold to start from. |
| MediaStore generation | API 30+ for delta; full rescan below. |

#### Verified toolchain

Cross-compilation of the Rust core to Android was proven before further engine work, since a failure here would invalidate the stack choice. Confirmed working:

| Component | Version |
|---|---|
| Rust | 1.88.0, host `x86_64-pc-windows-msvc` |
| Rust targets | `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android` |
| cargo-ndk | 4.1.2 |
| Android NDK | 28.2.13676358 — the version Flutter 3.44 pins in `FlutterExtension.kt` |
| Android SDK | platforms 34/35/36, build-tools 34/35/36 |
| Flutter | 3.44.0 stable, Dart 3.12.0 — wants compileSdk/targetSdk 36, minSdk 24 |
| JDK | 21 |

Build command, and the result that matters:

```
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o jniLibs build -p photosync-ffi --release
```

produces `libphotosync.so` verified as `ELF64 / DYN / AArch64` with `photosync_self_check` and `photosync_schema_version` exported. `libsqlite3-sys` with the `bundled` feature compiles its C sources through the NDK without special configuration, which was the specific risk worth retiring early.

Two things that cost time and will again:

- Always match the NDK to the version Flutter pins, read from `FlutterExtension.kt` rather than guessed. Flutter also raises `compileSdk` faster than an existing SDK install tracks, so check the platform is present.
- On Windows `cmd`, `set VAR=value & next` captures the space before the `&` into the value, producing a path that does not exist. Use `set "VAR=value"`.

### 22.3 Development

Windows and macOS are development and test platforms only, but the multicast receive loop must tolerate 10 consecutive errors there (§7.2), or testing on Windows produces phantom failures.

#### The `psdev` harness

Testing happens on real devices, so the engine ships with a desktop harness rather than a unit-test suite. Its purpose is not to replace device testing but to make the conditions that *cannot* be produced on demand reproducible:

| Command | Purpose |
|---|---|
| `psdev fixture <file> <mb>` | Pseudorandom test file. Deliberately not zero-filled: identical bytes give every chunk the same hash and would let a misaligned or duplicated chunk pass unnoticed. |
| `psdev collide <dir>` | Two distinct files sharing size, head, middle and tail. Criterion 10's fixture; cannot occur by accident. |
| `psdev scan` / `status` / `sent` | Catalogue, counters, and one simulated confirmed transfer. Asserts the §12.1 invariant that `remaining` equals the candidate count. |
| `psdev transfer <file> clean\|cut\|corrupt\|misaligned\|all` | Drives the real chunk pipeline with injected faults. |
| `psdev selfcheck` | The same self check the native library exposes, for comparing desktop against device. |
| `psdev pairing` | The code-bound auth properties: MITM transcript binding, role reflection, cross-session replay, the attempt budget, expiry, input parsing. |
| `psdev handshake` | Real TCP + TLS 1.3, three legs: honest first contact, a relaying MITM that does not know the code, and a pinned-fingerprint mismatch. |
| `psdev sync <src> <dst>` | A full session between two databases and two library directories over real TCP + TLS: first sync, repeat sync of an unchanged library, and a sync after one asset is added. |
| `psdev resume-session <src> <dst>` | Cuts a sync mid-asset, then reconnects with **no code** and finishes. Criteria 3 and 7 at the session level. |

`psdev sync` is where criteria 1, 2 and 10 stop being arguments:

```
first sync              transferred=5 skipped=0 failed=0 bytes=19922944  OK
  byte-for-byte identical to the source: yes
  staging directory empty: yes
second sync, unchanged  transferred=0 skipped=5 failed=0 bytes=0  OK
  all 5 answered with an exact full-hash Skip: yes
third sync, one added   transferred=1 skipped=5 failed=0 bytes=33  OK
```

The five assets include the quick-key collision pair, so criterion 10 now runs through the whole pipeline rather than only the catalogue: the second of the pair is offered on a `Probable` verdict, streamed, and committed because the full hashes differ.

"Staging directory empty" is checked deliberately. A leftover partial means a commit that did not move its staging file or an abandon that did not delete it, and either would quietly accumulate storage on a phone.

`psdev resume-session` closes the loop on the resume story:

```
leg 1 (cut at ~7 MB)        transferred=1
  resumable session 793188bc with 1 in-flight asset(s)
    partial: 4194304 of 8388608 bytes (50%)
leg 2 (reconnect, no code)  transferred=2 failed=0
final state
  assets in the receiver library : 3 of 3 OK
  byte-for-byte identical        : yes OK
  staging empty                  : yes OK
  second leg needed no code      : yes OK, pinned fingerprint pair only (§9.5)
```

Note the partial sits exactly on a chunk boundary. That is the invariant from §13.4 holding: `bytes_received` never names anything but verified, flushed bytes.

`transfer` covers four cases, three of which are impractical to trigger deliberately on hardware:

- **cut** — drop mid-asset, append unflushed junk past the last acknowledgement, reconnect, resume. Criterion 3.
- **corrupt** — flip a bit in one chunk. Criterion 9. Over TCP this essentially never happens in the wild, which is precisely why the retry path would otherwise stay unexercised until a user hit it.
- **misaligned** — offer a chunk from further ahead than the receiver expects. Confirms the chunk is refused rather than written at the offset it claims, which would leave a hole that every per-chunk hash still passes.
- **clean** — the baseline, checked against an independently computed digest.

The same fault switches belong in a hidden debug menu in the app, so these cases can also be forced on a phone (§13.5, §19).

---

## 23. MVP scope

In:

- Send / Receive roles, one direction per session
- 6-digit pairing code, single-use, expiry, global 3-attempt limit
- Multicast announcement discovery, subnet-scan fallback, manual address, hotspot support
- Whole-library selection, photos and videos
- Quick-key candidate filter plus full-hash authority, so nothing is silently skipped and nothing is sent twice
- Cheap library rescan via platform change tokens
- 4 MB chunked transfer with per-chunk hashes and byte-offset resume
- Streaming SHA-256 verification before commit
- Save into native Photos (iOS) / MediaStore (Android)
- SQLite state: derived candidate query, in-flight transfer rows, resumable sessions
- TLS 1.3, mutual, fingerprint-pinned, with code-bound authentication
- Paired-device list with Forget
- The UI in §19 and the error handling in §20

Out (§3), plus explicitly deferred: two-way sync, deletion propagation, albums, favorites, Live Photo reassembly, background sync, desktop, QR pairing.

---

## 24. Success criteria

1. iPhone with 10,000 photos → Android with 0. Result: 10,000 on the Android, visible in the Gallery app, correct dates.
2. Same pair again after the iPhone gains 100 photos. Result: exactly 100 transferred, and the rescan does not re-hash the library.
3. 8 GB video at 60%, Wi-Fi dropped, reconnect. Result: resumes near 60%, no code re-entry, final hash matches.
4. Same 5 GB video already on the receiver, previously sent by this sender. Result: not transferred, reported as skipped.
5. Android hotspot, iPhone joined, no internet at all. Result: pairing and sync both work.
6. 100,000+ assets. Result: app stays responsive, memory flat, no full-library load.
7. App force-killed mid-sync, reopened. Result: offers Resume and completes.
8. Wrong code entered 3 times. Result: receiver issues a new code, no session established, and a 4th attempt with the old code fails.
9. A chunk is corrupted in transit. Result: that chunk alone is retried, the asset completes, the hash matches.
10. Two distinct files with identical size, head, middle and tail. Result: both arrive intact. Neither is silently dropped.

Criteria 3, 5, 7 and 10 are the ones that decide whether this product is real. Test them on hardware before building any UI polish. Criterion 10 needs a synthetic fixture; build it in the fakes (§17).

---

## 25. Build order

1. Asset model and SQLite schema (§14)
2. Fake providers — library, storage, transport, discovery — including fault injection (§17). Enables everything below to be tested without devices
3. Quick-key identity, manifest diff, the derived candidate query (§11, §12)
4. Transfer state machine, in-flight rows, session persistence (§13.6, §14)
5. Chunked transfer, per-chunk hashes, resume, streaming SHA-256 verify (§13)
6. Real TCP transport, then TLS, then code-bound auth (§8, §9)
7. Discovery: multicast announce, then subnet-scan fallback (§7)
8. iOS PhotoKit read adapter and hashing pipeline (§10.1, §15)
9. Android MediaStore write adapter — prove iOS → Android end to end (§10.4)
10. The reverse pair of adapters
11. Library change detection (§15.1)
12. UI per §19
13. Hotspot, interruption and thermal testing on real hardware

Do not start with the UI. The engine is the product.

Submit the iOS multicast entitlement request (§22.1) before step 1 — the approval clock runs in parallel with everything else.

---

## 26. Decisions and rationale

Recorded so they can be revisited deliberately rather than by accident.

### 26.1 mDNS rejected as the discovery primary

Revision 1 specified mDNS as primary with subnet scan as fallback. Rejected in favour of UDP multicast announce plus TCP callback.

- LocalSend, the closest working analogue, contains no mDNS/DNS-SD/NsdManager code at all. There is no prior art to copy in that direction.
- The one scenario PhotoSync must support (§7.6, phone hotspot, no internet) is precisely where multicast-based service discovery is least reliable, and mDNS adds a responder and a query/response state machine on top of the same fragile multicast substrate.
- A raw announcement is one datagram whose payload we control, sent three times. It is easier to reason about, easier to debug with a packet capture, and it fails in one way instead of several.
- The announce-then-callback shape has a property mDNS browsing lacks: a peer only enters the candidate list after a successful TCP connection, so everything discovered is known to be reachable.

Cost of the decision: we lose interoperability with generic Bonjour browsers, which we do not want anyway, and we must pick and defend our own multicast group and port.

### 26.2 The pairing code is not in the announcement

Revision 1 put the session code in an mDNS TXT record so the sender could match receivers before connecting. That broadcasts a 20-bit secret to the whole LAN, including the attacker the code exists to defeat. The code now appears only inside the authenticated handshake, and candidate selection is done by trying to authenticate.

### 26.3 LocalSend's PIN model rejected wholesale

Studied and deliberately not copied. Its PIN is static across sessions, stored in plaintext preferences, transmitted as a plaintext query parameter, unbound to the certificate, grants no durable trust, and its rate limiter is per-IP, resettable by any success, non-persistent and non-constant-time. Every one of those properties is inverted in §9.

### 26.4 Device-based identity rejected

Immich shipped `deviceAssetId + deviceId + ownerId` as asset identity, hit instability on both platforms, built an iCloud-id remapping layer, disabled it, and finally dropped both columns from the schema. Content hashing is the destination; start there.

### 26.5 Full-library hashing rejected, but the full hash kept authoritative

Immich streams every byte of every asset through SHA-1 before upload, mitigated by native hashing, a persistent cache, and batching. For an 18 GB library on a phone that is tens of minutes of I/O before the first byte moves, and PhotoSync's premise is that a sync starts immediately.

The quick key (§11.1) avoids it. The residual risk — two distinct files sharing size, head, middle and tail — is handled by never letting the quick key alone cause a skip (§11.3), rather than by pretending it cannot happen. Criterion 10 tests it.

### 26.6 Materialised send queue rejected

Revision 1 created a `transfer` row per asset for the whole library. Replaced by the derived anti-join (§12.1), with `transfer` reduced to in-flight rows only. This removes a second source of truth, removes crash reconciliation, and turns a 500,000-row table into a few dozen rows.

### 26.7 Per-chunk hashes added

Revision 1 verified only the whole-file hash at the end, so any corruption in an 8 GB video meant re-sending 8 GB. Per-chunk SHA-256 costs 32 bytes per 4 MB and reduces that to 4 MB. Given that resume of a large video is success criterion 3, the trade is obvious.

### 26.8 Hash state is rebuilt by re-reading, not persisted

Platform crypto APIs do not expose export/import of partial digest state, so on resume the receiver re-reads its own staging file to rebuild the SHA-256 (§13.4). Alternatives considered: persisting raw digest state (not portable, and not available through `MessageDigest` or CryptoKit), or defining the file identity as a hash of chunk hashes (removes the re-read, but ties identity to the chunk size, so changing 4 MB later would invalidate every stored identity). The local read is the cheapest honest option.

### 26.9 A duplicate is a success

Immich returns a success status for a duplicate upload and its client treats it identically to a new asset, so no error path exists. Adopted verbatim in spirit (§18).

### 26.10 `sent_log` is keyed on the full hash

Found by running the engine, not by reading it. The first implementation keyed `sent_log` on `(peer_id, quick_hash)` and joined the candidate query on `quick_hash`, mirroring revision 1 of §14. With a deliberately constructed collision pair — two distinct 512 KB files sharing size, head, middle and tail — confirming one removed **both** from the candidate set, so the second was silently never sent. Success criterion 10 failed.

The lesson generalises beyond the one line of SQL: §11.3 was written as a rule about the receiver's answer, and that framing hid the fact that the same rule governs the sender's own records. Anywhere a quick key is used as though it were an identity is a place an asset can disappear. The quick key may filter; only the full hash may identify.

Reproducible with the dev harness: `psdev collide <dir>` writes the fixture, `psdev scan` catalogues it, `psdev sent` confirms one and asserts that exactly one asset leaves the candidate set.

### 26.11 The MITM claim was overclaimed, and the first test was wrong

The first attempt at `psdev handshake` modelled a man in the middle by having the attacker *be* the receiver while knowing the code. It passed the handshake, the test reported a failure, and the failure was in the test.

An attacker holding the code is indistinguishable from the intended peer, by construction: at first contact the code is the only authenticator there is. The property worth testing, and the one §9.3 actually delivers, is that an attacker who does **not** know the code cannot relay between two honest devices — its two TLS legs each present its own certificate, so the sender's transcript and the receiver's transcript cannot agree.

Two things came out of this beyond the corrected test. §9.4 now states the property and its limit explicitly rather than leaving "a man in the middle is rejected" to be read too broadly. And the relay itself had to be a genuine bidirectional byte copy: the receiver sends `Hello` and `AuthChallenge` back to back, so a request/response relay deadlocks — a real attacker would not have made that mistake, and a test that does gives a false pass.

---

## 27. Open decisions

Everything else in this document is settled. These are not.

### 27.1 Stack — blocking, decide first

Not addressed in revision 1, and it blocks §25 step 1. The reference projects took different routes: LocalSend is Flutter with the protocol core in Rust exposed through FFI; Immich is Flutter with native Kotlin/Swift for library access and hashing via a generated channel.

Options: single Flutter codebase; Flutter with native platform channels for PhotoKit/MediaStore and hashing; shared engine in Rust or Kotlin Multiplatform with native UI; fully native twice.

Both references put the photo-library and hashing work in native code regardless of the UI choice, which suggests platform channels are unavoidable in a Flutter build. §17 requires the engine to be platform-agnostic and testable without a device; that is the constraint any choice must satisfy.

### 27.2 iCloud "Optimize iPhone Storage" — recommendation settled, needs confirming

If enabled, originals are not on the phone and PhotoKit must download them, which needs internet and contradicts the offline promise.

Decision: **skip non-local assets, count them, and report the count in the completion summary** (§19.4). The `is_local` column (§14.1) carries the flag, and the §12.1 query filters on it. Immich's precedent supports this shape — it gates network fetches on whether the user explicitly scoped that content for backup, rather than silently downloading. PhotoSync's default scope is the whole library, so the honest equivalent is to skip and disclose.

Still open: whether to offer an explicit "download missing originals" action when internet is present, and whether that belongs in MVP.

### 27.3 HEIC / HEVC arriving on Android

§16 says never recompress, so an iPhone HEIC lands as HEIC. Android decodes HEIF from API 28+ and modern gallery apps handle it, but some third-party apps do not. Options: keep originals (recommended), or offer an optional "convert to JPEG for compatibility" toggle that violates the no-recompression rule only when the user explicitly asks.

### 27.4 Live Photos in MVP

Transfer both resources and accept two visible items, or hold them back until phase 2? Recommendation: transfer both, group them via `resource_group_id`, send video first (§16), accept two visible items for now.

### 27.5 Receiver destination

Main library plus a PhotoSync album, or main library only? Recommendation: both — easy to find, easy to undo.

### 27.6 Screen-off behaviour

iOS will suspend the app. Options: require the screen on (simple, honest) or attempt background transfer (unreliable, large effort). Recommendation: require screen on for MVP and say so in the UI. Note that §8's socket rebind on resume already handles the short-suspend case.

---

## Appendix A — Prior art references

Verified against the copies in `referer/` at the time of writing. Paths are relative to each project root. Mechanisms were read and confirmed; constants below were grep-verified.

### LocalSend — `referer/localsend/`

| Topic | Location | Confirmed detail |
|---|---|---|
| Multicast constants | `packages/core/src/multicast/mod.rs` | Group `224.0.0.167` with the `224.0.0.0/24` Android constraint documented in-source; port `53317`; announce delays 100/500/2000 ms |
| Socket options | `packages/core/src/multicast/socket.rs` | Wildcard bind, reuse addr/port, per-interface join, `IP_MULTICAST_IF` pinning, TTL 1, loopback on |
| Discovery orchestration | `packages/core/src/discovery/mod.rs` | Probe timeout `500ms`; scan concurrency `50`; staged escalation with grace period; announce-then-register handshake |
| App-level scan policy | `app/lib/provider/network/scan_facade.dart` | Interface cap of 3; 1s grace before fallback |
| PIN handling | `packages/core/src/http/server/common/pin.rs` | `MAX_PIN_ATTEMPTS = 3`, per-IP LRU, reset on success, plaintext comparison |
| Certificate and fingerprint | `packages/core/src/crypto/cert.rs` | Self-signed, no SAN, SHA-256 over DER as uppercase hex |
| Handshake-time pinning | `packages/core/src/http/client/server_cert_verifier.rs` | Fingerprint compared inside `verify_server_cert`; hostname check deliberately skipped |
| Session model | `packages/core/src/http/server/v2.rs` | Session id, per-file tokens, sender-IP binding, single-session slot, no TTL |
| iOS/Android platform setup | `app/ios/Runner/Runner.entitlements`, `app/android/app/src/main/AndroidManifest.xml`, `MainActivity.kt` | Multicast entitlement; `ACCESS_LOCAL_NETWORK` runtime request |
| iOS resume rebind | `app/lib/main.dart` | Unconditional discovery restart on resume |

### Immich — `referer/immich/`

| Topic | Location | Confirmed detail |
|---|---|---|
| Streaming hash during write | `server/src/middleware/file-upload.interceptor.ts` | `createHash('sha1')` fed from the same piped chunks; size accumulated from the stream; empty file rejected |
| Upload orchestration | `server/src/services/asset-media.service.ts` | Staging path, duplicate-as-success, cleanup on failure, `bulkUploadCheck` |
| Checksum identity | `server/src/schema/tables/asset.table.ts`, `server/src/utils/database.ts` | Partial unique index `UQ_assets_owner_checksum`; `bytea` checksum plus algorithm enum |
| Device identity removal | `server/src/schema/migrations/1776263790468-DropDeviceIdAndDeviceAssetId.ts` | `deviceAssetId` and `deviceId` dropped |
| Derived candidate query | `mobile/lib/infrastructure/repositories/backup.repository.dart` | Checksum anti-join; `COUNT(*) FILTER` counters; `checksum IS NULL` as the hash queue |
| Local change detection | `mobile/lib/domain/services/local_sync.service.dart` | Full vs delta sync, fast/slow path, metadata-only change comparison |
| Platform change tokens | `mobile/android/.../sync/MessagesImpl30.kt`, `mobile/ios/Runner/Sync/MessagesImpl.swift` | MediaStore version + per-volume generation; `PHPersistentChangeToken` |
| Native streaming hash | `mobile/android/.../sync/MessagesImplBase.kt` | 2 MB read buffer |
| Concurrency limits | `mobile/lib/main.dart` | `holdingQueue (6, 6, 3)`; foreground service above 256 MB |
| Batch enqueue | `mobile/lib/services/background_upload.service.dart` | Batch size 100; Live Photo video-then-still ordering |
| Delta protocol | `server/src/services/sync.service.ts` | Apply-then-ack, per-type checkpoints, deletes before upserts, retention-bounded forced resync |
| Mobile persistence | `mobile/lib/data/db/main/database.dart` | Drift/SQLite, WAL, stepwise transactional migrations |
| Absence of chunking | repo-wide | No matches for `Upload-Offset`, `Tus-Resumable`, `@tus/`. Confirmed absent. |

### Licensing

LocalSend is Apache-2.0; Immich is AGPL-3.0. This document describes mechanisms and cites locations rather than reproducing implementation code. That distinction stops mattering only if PhotoSync is distributed or exposed over a network to other users — at which point copying implementation code, particularly from the AGPL project, needs a deliberate look. Independent implementation of a described technique is not encumbered.
