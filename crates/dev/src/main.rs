//! `psdev` — desktop harness for the PhotoSync engine.
//!
//! This is the dev tool that replaces the fakes-and-unit-tests approach: it runs
//! the real engine over a real directory of real files, on a desktop, so the
//! parts that are awkward to trigger on a phone can be exercised deliberately
//! (`app_info.md` §22.3). It is not shipped.
//!
//! Usage:
//!   psdev scan <dir> [db]        catalogue and hash a directory as a library
//!   psdev status <db>            counters and the next page of candidates
//!   psdev resume <file>          prove resumed hashing matches a single pass
//!   psdev collide <dir>          write a quick-key collision pair (criterion 10)

use photosync_core::catalog::{self, FilterArgs, ScannedAsset};
use photosync_core::chunk::{self, ChunkVerdict, FinishVerdict, InboundAsset, OutboundAsset};
use photosync_core::identity::{self, FullHasher, QUICK_SAMPLE_LEN};
use photosync_core::model::{Hash32, MediaType};
use photosync_core::pairing::{AuthChallenge, CodeSession, PairingCode, ProofRole};
use photosync_core::provider::FileStaging;
use photosync_core::{db, Result};
use std::fs::{self, File};

mod cutstream;
mod fsprovider;
mod handshake;
mod sync;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// A fixed fake fingerprint so repeated runs address the same peer row.
const DEV_PEER_FP: [u8; 32] = [0xAB; 32];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("scan") => match args.get(1) {
            Some(dir) => cmd_scan(Path::new(dir), db_path(args.get(2))),
            None => usage("scan needs a directory"),
        },
        Some("status") => cmd_status(db_path(args.get(1))),
        Some("resume") => match args.get(1) {
            Some(f) => cmd_resume(Path::new(f)),
            None => usage("resume needs a file"),
        },
        Some("collide") => match args.get(1) {
            Some(d) => cmd_collide(Path::new(d)),
            None => usage("collide needs a directory"),
        },
        Some("sent") => cmd_sent(db_path(args.get(1))),
        Some("selfcheck") => cmd_selfcheck(),
        Some("transfer") => match args.get(1) {
            Some(f) => cmd_transfer(Path::new(f), args.get(2).map(String::as_str)),
            None => usage("transfer needs a file"),
        },
        Some("pairing") => cmd_pairing(),
        Some("handshake") => handshake::run(),
        Some("sync") => match (args.get(1), args.get(2)) {
            (Some(src), Some(dst)) => sync::run(Path::new(src), Path::new(dst)),
            _ => usage("sync needs a source and a destination directory"),
        },
        Some("resume-session") => match (args.get(1), args.get(2)) {
            (Some(src), Some(dst)) => sync::run_resume(Path::new(src), Path::new(dst)),
            _ => usage("resume-session needs a source and a destination directory"),
        },
        Some("fixture") => match (args.get(1), args.get(2)) {
            (Some(f), Some(mb)) => cmd_fixture(Path::new(f), mb.parse().unwrap_or(45)),
            _ => usage("fixture needs a path and a size in MB"),
        },
        _ => usage("unknown command"),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(msg: &str) -> Result<()> {
    eprintln!("psdev: {msg}\n");
    eprintln!("  psdev scan <dir> [db]   catalogue and hash a directory as a library");
    eprintln!("  psdev status [db]       counters and the next page of candidates");
    eprintln!("  psdev resume <file>     prove resumed hashing matches a single pass");
    eprintln!("  psdev collide <dir>     write a quick-key collision pair");
    eprintln!("  psdev selfcheck         run the engine self check");
    eprintln!("  psdev transfer <file> [clean|cut|corrupt|all]");
    eprintln!("                          drive a real file through the chunk pipeline");
    eprintln!("  psdev fixture <file> <mb>");
    eprintln!("                          write a pseudorandom test file");
    eprintln!("  psdev pairing           exercise the code-bound auth properties");
    eprintln!("  psdev handshake         real TCP + TLS 1.3 handshake, incl. a MITM attempt");
    eprintln!("  psdev sync <src> <dst>  full end-to-end sync between two libraries");
    eprintln!("  psdev resume-session <src> <dst>");
    eprintln!("                          cut a sync mid-asset, reconnect with no code, resume");
    std::process::exit(2);
}

fn db_path(arg: Option<&String>) -> PathBuf {
    arg.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("photosync-dev.db"))
}

// ---------------------------------------------------------------------------

/// Catalogues a directory, then drains the hashing queue exactly as §15.3
/// describes: `WHERE quick_hash IS NULL`, in batches, newest first.
fn cmd_scan(dir: &Path, db_file: PathBuf) -> Result<()> {
    let conn = db::open(&db_file)?;
    println!("db      : {}", db_file.display());
    println!("scanning: {}", dir.display());

    let mut seen = 0usize;
    let mut fresh = 0usize;
    let mut changed = 0usize;

    // One transaction for the whole catalogue pass. On a real library this
    // would be chunked; a dev directory is small.
    conn.execute_batch("BEGIN")?;
    for entry in walk(dir) {
        let Some(asset) = describe(&entry) else {
            continue;
        };
        let before_new = !catalog::was_catalogued(&conn, &asset.platform_asset_id)?;
        let outcome = catalog::upsert(&conn, &asset)?;
        seen += 1;
        if before_new {
            fresh += 1;
        } else if outcome.needs_hash {
            // Existing row whose content markers moved: §15.2 reset its hashes.
            changed += 1;
        }
    }
    conn.execute_batch("COMMIT")?;
    println!("catalogued: {seen} files ({fresh} new, {changed} changed since last scan)");

    // Drain the hash queue.
    let mut hashed = 0usize;
    loop {
        let batch = catalog::pending_hashes(&conn, 512)?;
        if batch.is_empty() {
            break;
        }
        conn.execute_batch("BEGIN")?;
        for pending in &batch {
            let mut file = BufReader::new(File::open(&pending.platform_asset_id)?);
            let quick = identity::quick_hash(pending.size, &mut file)?;
            catalog::set_quick_hash(&conn, pending.id, &quick)?;
            hashed += 1;
        }
        conn.execute_batch("COMMIT")?;
    }
    println!("hashed    : {hashed} files");

    report(&conn)
}

fn cmd_status(db_file: PathBuf) -> Result<()> {
    let conn = db::open(&db_file)?;
    println!("db      : {}", db_file.display());
    report(&conn)
}

fn report(conn: &rusqlite::Connection) -> Result<()> {
    let fp = Hash32::from_bytes(DEV_PEER_FP);
    let peer_id = catalog::upsert_peer(conn, &fp, Some("psdev peer"), Some("desktop"))?;

    for include_videos in [true, false] {
        let filter = FilterArgs {
            peer_id,
            include_videos,
        };
        let counters = catalog::counters(conn, filter)?;
        // §12.1 invariant: remaining must equal the candidate count for the
        // same filters, or progress can never reach 100%.
        let page = catalog::candidates(conn, filter, 10_000)?;
        let agree = counters.remaining as usize == page.len();

        println!(
            "\nvideos={include_videos:<5} total={} hashing={} remaining={} not_on_device={}  \
             candidates={} {}",
            counters.total,
            counters.hashing,
            counters.remaining,
            counters.not_on_device,
            page.len(),
            if agree { "OK" } else { "MISMATCH" }
        );

        for c in page.iter().take(5) {
            let name = Path::new(&c.platform_asset_id)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            println!(
                "  {:<28} {:>10} B  {:<5} quick={} full={}",
                truncate(&name, 28),
                c.descriptor.size,
                c.descriptor.media_type,
                c.descriptor.quick_hash.short(),
                c.descriptor
                    .full_hash
                    .map(|h| h.short())
                    .unwrap_or_else(|| "-".into())
            );
        }
        if page.len() > 5 {
            println!("  ... and {} more", page.len() - 5);
        }
    }
    Ok(())
}

/// Exercises the claims §9.3 and §9.4 make, rather than trusting the comments.
///
/// Each case below corresponds to a property the design asserts. If any prints
/// WRONG, the pairing story is broken regardless of what the spec says.
fn cmd_pairing() -> Result<()> {
    let sender_fp = Hash32::from_bytes([0x11; 32]);
    let receiver_fp = Hash32::from_bytes([0x22; 32]);
    let attacker_fp = Hash32::from_bytes([0x99; 32]);

    let check = |label: &str, expected_ok: bool, got_ok: bool| {
        println!(
            "{:<46} {:<9} {}",
            label,
            if got_ok { "accepted" } else { "rejected" },
            if got_ok == expected_ok { "OK" } else { "WRONG" }
        );
    };

    // --- the honest case -----------------------------------------------------
    let mut issued = CodeSession::issue(0);
    let code = PairingCode::parse(issued.code().as_str()).expect("well formed");
    println!("code displayed : {}\n", issued.code().display_grouped());

    let challenge = AuthChallenge::new(sender_fp, receiver_fp);
    let sender_proof = challenge.proof(&code, ProofRole::Sender);
    check(
        "honest sender proof",
        true,
        issued
            .verify(&challenge, ProofRole::Sender, &sender_proof, 0)
            .is_ok(),
    );

    // --- man in the middle ---------------------------------------------------
    // The attacker terminates TLS, so the receiver computes the transcript with
    // the attacker's fingerprint while the real sender used the receiver's.
    // §9.3 claims this cannot verify.
    let victim_view = AuthChallenge::from_nonce(challenge.nonce, sender_fp, receiver_fp);
    let mitm_view = AuthChallenge::from_nonce(challenge.nonce, sender_fp, attacker_fp);
    let proof_to_mitm = mitm_view.proof(&code, ProofRole::Sender);
    check(
        "MITM proof replayed to the real receiver",
        false,
        victim_view
            .verify(&code, ProofRole::Sender, &proof_to_mitm)
            .is_ok(),
    );

    // --- role confusion ------------------------------------------------------
    // A receiver's proof must not pass as a sender's, or it could be reflected.
    let receiver_proof = challenge.proof(&code, ProofRole::Receiver);
    check(
        "receiver proof reflected as sender proof",
        false,
        challenge
            .verify(&code, ProofRole::Sender, &receiver_proof)
            .is_ok(),
    );

    // --- cross-session replay ------------------------------------------------
    let other = AuthChallenge::new(sender_fp, receiver_fp);
    check(
        "proof from another session (fresh nonce)",
        false,
        other.verify(&code, ProofRole::Sender, &sender_proof).is_ok(),
    );

    // --- attempt budget ------------------------------------------------------
    // §9.4: three attempts total, global, never restored by a success.
    println!();
    let mut budget = CodeSession::issue(0);
    let real = PairingCode::parse(budget.code().as_str()).expect("well formed");
    let ch = AuthChallenge::new(sender_fp, receiver_fp);
    let wrong = ch.proof(
        &PairingCode::parse(&format!("{:06}", 1 + real.as_str().parse::<u32>().unwrap() % 999_999))
            .expect("well formed"),
        ProofRole::Sender,
    );

    for attempt in 1..=4 {
        let outcome = budget.verify(&ch, ProofRole::Sender, &wrong, 0);
        println!(
            "wrong proof #{attempt:<32} {:<9} attempts_left={} live={}",
            if outcome.is_ok() { "accepted" } else { "rejected" },
            budget.attempts_left(),
            budget.is_live(0)
        );
    }
    println!(
        "{:<46} {}",
        "code spent after 3 failures",
        if budget.is_live(0) { "WRONG" } else { "OK" }
    );

    // A correct proof after the budget is gone must still fail.
    let good = ch.proof(&real, ProofRole::Sender);
    check(
        "correct proof after budget exhausted",
        false,
        budget.verify(&ch, ProofRole::Sender, &good, 0).is_ok(),
    );

    // --- success does not restore the budget --------------------------------
    println!();
    let mut mixed = CodeSession::issue(0);
    let mixed_code = PairingCode::parse(mixed.code().as_str()).expect("well formed");
    let mch = AuthChallenge::new(sender_fp, receiver_fp);
    let bad = Hash32::from_bytes([0x00; 32]);
    let _ = mixed.verify(&mch, ProofRole::Sender, &bad, 0);
    let left_after_failure = mixed.attempts_left();
    let _ = mixed.verify(
        &mch,
        ProofRole::Sender,
        &mch.proof(&mixed_code, ProofRole::Sender),
        0,
    );
    println!(
        "{:<46} {} -> {} {}",
        "success must not restore the budget",
        left_after_failure,
        mixed.attempts_left(),
        if mixed.attempts_left() == left_after_failure {
            "OK"
        } else {
            "WRONG"
        }
    );

    // --- expiry --------------------------------------------------------------
    let mut aged = CodeSession::issue(0);
    let aged_code = PairingCode::parse(aged.code().as_str()).expect("well formed");
    let ach = AuthChallenge::new(sender_fp, receiver_fp);
    let aged_proof = ach.proof(&aged_code, ProofRole::Sender);
    let past_ttl = photosync_core::pairing::CODE_TTL_MS;
    check(
        "correct proof exactly at expiry",
        false,
        aged.verify(&ach, ProofRole::Sender, &aged_proof, past_ttl)
            .is_ok(),
    );

    // --- input handling ------------------------------------------------------
    println!();
    for (input, want) in [
        ("482913", true),
        ("482 913", true),
        ("48291", false),
        ("4829134", false),
        ("48291a", false),
        ("000000", true),
    ] {
        let got = PairingCode::parse(input).is_some();
        println!(
            "parse {:<40} {:<9} {}",
            format!("{input:?}"),
            if got { "accepted" } else { "rejected" },
            if got == want { "OK" } else { "WRONG" }
        );
    }
    Ok(())
}

/// Writes a pseudorandom fixture of `mb` megabytes.
///
/// Deliberately not zero-filled. A file of identical bytes gives every chunk the
/// same hash, which would let a misaligned or duplicated chunk pass unnoticed and
/// makes the corruption test far weaker than it looks. The generator is a plain
/// xorshift so a given size always produces the same bytes and results stay
/// comparable between runs and between machines.
fn cmd_fixture(path: &Path, mb: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let total = mb * 1024 * 1024;
    let mut file = File::create(path)?;

    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut buf = vec![0u8; 1 << 20];
    let mut written = 0u64;
    while written < total {
        for slot in buf.chunks_exact_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            slot.copy_from_slice(&state.to_le_bytes());
        }
        let want = ((total - written) as usize).min(buf.len());
        file.write_all(&buf[..want])?;
        written += want as u64;
    }
    file.sync_all()?;
    println!("wrote {} ({} B)", path.display(), total);
    Ok(())
}

/// Drives a real file through the chunk pipeline, with deliberate faults.
///
/// This is what replaces unit tests for §13: the conditions that decide success
/// criteria 3 and 9 cannot be produced by asking a real network nicely, so they
/// are injected here and the engine is otherwise the real one.
///
/// Modes: `clean` (default), `cut` (drop the connection mid-asset and resume),
/// `corrupt` (flip a bit in one chunk), `all`.
fn cmd_transfer(file: &Path, mode: Option<&str>) -> Result<()> {
    let mode = mode.unwrap_or("all");
    let size = fs::metadata(file)?.len();
    println!("file  : {} ({} B)", file.display(), size);
    println!("chunks: {} × {} B", chunk::chunk_count(size), chunk::CHUNK_LEN);

    // The truth we must reproduce, computed independently.
    let expected = {
        let mut h = FullHasher::new();
        let mut f = File::open(file)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        h.finish().expect("non-empty")
    };
    println!("truth : {expected}\n");

    if mode == "clean" || mode == "all" {
        run_clean(file, size, expected)?;
    }
    if mode == "cut" || mode == "all" {
        run_cut(file, size, expected)?;
    }
    if mode == "corrupt" || mode == "all" {
        run_corrupt(file, size, expected)?;
    }
    if mode == "misaligned" || mode == "all" {
        run_misaligned(file, size)?;
    }
    Ok(())
}

/// A chunk that does not start where the receiver expects must be refused,
/// not written at the offset it claims.
///
/// This is the failure mode where a sender reconnects believing it is further
/// ahead than the receiver. Writing such a chunk would leave a hole that every
/// per-chunk hash still passes, and only the whole-file hash would catch it —
/// after the entire asset had been transferred.
fn run_misaligned(file: &Path, size: u64) -> Result<()> {
    let (path, staging) = staging_for("misaligned")?;
    let mut out = OutboundAsset::begin(File::open(file)?, size)?;
    let mut inb = InboundAsset::begin(staging, size)?;

    // Accept the first chunk normally.
    let first = out.next_chunk()?.expect("at least one chunk");
    let _ = inb.accept_chunk(&first)?;

    // Now skip one: offer chunk 3 while the receiver is waiting for chunk 1.
    let _skipped = out.next_chunk()?;
    let ahead = out.next_chunk()?.expect("third chunk");

    let verdict = inb.accept_chunk(&ahead)?;
    let expected_offset = inb.resume_offset();
    let len_after = fs::metadata(&path)?.len();

    let ok = matches!(
        verdict,
        ChunkVerdict::Misaligned { expected_offset: e } if e == expected_offset
    ) && len_after == expected_offset;

    println!(
        "misalign: offered offset {} while expecting {} -> {:?}",
        ahead.offset, expected_offset, verdict
    );
    println!(
        "          staging still {len_after} B, no hole written — {}",
        if ok { "OK" } else { "WRONG" }
    );
    fs::remove_file(path).ok();
    Ok(())
}

fn staging_for(label: &str) -> Result<(PathBuf, FileStaging)> {
    let dir = std::env::temp_dir().join("psdev-staging");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{label}.part"));
    let staging = FileStaging::create(&path)?;
    Ok((path, staging))
}

/// Uninterrupted transfer.
fn run_clean(file: &Path, size: u64, expected: Hash32) -> Result<()> {
    let (path, staging) = staging_for("clean")?;
    let mut out = OutboundAsset::begin(File::open(file)?, size)?;
    let mut inb = InboundAsset::begin(staging, size)?;

    let mut n = 0u64;
    while let Some(chunk) = out.next_chunk()? {
        match inb.accept_chunk(&chunk)? {
            ChunkVerdict::Accepted { .. } => n += 1,
            other => {
                println!("clean   : UNEXPECTED {other:?}");
                return Ok(());
            }
        }
    }
    let declared = out.full_hash().expect("complete");
    let (verdict, _) = inb.finish(declared)?;
    println!(
        "clean   : {n} chunks, {}",
        verdict_summary(&verdict, expected, declared)
    );
    fs::remove_file(path).ok();
    Ok(())
}

/// Interrupt at roughly 60% of the asset, then reconnect and resume.
///
/// This is success criterion 3 without needing a phone or a Wi-Fi router.
fn run_cut(file: &Path, size: u64, expected: Hash32) -> Result<()> {
    let (path, staging) = staging_for("cut")?;
    let cut_after = ((chunk::chunk_count(size) * 3) / 5).max(1);

    // First leg: send some chunks, persist the offset as the receiver would,
    // then drop everything on the floor.
    let mut persisted = 0u64;
    {
        let mut out = OutboundAsset::begin(File::open(file)?, size)?;
        let mut inb = InboundAsset::begin(staging, size)?;
        for _ in 0..cut_after {
            let Some(chunk) = out.next_chunk()? else { break };
            if let ChunkVerdict::Accepted { bytes_received } = inb.accept_chunk(&chunk)? {
                // The caller persists only after Accepted. Same ordering the
                // real receiver uses.
                persisted = bytes_received;
            }
        }
        // Simulate an unclean drop: writer and hasher are lost, only the
        // persisted offset and the file on disk survive.
    }

    // Simulate trailing bytes that were written but never flushed or
    // acknowledged. Resume must discard them.
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.seek(SeekFrom::End(0))?;
        f.write_all(b"garbage-past-the-last-ack")?;
    }
    let dirty_len = fs::metadata(&path)?.len();

    // Second leg: reconnect. Receiver is authoritative for the offset.
    let staging = FileStaging::create(&path)?;
    let mut inb = InboundAsset::resume(staging, size, persisted)?;
    let offset = inb.resume_offset();
    let mut out = OutboundAsset::resume(File::open(file)?, size, offset)?;

    let mut n = 0u64;
    while let Some(chunk) = out.next_chunk()? {
        match inb.accept_chunk(&chunk)? {
            ChunkVerdict::Accepted { .. } => n += 1,
            other => {
                println!("cut     : UNEXPECTED {other:?}");
                return Ok(());
            }
        }
    }
    let declared = out.full_hash().expect("complete");
    let (verdict, _) = inb.finish(declared)?;

    println!(
        "cut     : dropped at {} B of {} B ({:.0}%), staging had {} B of junk appended",
        persisted,
        size,
        (persisted as f64 / size as f64) * 100.0,
        dirty_len - persisted
    );
    println!(
        "          resumed at {offset} B, {n} more chunks, {}",
        verdict_summary(&verdict, expected, declared)
    );
    fs::remove_file(path).ok();
    Ok(())
}

/// Flip a bit in one chunk and confirm only that chunk is retried.
///
/// Success criterion 9. TCP makes this essentially unreproducible in the wild,
/// which is exactly why the retry path would otherwise never run before a user
/// hit it.
fn run_corrupt(file: &Path, size: u64, expected: Hash32) -> Result<()> {
    let (path, staging) = staging_for("corrupt")?;
    let mut out = OutboundAsset::begin(File::open(file)?, size)?;
    let mut inb = InboundAsset::begin(staging, size)?;

    let victim = chunk::chunk_count(size) / 2;
    let mut index = 0u64;
    let mut accepted = 0u64;
    let mut retries = 0u64;

    while let Some(chunk) = out.next_chunk()? {
        if index == victim {
            // Corrupt a copy, exactly as a flipped bit on the wire would.
            let mut bad = chunk.clone();
            if let Some(b) = bad.payload.get_mut(0) {
                *b ^= 0x01;
            }
            match inb.accept_chunk(&bad)? {
                ChunkVerdict::Corrupt => retries += 1,
                other => {
                    println!("corrupt : UNEXPECTED, corruption not detected: {other:?}");
                    return Ok(());
                }
            }
            // Nothing was written, so the offset did not move. Resend the
            // original chunk, not a freshly read one.
            if !matches!(
                inb.accept_chunk(&chunk)?,
                ChunkVerdict::Accepted { .. }
            ) {
                println!("corrupt : UNEXPECTED, retry of a good chunk was refused");
                return Ok(());
            }
            accepted += 1;
        } else {
            match inb.accept_chunk(&chunk)? {
                ChunkVerdict::Accepted { .. } => accepted += 1,
                other => {
                    println!("corrupt : UNEXPECTED {other:?}");
                    return Ok(());
                }
            }
        }
        index += 1;
    }

    let declared = out.full_hash().expect("complete");
    let (verdict, _) = inb.finish(declared)?;
    println!(
        "corrupt : chunk {victim} corrupted, {retries} rejected, {accepted} accepted, {}",
        verdict_summary(&verdict, expected, declared)
    );
    println!(
        "          retried {} B instead of the whole {} B asset",
        chunk::CHUNK_LEN.min(size as usize),
        size
    );
    fs::remove_file(path).ok();
    Ok(())
}

fn verdict_summary(verdict: &FinishVerdict, expected: Hash32, declared: Hash32) -> String {
    match verdict {
        FinishVerdict::Verified { full_hash } if *full_hash == expected && declared == expected => {
            "VERIFIED and matches the independently computed hash".to_string()
        }
        FinishVerdict::Verified { full_hash } => {
            format!("verified against the sender but WRONG: {full_hash} != {expected}")
        }
        other => format!("FAILED: {other:?}"),
    }
}

/// Runs the same self check the native library exposes, so the desktop result
/// can be compared against what a device reports.
fn cmd_selfcheck() -> Result<()> {
    let check = photosync_core::bringup::self_check()?;
    println!("schema_version : {}", check.schema_version);
    println!("candidates     : {}", check.candidates);
    println!("remaining      : {}", check.remaining);
    println!(
        "result         : {}",
        if check.is_ok() { "OK" } else { "FAILED" }
    );
    Ok(())
}

/// Marks the single newest candidate as sent, then re-reports.
///
/// Simulates one completed transfer so repeat-sync behaviour can be inspected
/// without a peer. Exactly one asset should leave the candidate set.
fn cmd_sent(db_file: PathBuf) -> Result<()> {
    let conn = db::open(&db_file)?;
    let fp = Hash32::from_bytes(DEV_PEER_FP);
    let peer_id = catalog::upsert_peer(&conn, &fp, Some("psdev peer"), Some("desktop"))?;
    let filter = FilterArgs {
        peer_id,
        include_videos: true,
    };

    let before = catalog::candidates(&conn, filter, 10_000)?;
    let Some(first) = before.first() else {
        println!("no candidates left");
        return Ok(());
    };

    // Compute the full hash the way a real transfer would: streaming, one pass.
    let mut h = FullHasher::new();
    let mut f = File::open(&first.platform_asset_id)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let full = h.finish().expect("non-empty");

    catalog::set_full_hash(&conn, first.id, &full)?;
    catalog::mark_sent(&conn, peer_id, &full, &first.descriptor.quick_hash)?;

    let after = catalog::candidates(&conn, filter, 10_000)?;
    println!(
        "marked sent: {}  quick={} full={}",
        Path::new(&first.platform_asset_id)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        first.descriptor.quick_hash.short(),
        full.short()
    );
    println!(
        "candidates : {} -> {}  (expected {} -> {})",
        before.len(),
        after.len(),
        before.len(),
        before.len() - 1
    );
    if after.len() != before.len() - 1 {
        println!(
            "WRONG: {} asset(s) left the candidate set instead of 1",
            before.len() - after.len()
        );
    }
    report(&conn)
}

/// Proves the resume path (§13.4, §26.8): hashing a file in one pass and
/// rebuilding the digest from a truncated prefix then continuing must agree.
fn cmd_resume(file: &Path) -> Result<()> {
    let size = fs::metadata(file)?.len();
    if size < 8 {
        eprintln!("file too small to be interesting");
        return Ok(());
    }

    // Single pass, as a normal uninterrupted transfer would do.
    let mut whole = FullHasher::new();
    let mut f = File::open(file)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
    }
    let expected = whole.finish().expect("non-empty");

    // Interrupted at an arbitrary boundary, then resumed.
    let cut = size / 3;
    let mut f = File::open(file)?;
    let mut resumed = FullHasher::resume_from(&mut f, cut)?;
    f.seek(SeekFrom::Start(cut))?;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        resumed.update(&buf[..n]);
    }
    let actual = resumed.finish().expect("non-empty");

    println!("file     : {}", file.display());
    println!("size     : {size} B, resumed from {cut} B");
    println!("one pass : {expected}");
    println!("resumed  : {actual}");
    println!(
        "result   : {}",
        if expected == actual {
            "MATCH — resume produces the same digest"
        } else {
            "MISMATCH — resume is broken"
        }
    );
    Ok(())
}

/// Writes two distinct files sharing size, head, middle and tail, which is the
/// fixture success criterion 10 needs and which cannot be produced by accident.
fn cmd_collide(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let len = (QUICK_SAMPLE_LEN * 8) as usize;

    let make = |name: &str, filler: u8| -> Result<PathBuf> {
        let path = dir.join(name);
        let mut data = vec![0u8; len];
        // Head, middle and tail windows are identical in both files...
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // ...and only bytes strictly between the sampled windows differ.
        let head_end = QUICK_SAMPLE_LEN as usize;
        let mid_start = len / 2 - QUICK_SAMPLE_LEN as usize / 2;
        let gap_start = head_end + 16;
        let gap_end = mid_start - 16;
        for b in &mut data[gap_start..gap_end] {
            *b = filler;
        }
        File::create(&path)?.write_all(&data)?;
        Ok(path)
    };

    let a = make("collide_a.bin", 0x11)?;
    let b = make("collide_b.bin", 0x22)?;

    let qh = |p: &Path| -> Result<Hash32> {
        let size = fs::metadata(p)?.len();
        let mut f = File::open(p)?;
        Ok(identity::quick_hash(size, &mut f)?)
    };
    let fh = |p: &Path| -> Result<Hash32> {
        let mut h = FullHasher::new();
        let mut f = File::open(p)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(h.finish().expect("non-empty"))
    };

    let (qa, qb) = (qh(&a)?, qh(&b)?);
    let (fa, fb) = (fh(&a)?, fh(&b)?);

    println!("wrote {} and {}", a.display(), b.display());
    println!("size      : {len} B each");
    println!("quick  A  : {qa}");
    println!("quick  B  : {qb}");
    println!("full   A  : {fa}");
    println!("full   B  : {fb}");
    println!();
    println!(
        "quick keys {} — {}",
        if qa == qb { "COLLIDE" } else { "differ" },
        if qa == qb {
            "this is the case §11.3 must survive: a quick match may only mean 'probable'"
        } else {
            "collision fixture failed to reproduce the overlap"
        }
    );
    println!(
        "full hashes {} — the authority still separates them",
        if fa == fb { "COLLIDE (bad)" } else { "differ" }
    );
    Ok(())
}

// ---------------------------------------------------------------------------

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => out.push(p),
                _ => {}
            }
        }
    }
    out
}

/// Builds a [`ScannedAsset`] from a real file. The absolute path stands in for
/// the platform asset id, which is exactly how it is used: a local key only.
fn describe(path: &Path) -> Option<ScannedAsset> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let (media_type, mime) = match ext.as_str() {
        "jpg" | "jpeg" => (MediaType::Image, "image/jpeg"),
        "png" => (MediaType::Image, "image/png"),
        "heic" | "heif" => (MediaType::Image, "image/heic"),
        "webp" => (MediaType::Image, "image/webp"),
        "gif" => (MediaType::Image, "image/gif"),
        "mp4" | "m4v" => (MediaType::Video, "video/mp4"),
        "mov" => (MediaType::Video, "video/quicktime"),
        "bin" => (MediaType::Image, "application/octet-stream"),
        _ => return None,
    };

    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Some(ScannedAsset {
        platform_asset_id: path.to_string_lossy().into_owned(),
        size: meta.len(),
        media_type,
        mime: mime.to_string(),
        created_at: modified,
        modified_at: modified,
        width: None,
        height: None,
        duration_ms: None,
        display_name: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned()),
        resource_group_id: None,
        is_local: true,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}
