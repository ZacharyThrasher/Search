//! NTFS enumeration via `FSCTL_ENUM_USN_DATA`.
//!
//! This asks NTFS for one record per file straight out of the master file
//! table, so no directory walking happens. It needs a handle to the raw
//! volume (`\\.\C:`), which Windows only grants to elevated processes.

use std::io;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER,
};

use crate::index::Builder;
use crate::usn::{FRN_RECORD_MASK, parse_enum_buffer};

const GENERIC_READ: u32 = 0x8000_0000;
const DRIVE_FIXED: u32 = 3;
/// MFT record number of the volume root directory.
const ROOT_RECORD: u64 = 5;
/// Output buffer per `DeviceIoControl` call.
const BUF_BYTES: usize = 1 << 20;

pub const ERROR_ACCESS_DENIED: i32 = 5;
pub const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;

/// Mirror of `MFT_ENUM_DATA_V0`.
#[repr(C)]
struct MftEnumData {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

struct Volume(HANDLE);

impl Volume {
    fn open(letter: char) -> io::Result<Volume> {
        let path: Vec<u16> = format!(r"\\.\{letter}:")
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Volume(h))
        }
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: handle came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Fixed drives formatted NTFS, e.g. `['C', 'E']`.
pub fn fixed_ntfs_volumes() -> Vec<char> {
    let mut out = Vec::new();
    // SAFETY: plain Win32 queries with valid, NUL-terminated buffers.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        let mut fs = [0u16; 32];
        unsafe {
            if GetDriveTypeW(root.as_ptr()) != DRIVE_FIXED {
                continue;
            }
            let ok = GetVolumeInformationW(
                root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            );
            if ok == 0 {
                continue;
            }
        }
        let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        if String::from_utf16_lossy(&fs[..n]).eq_ignore_ascii_case("NTFS") {
            out.push(letter);
        }
    }
    out
}

#[derive(Debug, Default, Clone)]
pub struct VolumeStats {
    pub letter: char,
    pub entries: usize,
    pub orphans: usize,
    /// Estimated MFT records (valid MFT length / record size).
    pub mft_records_estimate: u64,
    /// Wall-clock time reading the MFT (all worker threads together).
    pub enumerate: Duration,
    /// Summed time inside `DeviceIoControl` across workers.
    pub ioctl: Duration,
    /// Time merging worker output into the index builder.
    pub parse: Duration,
    /// Time resolving parent links.
    pub resolve: Duration,
    pub ioctl_calls: usize,
    pub bytes: u64,
}

fn mft_record_estimate(vol: &Volume) -> u64 {
    // SAFETY: `data` is a correctly sized, writable NTFS_VOLUME_DATA_BUFFER.
    unsafe {
        let mut data: NTFS_VOLUME_DATA_BUFFER = std::mem::zeroed();
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            vol.0,
            FSCTL_GET_NTFS_VOLUME_DATA,
            null(),
            0,
            &mut data as *mut _ as *mut _,
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut ret,
            null_mut(),
        );
        if ok == 0 || data.BytesPerFileRecordSegment == 0 {
            return 0;
        }
        data.MftValidDataLength as u64 / data.BytesPerFileRecordSegment as u64
    }
}

/// Entries gathered by one worker, kept compact until they are merged.
#[derive(Default)]
struct Chunk {
    /// (record, parent, is_dir, end offset of the name in `names`)
    recs: Vec<(u64, u64, bool, u32)>,
    names: String,
    ioctl: Duration,
    calls: usize,
    bytes: u64,
}

/// Enumerate records with number in `[start, end)` on `letter:`.
/// `FSCTL_ENUM_USN_DATA` returns records in ascending order and accepts any
/// starting reference number, so disjoint ranges can run on separate threads.
fn enum_range(letter: char, start: u64, end: u64) -> io::Result<Chunk> {
    let vol = Volume::open(letter)?;
    let mut chunk = Chunk::default();
    let mut input = MftEnumData {
        start_file_reference_number: start,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut out = vec![0u8; BUF_BYTES];
    let mut name = String::new();
    let mut last = start;
    'outer: loop {
        let mut ret = 0u32;
        let t = Instant::now();
        // SAFETY: input/out are valid for the sizes passed; `ret` is writable.
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_ENUM_USN_DATA,
                &input as *const _ as *const _,
                std::mem::size_of::<MftEnumData>() as u32,
                out.as_mut_ptr() as *mut _,
                out.len() as u32,
                &mut ret,
                null_mut(),
            )
        };
        // Capture the error before anything else can overwrite it.
        // SAFETY: trivially safe thread-local read.
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        chunk.ioctl += t.elapsed();
        chunk.calls += 1;
        if ok == 0 {
            if err == ERROR_HANDLE_EOF {
                break;
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        chunk.bytes += ret as u64;

        let mut past_end = false;
        let mut disordered = false;
        let next = parse_enum_buffer(&out[..ret as usize], |e| {
            if e.record >= end {
                past_end = true;
                return;
            }
            if e.record < last {
                disordered = true;
            }
            last = e.record;
            if e.record == ROOT_RECORD {
                return; // the root is synthesized by begin_volume
            }
            e.decode_name(&mut name);
            chunk.names.push_str(&name);
            chunk
                .recs
                .push((e.record, e.parent, e.is_dir(), chunk.names.len() as u32));
        })
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if disordered {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FSCTL_ENUM_USN_DATA returned records out of order",
            ));
        }
        if past_end || (next & FRN_RECORD_MASK) >= end {
            break 'outer;
        }
        input.start_file_reference_number = next;
    }
    Ok(chunk)
}

/// Enumerate every file on `letter:` into `builder` as one volume.
/// `threads == 1` reads serially; larger values split the record-number space
/// into ranges that are enumerated concurrently.
pub fn index_volume(
    letter: char,
    builder: &mut Builder,
    threads: usize,
) -> io::Result<VolumeStats> {
    let est = {
        let vol = Volume::open(letter)?;
        mft_record_estimate(&vol)
    };
    let mut stats = VolumeStats {
        letter,
        mft_records_estimate: est,
        ..Default::default()
    };

    // Range boundaries over [0, est); the last range is open-ended.
    let threads = threads.max(1);
    let ranges = if threads == 1 || est < 100_000 {
        1
    } else {
        threads * 2
    };
    let bounds: Vec<(u64, u64)> = (0..ranges as u64)
        .map(|k| {
            let a = k * est / ranges as u64;
            let b = if k + 1 == ranges as u64 {
                u64::MAX
            } else {
                (k + 1) * est / ranges as u64
            };
            (a, b)
        })
        .collect();

    let t = Instant::now();
    let chunks: Vec<io::Result<Chunk>> = if ranges == 1 {
        vec![enum_range(letter, 0, u64::MAX)]
    } else {
        use rayon::prelude::*;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(io::Error::other)?;
        pool.install(|| {
            bounds
                .par_iter()
                .map(|&(a, b)| enum_range(letter, a, b))
                .collect()
        })
    };
    stats.enumerate = t.elapsed();
    let chunks = chunks.into_iter().collect::<io::Result<Vec<_>>>()?;

    let t = Instant::now();
    builder.begin_volume(&format!("{letter}:"), ROOT_RECORD);
    builder.reserve(est as usize, est as usize * 24);
    for c in &chunks {
        stats.ioctl += c.ioctl;
        stats.ioctl_calls += c.calls;
        stats.bytes += c.bytes;
        let mut at = 0usize;
        for &(record, parent, is_dir, end) in &c.recs {
            let end = end as usize;
            builder.push(record, parent, &c.names[at..end], is_dir);
            at = end;
        }
        stats.entries += c.recs.len();
    }
    stats.parse = t.elapsed();

    let t = Instant::now();
    stats.orphans = builder.end_volume();
    stats.resolve = t.elapsed();
    Ok(stats)
}
