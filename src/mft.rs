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
use crate::usn::parse_enum_buffer;

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
    /// Time spent inside `DeviceIoControl` (kernel reading the MFT).
    pub ioctl: Duration,
    /// Time spent decoding records and filling the index builder.
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

/// Enumerate every file on `letter:` into `builder` as one volume.
pub fn index_volume(letter: char, builder: &mut Builder) -> io::Result<VolumeStats> {
    let vol = Volume::open(letter)?;
    let mut stats = VolumeStats {
        letter,
        ..Default::default()
    };
    stats.mft_records_estimate = mft_record_estimate(&vol);
    let est = stats.mft_records_estimate as usize;
    builder.begin_volume(&format!("{letter}:"), ROOT_RECORD);
    builder.reserve(est, est.saturating_mul(24));

    let mut input = MftEnumData {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut out = vec![0u8; BUF_BYTES];
    let mut name = String::new();
    loop {
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
        stats.ioctl += t.elapsed();
        stats.ioctl_calls += 1;
        if ok == 0 {
            if err == ERROR_HANDLE_EOF {
                break;
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        stats.bytes += ret as u64;

        let t = Instant::now();
        let buf = &out[..ret as usize];
        let next = parse_enum_buffer(buf, |e| {
            if e.record == ROOT_RECORD {
                return; // the root is synthesized by begin_volume
            }
            e.decode_name(&mut name);
            builder.push(e.record, e.parent, &name, e.is_dir());
            stats.entries += 1;
        })
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        stats.parse += t.elapsed();
        input.start_file_reference_number = next;
    }

    let t = Instant::now();
    stats.orphans = builder.end_volume();
    stats.resolve = t.elapsed();
    Ok(stats)
}
