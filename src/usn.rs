//! Parser for the buffers returned by `FSCTL_ENUM_USN_DATA` and
//! `FSCTL_READ_USN_JOURNAL`.
//!
//! Both hand back an 8-byte header (the next file reference number for
//! enumeration, the next USN for journal reads) followed by a packed run of
//! `USN_RECORD_V2` structures. Everything here is bounds-checked and
//! platform-independent so it can be unit-tested (and fuzzed) without a
//! volume handle.

use std::fmt;

/// `FILE_ATTRIBUTE_DIRECTORY`.
pub const ATTR_DIRECTORY: u32 = 0x10;

/// Only the low 48 bits of a file reference number are the MFT record number;
/// the top 16 bits are a reuse sequence number.
pub const FRN_RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// `USN_REASON_*` bits the live index cares about.
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// Journal reads are filtered to these in the kernel, so data writes and
/// attribute changes (the bulk of the traffic) never reach us.
pub const REASON_MASK: u32 = REASON_FILE_CREATE | REASON_FILE_DELETE | REASON_RENAME_NEW_NAME;

const HEADER_LEN: usize = 60; // fixed part of USN_RECORD_V2, up to the name
const OFF_MAJOR: usize = 4;
const OFF_FRN: usize = 8;
const OFF_PARENT: usize = 16;
const OFF_USN: usize = 24;
const OFF_REASON: usize = 40;
const OFF_ATTRS: usize = 52;
const OFF_NAME_LEN: usize = 56;
const OFF_NAME_OFF: usize = 58;

/// One decoded record. `name` is raw UTF-16LE bytes borrowed from the buffer
/// (not guaranteed 2-byte aligned, so we never reinterpret it).
#[derive(Debug, Clone, Copy)]
pub struct UsnEntry<'a> {
    pub record: u64,
    pub parent: u64,
    /// This record's position in the journal.
    pub usn: i64,
    /// `USN_REASON_*` bits (always the cumulative set for journal records).
    pub reason: u32,
    pub attributes: u32,
    pub name_utf16le: &'a [u8],
}

impl UsnEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    /// Decode the name into `out` (replacing invalid surrogates with U+FFFD).
    pub fn decode_name(&self, out: &mut String) {
        out.clear();
        let units = self
            .name_utf16le
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]));
        out.extend(char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Buffer shorter than the 8-byte next-FRN header.
    TooShort,
    /// A record header was truncated, oversized, or internally inconsistent.
    Malformed { at: usize, why: &'static str },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::TooShort => write!(f, "USN buffer shorter than its header"),
            ParseError::Malformed { at, why } => {
                write!(f, "malformed USN record at offset {at}: {why}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Walk one `FSCTL_ENUM_USN_DATA` output buffer, calling `each` per V2 record.
/// Returns the file reference number to resume from on the next call.
///
/// Records with a major version other than 2 are skipped (V3/V4 appear on
/// ReFS, which this step does not target). Any structural problem aborts the
/// buffer with an error rather than guessing.
pub fn parse_enum_buffer<'a>(
    buf: &'a [u8],
    mut each: impl FnMut(UsnEntry<'a>),
) -> Result<u64, ParseError> {
    if buf.len() < 8 {
        return Err(ParseError::TooShort);
    }
    let next = u64_at(buf, 0);
    let mut pos = 8usize;
    while pos < buf.len() {
        let rest = &buf[pos..];
        if rest.len() < 4 {
            return Err(ParseError::Malformed {
                at: pos,
                why: "truncated length",
            });
        }
        let rec_len = u32_at(rest, 0) as usize;
        if rec_len < HEADER_LEN {
            return Err(ParseError::Malformed {
                at: pos,
                why: "record shorter than header",
            });
        }
        if rec_len > rest.len() {
            return Err(ParseError::Malformed {
                at: pos,
                why: "record runs past buffer",
            });
        }
        let rec = &rest[..rec_len];
        if u16_at(rec, OFF_MAJOR) == 2 {
            let name_len = u16_at(rec, OFF_NAME_LEN) as usize;
            let name_off = u16_at(rec, OFF_NAME_OFF) as usize;
            if !name_len.is_multiple_of(2) {
                return Err(ParseError::Malformed {
                    at: pos,
                    why: "odd UTF-16 name length",
                });
            }
            if name_off < HEADER_LEN || name_off.checked_add(name_len).is_none_or(|e| e > rec_len) {
                return Err(ParseError::Malformed {
                    at: pos,
                    why: "name outside record",
                });
            }
            each(UsnEntry {
                record: u64_at(rec, OFF_FRN) & FRN_RECORD_MASK,
                parent: u64_at(rec, OFF_PARENT) & FRN_RECORD_MASK,
                usn: u64_at(rec, OFF_USN) as i64,
                reason: u32_at(rec, OFF_REASON),
                attributes: u32_at(rec, OFF_ATTRS),
                name_utf16le: &rec[name_off..name_off + name_len],
            });
        }
        // Records are 8-byte aligned; RecordLength already includes padding,
        // but round up defensively so a bad length can't stall the loop.
        pos += (rec_len + 7) & !7;
    }
    Ok(next)
}

/// Walk one `FSCTL_READ_USN_JOURNAL` output buffer. Same record layout as
/// enumeration; the header is the USN to pass as `StartUsn` next time.
pub fn parse_journal_buffer<'a>(
    buf: &'a [u8],
    each: impl FnMut(UsnEntry<'a>),
) -> Result<i64, ParseError> {
    parse_enum_buffer(buf, each).map(|n| n as i64)
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Build a V2 record for tests.
    pub fn record(frn: u64, parent: u64, attrs: u32, name: &str, major: u16) -> Vec<u8> {
        journal_record(frn, parent, 0, 0, attrs, name, major)
    }

    /// Build a V2 record with journal fields for tests.
    pub fn journal_record(
        frn: u64,
        parent: u64,
        usn: i64,
        reason: u32,
        attrs: u32,
        name: &str,
        major: u16,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_len = units.len() * 2;
        let len = (60 + name_len + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&major.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    pub fn buffer(next: u64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut b = next.to_le_bytes().to_vec();
        for r in records {
            b.extend_from_slice(r);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn collect(buf: &[u8]) -> Result<(u64, Vec<(u64, u64, bool, String)>), ParseError> {
        let mut v = Vec::new();
        let mut s = String::new();
        let next = parse_enum_buffer(buf, |e| {
            e.decode_name(&mut s);
            v.push((e.record, e.parent, e.is_dir(), s.clone()));
        })?;
        Ok((next, v))
    }

    #[test]
    fn parses_records_and_strips_sequence_bits() {
        let seq = 7u64 << 48;
        let b = buffer(
            99,
            &[
                record(seq | 100, seq | 5, ATTR_DIRECTORY, "Docs", 2),
                record(seq | 101, seq | 100, 0x20, "résumé 📄.txt", 2),
            ],
        );
        let (next, v) = collect(&b).unwrap();
        assert_eq!(next, 99);
        assert_eq!(v[0], (100, 5, true, "Docs".into()));
        assert_eq!(v[1], (101, 100, false, "résumé 📄.txt".into()));
    }

    #[test]
    fn journal_buffer_carries_usn_and_reason() {
        let b = buffer(
            5000,
            &[
                journal_record(40, 5, 4096, REASON_FILE_CREATE, 0x20, "new.txt", 2),
                journal_record(40, 5, 4200, REASON_RENAME_NEW_NAME | 0x8000_0000, 0x20, "n2.txt", 2),
            ],
        );
        let mut seen = Vec::new();
        let next = parse_journal_buffer(&b, |e| seen.push((e.usn, e.reason, e.record))).unwrap();
        assert_eq!(next, 5000);
        assert_eq!(
            seen,
            [
                (4096, REASON_FILE_CREATE, 40),
                (4200, REASON_RENAME_NEW_NAME | 0x8000_0000, 40)
            ]
        );
    }

    #[test]
    fn skips_other_major_versions() {
        let b = buffer(1, &[record(10, 5, 0, "v3", 3), record(11, 5, 0, "v2", 2)]);
        let (_, v) = collect(&b).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].3, "v2");
    }

    #[test]
    fn empty_payload_is_ok() {
        let (next, v) = collect(&buffer(42, &[])).unwrap();
        assert_eq!(next, 42);
        assert!(v.is_empty());
    }

    #[test]
    fn rejects_hostile_buffers() {
        assert_eq!(collect(&[1, 2, 3]), Err(ParseError::TooShort));

        // record length past end of buffer
        let mut b = buffer(1, &[record(10, 5, 0, "a", 2)]);
        b.truncate(b.len() - 8);
        assert!(matches!(collect(&b), Err(ParseError::Malformed { .. })));

        // zero-length record must not spin forever
        let mut b = buffer(1, &[record(10, 5, 0, "a", 2)]);
        b[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(collect(&b), Err(ParseError::Malformed { .. })));

        // name offset/length escaping the record
        let mut b = buffer(1, &[record(10, 5, 0, "a", 2)]);
        b[8 + 56..8 + 58].copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert!(matches!(collect(&b), Err(ParseError::Malformed { .. })));

        // odd UTF-16 length
        let mut b = buffer(1, &[record(10, 5, 0, "ab", 2)]);
        b[8 + 56..8 + 58].copy_from_slice(&3u16.to_le_bytes());
        assert!(matches!(collect(&b), Err(ParseError::Malformed { .. })));

        // name offset pointing into the fixed header
        let mut b = buffer(1, &[record(10, 5, 0, "ab", 2)]);
        b[8 + 58..8 + 60].copy_from_slice(&8u16.to_le_bytes());
        assert!(matches!(collect(&b), Err(ParseError::Malformed { .. })));
    }

    #[test]
    fn lone_surrogate_becomes_replacement_char() {
        let mut b = buffer(1, &[record(10, 5, 0, "ab", 2)]);
        b[8 + 60..8 + 62].copy_from_slice(&0xD800u16.to_le_bytes());
        let (_, v) = collect(&b).unwrap();
        assert_eq!(v[0].3, "\u{FFFD}b");
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Cheap deterministic fuzz: xorshift-filled buffers with a plausible header.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..2000 {
            let mut b = vec![0u8; 8 + (x % 300) as usize];
            for byte in b.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *byte = x as u8;
            }
            let _ = parse_enum_buffer(&b, |_| {});
        }
    }
}
