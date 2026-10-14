//! On-disk snapshot of one volume's `Index` plus the USN journal position it
//! is current to, so a restart can load it and replay only what changed.
//!
//! The columns are written raw (little-endian, no per-entry parsing) and read
//! straight back into their `Vec`s, so loading is bounded by disk speed:
//!
//! ```text
//! header  96 B   magic "SRCHSNAP", version, journal id, next USN, saved-at,
//!                entry count, name-buffer sizes, orphan count
//! columns        lower, orig, lower_off, orig_off, parent, rec, flags
//! footer  16 B   checksum over header + columns, magic "SNAPEND\0"
//! ```
//!
//! Anything that does not check out (size, magic, version, checksum, column
//! invariants) is reported as `Corrupt`; the caller then rescans. Writes go to
//! a temp file that replaces the old snapshot only once complete.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use crate::index::{Index, OwnedParts, spare};

const _: () = assert!(cfg!(target_endian = "little"), "snapshots are little-endian");

const MAGIC: &[u8; 8] = b"SRCHSNAP";
const END_MAGIC: &[u8; 8] = b"SNAPEND\0";
/// Bump when the layout changes; older files are then rescanned, not parsed.
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 96;
const FOOTER_LEN: usize = 16;
const HASH_CHUNK: usize = 4 << 20;

/// Where a snapshot's index is current to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub journal_id: u64,
    /// USN to resume reading the journal from.
    pub next_usn: i64,
    /// Seconds since the Unix epoch when the snapshot was written.
    pub saved_unix: u64,
}

impl Meta {
    pub fn now(journal_id: u64, next_usn: i64) -> Meta {
        Meta {
            journal_id,
            next_usn,
            saved_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    /// How long ago the snapshot was written (zero if the clock went back).
    pub fn age(&self) -> std::time::Duration {
        let saved = UNIX_EPOCH + std::time::Duration::from_secs(self.saved_unix);
        SystemTime::now().duration_since(saved).unwrap_or_default()
    }
}

#[derive(Debug)]
pub enum LoadError {
    Missing,
    Corrupt(String),
    Io(io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Missing => write!(f, "no snapshot"),
            LoadError::Corrupt(why) => write!(f, "snapshot unusable: {why}"),
            LoadError::Io(e) => write!(f, "snapshot unreadable: {e}"),
        }
    }
}

/// Path of the snapshot for a volume key such as `"C"`.
pub fn path_for(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.snap"))
}

fn u32s_as_bytes(v: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding or invalid bit patterns; the byte view covers
    // exactly the same memory and u8 has alignment 1.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

fn u32s_as_bytes_mut(v: &mut [u32]) -> &mut [u8] {
    // SAFETY: as above; every bit pattern is a valid u32.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), std::mem::size_of_val(v)) }
}

fn chunk_hash(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = 0x243F_6A88_85A3_08D3 ^ b.len() as u64;
    let (words, rest) = b.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(K).rotate_left(29);
    }
    for &x in rest {
        h = (h ^ x as u64).wrapping_mul(K);
    }
    h ^ (h >> 32)
}

fn mix(acc: u64, h: u64) -> u64 {
    (acc ^ h).wrapping_mul(0xFF51_AFD7_ED55_8CCD).rotate_left(31)
}

/// Checksum of a byte range: independent per-chunk hashes (so big columns
/// hash in parallel) folded in order.
fn section_hash(b: &[u8]) -> u64 {
    let parts: Vec<u64> = b.par_chunks(HASH_CHUNK).map(chunk_hash).collect();
    parts.into_iter().fold(chunk_hash(&[]), mix)
}

fn checksum(header: &[u8], columns: &[&[u8]]) -> u64 {
    columns
        .iter()
        .fold(chunk_hash(header), |acc, c| mix(acc, section_hash(c)))
}

fn header_bytes(meta: &Meta, n: usize, lower_len: usize, orig_len: usize, orphans: usize) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h[16..24].copy_from_slice(&meta.journal_id.to_le_bytes());
    h[24..32].copy_from_slice(&meta.next_usn.to_le_bytes());
    h[32..40].copy_from_slice(&meta.saved_unix.to_le_bytes());
    h[40..48].copy_from_slice(&(n as u64).to_le_bytes());
    h[48..56].copy_from_slice(&(lower_len as u64).to_le_bytes());
    h[56..64].copy_from_slice(&(orig_len as u64).to_le_bytes());
    h[64..72].copy_from_slice(&(orphans as u64).to_le_bytes());
    h
}

fn le64(h: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(h[at..at + 8].try_into().unwrap())
}

/// Write `ix` (minus any dead entries) to `path`. Returns the file size.
pub fn save(path: &Path, meta: &Meta, ix: &Index) -> io::Result<u64> {
    let squeezed;
    let ix = if ix.dead_len() > 0 {
        squeezed = ix.compacted();
        &squeezed
    } else {
        ix
    };
    let p = ix.parts();
    let columns: [&[u8]; 7] = [
        p.lower,
        p.orig,
        u32s_as_bytes(p.lower_off),
        u32s_as_bytes(p.orig_off),
        u32s_as_bytes(p.parent),
        u32s_as_bytes(p.rec),
        p.flags,
    ];
    let header = header_bytes(meta, p.parent.len(), p.lower.len(), p.orig.len(), p.orphans);
    let sum = checksum(&header, &columns);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(&header)?;
        for c in columns {
            f.write_all(c)?;
        }
        f.write_all(&sum.to_le_bytes())?;
        f.write_all(END_MAGIC)?;
        f.flush()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    fs::metadata(path).map(|m| m.len())
}

fn read_exact_into(f: &mut File, buf: &mut [u8]) -> Result<(), LoadError> {
    f.read_exact(buf).map_err(LoadError::Io)
}

/// Columns are read with a little spare capacity so the first live edits
/// do not have to reallocate (and copy) hundreds of MB.
fn read_u32s(f: &mut File, n: usize) -> Result<Vec<u32>, LoadError> {
    let mut v = Vec::with_capacity(n + spare(n));
    v.resize(n, 0u32);
    read_exact_into(f, u32s_as_bytes_mut(&mut v))?;
    Ok(v)
}

fn read_bytes(f: &mut File, n: usize) -> Result<Vec<u8>, LoadError> {
    let mut v = Vec::with_capacity(n + spare(n));
    v.resize(n, 0u8);
    read_exact_into(f, &mut v)?;
    Ok(v)
}

/// Read and fully validate a snapshot.
pub fn load(path: &Path) -> Result<(Meta, Index), LoadError> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(e) => return Err(LoadError::Io(e)),
    };
    let file_len = f.metadata().map_err(LoadError::Io)?.len();
    let bad = |why: &str| LoadError::Corrupt(why.to_string());
    if file_len < (HEADER_LEN + FOOTER_LEN) as u64 {
        return Err(bad("file too short"));
    }
    let mut header = [0u8; HEADER_LEN];
    read_exact_into(&mut f, &mut header)?;
    if &header[0..8] != MAGIC {
        return Err(bad("not a snapshot"));
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(LoadError::Corrupt(format!(
            "format version {version}, expected {VERSION}"
        )));
    }
    let n = le64(&header, 40);
    let lower_len = le64(&header, 48);
    let orig_len = le64(&header, 56);
    let orphans = le64(&header, 64);
    if n >= u32::MAX as u64 || lower_len >= u32::MAX as u64 || orig_len >= u32::MAX as u64 {
        return Err(bad("sizes out of range"));
    }
    // Everything is sized from the header, so check it against the real file
    // length before allocating anything.
    let expected = HEADER_LEN as u64
        + lower_len
        + orig_len
        + 4 * 2 * (n + 1)
        + 4 * 2 * n
        + n
        + FOOTER_LEN as u64;
    if expected != file_len {
        return Err(bad("size does not match header (truncated?)"));
    }
    let (n, lower_len, orig_len) = (n as usize, lower_len as usize, orig_len as usize);

    let lower = read_bytes(&mut f, lower_len)?;
    let orig = read_bytes(&mut f, orig_len)?;
    let lower_off = read_u32s(&mut f, n + 1)?;
    let orig_off = read_u32s(&mut f, n + 1)?;
    let parent = read_u32s(&mut f, n)?;
    let rec = read_u32s(&mut f, n)?;
    let flags = read_bytes(&mut f, n)?;
    let mut footer = [0u8; FOOTER_LEN];
    read_exact_into(&mut f, &mut footer)?;
    if &footer[8..16] != END_MAGIC {
        return Err(bad("missing end marker"));
    }
    let want = u64::from_le_bytes(footer[0..8].try_into().unwrap());
    let got = checksum(
        &header,
        &[
            &lower,
            &orig,
            u32s_as_bytes(&lower_off),
            u32s_as_bytes(&orig_off),
            u32s_as_bytes(&parent),
            u32s_as_bytes(&rec),
            &flags,
        ],
    );
    if want != got {
        return Err(bad("checksum mismatch"));
    }

    let meta = Meta {
        journal_id: le64(&header, 16),
        next_usn: le64(&header, 24) as i64,
        saved_unix: le64(&header, 32),
    };
    let index = Index::from_parts(OwnedParts {
        lower,
        orig,
        lower_off,
        orig_off,
        parent,
        rec,
        flags,
        orphans: orphans as usize,
    })
    .map_err(LoadError::Corrupt)?;
    Ok((meta, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Builder, QueryOpts};

    fn sample() -> Index {
        let mut b = Builder::new();
        b.begin_volume("C:", 5);
        b.push(10, 5, "Users", true);
        b.push(11, 10, "Zach", true);
        b.push(12, 11, "Résumé Final.PDF", false);
        b.push(13, 11, "notes.txt", false);
        b.push(16, 999, "lost.txt", false);
        b.build()
    }

    fn all_paths(ix: &Index) -> Vec<String> {
        let r = ix.search(
            "",
            QueryOpts {
                limit: usize::MAX,
                ..Default::default()
            },
        );
        r.hits.iter().map(|&i| ix.path(i)).collect()
    }

    #[test]
    fn roundtrip_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "C");
        let mut ix = sample();
        ix.upsert(20, 11, "later.txt", false);
        ix.remove(13); // saved without the dead entry
        let meta = Meta::now(0xABCD, 123_456);
        let bytes = save(&path, &meta, &ix).unwrap();
        assert_eq!(bytes, fs::metadata(&path).unwrap().len());

        let (m2, back) = load(&path).unwrap();
        assert_eq!(m2, meta);
        assert_eq!(back.dead_len(), 0);
        assert_eq!(back.orphans(), ix.orphans());
        let mut want = all_paths(&ix);
        let mut got = all_paths(&back);
        want.sort();
        got.sort();
        assert_eq!(got, want);
        // The lookup table works after loading: edits still apply.
        let mut back = back;
        assert!(back.find_record(20).is_some());
        assert!(back.remove(20));
    }

    #[test]
    fn replaces_an_existing_snapshot_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "C");
        save(&path, &Meta::now(1, 10), &sample()).unwrap();
        save(&path, &Meta::now(2, 20), &sample()).unwrap();
        assert_eq!(load(&path).unwrap().0.journal_id, 2);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn missing_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&path_for(dir.path(), "Z")),
            Err(LoadError::Missing)
        ));
    }

    #[test]
    fn damage_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "C");
        save(&path, &Meta::now(1, 10), &sample()).unwrap();
        let good = fs::read(&path).unwrap();

        let corrupt = |bytes: &[u8]| {
            fs::write(&path, bytes).unwrap();
            matches!(load(&path), Err(LoadError::Corrupt(_)))
        };
        // Truncated at various points.
        for cut in [0, 10, HEADER_LEN, good.len() / 2, good.len() - 1] {
            assert!(corrupt(&good[..cut]), "cut at {cut}");
        }
        // One flipped bit anywhere past the magic is caught by the checksum
        // (or an earlier structural check).
        for at in [9, 17, 30, HEADER_LEN + 3, good.len() / 2, good.len() - 20] {
            let mut b = good.clone();
            b[at] ^= 0x01;
            assert!(corrupt(&b), "flip at {at}");
        }
        // Wrong version.
        let mut b = good.clone();
        b[8] = 99;
        assert!(corrupt(&b));
        // Trailing garbage.
        let mut b = good.clone();
        b.push(0);
        assert!(corrupt(&b));
        // The undamaged file still loads.
        fs::write(&path, &good).unwrap();
        assert!(load(&path).is_ok());
    }

    #[test]
    fn hostile_header_sizes_do_not_allocate() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "C");
        save(&path, &Meta::now(1, 10), &sample()).unwrap();
        let mut b = fs::read(&path).unwrap();
        b[40..48].copy_from_slice(&(u32::MAX as u64 - 1).to_le_bytes());
        fs::write(&path, &b).unwrap();
        assert!(matches!(load(&path), Err(LoadError::Corrupt(_))));
    }

    /// Cost of a restart on a volume the size of the author's C: drive.
    /// `cargo test --release -- --ignored --nocapture scale`
    #[test]
    #[ignore]
    fn scale_15m_entries() {
        use std::time::Instant;
        let n = 15_700_000u64;
        let exts = ["txt", "dll", "png", "json", "rs", "log", "tmp", "pdf"];
        let t = Instant::now();
        let mut b = Builder::new();
        b.reserve(n as usize, n as usize * 24);
        b.begin_volume("C:", 5);
        let mut dir = 5u64;
        for i in 0..n {
            let rec = 100 + i;
            if i % 20 == 0 {
                b.push(rec, dir.max(5) - (i % 7).min(dir.saturating_sub(5)), &format!("Folder_{i:x}"), true);
                dir = rec;
            } else {
                b.push(
                    rec,
                    dir,
                    &format!("some-file-name-{i:x}-{:04}.{}", i % 9973, exts[(i % 8) as usize]),
                    false,
                );
            }
        }
        let ix = b.build();
        eprintln!("built {} entries in {:?}, {} MiB", ix.len(), t.elapsed(), ix.heap_bytes() >> 20);

        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "C");
        let t = Instant::now();
        let bytes = save(&path, &Meta::now(1, 1), &ix).unwrap();
        eprintln!("save: {} MiB in {:?}", bytes >> 20, t.elapsed());
        let t = Instant::now();
        let (_, back) = load(&path).unwrap();
        eprintln!("load: {:?} ({} entries)", t.elapsed(), back.len());
        let t = Instant::now();
        let r = back.search("some-file-name-abc", QueryOpts::default());
        eprintln!("search after load: {:?} ({} hits)", t.elapsed(), r.total);
    }
}
