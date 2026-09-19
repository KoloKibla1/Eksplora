//! USN Change Journal reader (NTFS, optional, needs admin).
//! Portable base works without it (scan + watcher).
//! When elevated, this gives ms-level deltas without rescanning.
//!
//! Design: query journal -> READ_USN_JOURNAL in pages -> parse USN_RECORD_V2/V3.
//! Full FRN->path resolution is NOT done here (needs MFT walk); v0 returns
//! parent FRN + name + reason, enough to invalidate/update index entries
//! and to prove elevation + journal health.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct UsnJournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
}

#[derive(Debug, Clone)]
pub struct UsnEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    pub reason: u32,
    pub reason_str: String,
    pub name: String,
    pub is_dir: bool,
}

pub fn reason_to_str(reason: u32) -> String {
    // winioctl.h USN_REASON_* (subset, most common first).
    let mut parts = Vec::new();
    let flags: &[(u32, &str)] = &[
        (0x00000001, "DATA_OVERWRITE"),
        (0x00000002, "DATA_EXTEND"),
        (0x00000004, "DATA_TRUNCATION"),
        (0x00000010, "NAMED_DATA_OVERWRITE"),
        (0x00000020, "NAMED_DATA_EXTEND"),
        (0x00000040, "NAMED_DATA_TRUNCATION"),
        (0x00000100, "FILE_CREATE"),
        (0x00000200, "FILE_DELETE"),
        (0x00000400, "EA_CHANGE"),
        (0x00000800, "SECURITY_CHANGE"),
        (0x00001000, "RENAME_OLD"),
        (0x00002000, "RENAME_NEW"),
        (0x00004000, "INDEXABLE_CHANGE"),
        (0x00008000, "BASIC_INFO_CHANGE"),
        (0x00010000, "HARD_LINK_CHANGE"),
        (0x00020000, "COMPRESSION_CHANGE"),
        (0x00040000, "ENCRYPTION_CHANGE"),
        (0x00080000, "OBJECT_ID_CHANGE"),
        (0x00100000, "REPARSE_POINT_CHANGE"),
        (0x00200000, "STREAM_CHANGE"),
        (0x80000000, "CLOSE"),
    ];
    for (m, s) in flags {
        if reason & m != 0 {
            parts.push(*s);
        }
    }
    if parts.is_empty() {
        format!("0x{:X}", reason)
    } else {
        parts.join("|")
    }
}

fn volume_root_of(p: &Path) -> PathBuf {
    // `C:\foo` -> `C:\`, `\\?\C:\...` -> keep drive letter logic simple.
    let s = p.to_string_lossy();
    if s.len() >= 2 && s.chars().nth(1) == Some(':') {
        PathBuf::from(format!("{}:\\", s.chars().next().unwrap()))
    } else if s.starts_with(r"\\?\") && s.len() >= 7 {
        PathBuf::from(format!("{}:\\", s.chars().nth(4).unwrap_or('C')))
    } else {
        PathBuf::from("C:\\")
    }
}

fn drive_letter_of(root: &Path) -> char {
    root.to_string_lossy().chars().next().unwrap_or('C')
}

#[cfg(not(windows))]
pub fn query_journal(_path: &Path) -> Result<UsnJournalInfo> {
    bail!("USN is Windows/NTFS only")
}

#[cfg(not(windows))]
pub fn read_deltas(_path: &Path, _start_usn: Option<i64>, _limit: usize) -> Result<(Vec<UsnEntry>, i64)> {
    bail!("USN is Windows/NTFS only")
}

#[cfg(not(windows))]
pub fn status_string(path: &Path) -> String {
    format!("USN unavailable (non-windows, path={})", path.display())
}

#[cfg(windows)]
pub fn status_string(path: &Path) -> String {
    match query_journal(path) {
        Ok(j) => format!(
            "USN ready: journal_id={:#x} first_usn={} next_usn={} (elevated, NTFS)",
            j.journal_id, j.first_usn, j.next_usn
        ),
        Err(e) => format!(
            "USN unavailable ({}). Run elevated + NTFS for journal boost; base uses scan+watch.",
            e
        ),
    }
}

// ---- Windows implementation ----

#[cfg(windows)]
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x000900F4;
#[cfg(windows)]
const FSCTL_READ_USN_JOURNAL: u32 = 0x000900BB;

#[cfg(windows)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UsnJournalData {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
    min_supported_major_version: u16,
    max_supported_major_version: u16,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ReadUsnJournalData {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

#[cfg(windows)]
fn open_volume(drive: char) -> Result<windows::Win32::Foundation::HANDLE> {
    use std::os::windows::ffi::OsStrExt;

    let path = format!("\\\\.\\{}:", drive);
    let wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let h = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            0x8000_0000, // GENERIC_READ
            windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            None,
            windows::Win32::Storage::FileSystem::OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .with_context(|| format!("open volume {} (needs admin)", path))?;
    Ok(h)
}

#[cfg(windows)]
pub fn query_journal(path: &Path) -> Result<UsnJournalInfo> {
    let root = volume_root_of(path);
    let drive = drive_letter_of(&root);
    let h = open_volume(drive)?;

    let mut out = UsnJournalData {
        usn_journal_id: 0,
        first_usn: 0,
        next_usn: 0,
        lowest_valid_usn: 0,
        max_usn: 0,
        maximum_size: 0,
        allocation_delta: 0,
        min_supported_major_version: 0,
        max_supported_major_version: 0,
    };
    let mut returned: u32 = 0;
    let res = unsafe {
        windows::Win32::System::IO::DeviceIoControl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some((&mut out as *mut UsnJournalData) as *mut _),
            std::mem::size_of::<UsnJournalData>() as u32,
            Some(&mut returned as *mut u32),
            None,
        )
    };
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
    match res {
        Ok(()) => Ok(UsnJournalInfo {
            journal_id: out.usn_journal_id,
            first_usn: out.first_usn,
            next_usn: out.next_usn,
            lowest_valid_usn: out.lowest_valid_usn,
            max_usn: out.max_usn,
        }),
        Err(e) => bail!("FSCTL_QUERY_USN_JOURNAL failed: {} (needs admin + NTFS)", e),
    }
}

/// Read up to `limit` USN records starting at `start_usn` (default: FirstUsn).
/// Returns (entries, next_usn_to_continue_from).
#[cfg(windows)]
pub fn read_deltas(path: &Path, start_usn: Option<i64>, limit: usize) -> Result<(Vec<UsnEntry>, i64)> {
    let limit = limit.clamp(1, 100_000);
    let journal = query_journal(path)?;
    let mut cursor = start_usn.unwrap_or(journal.first_usn);
    // If journal was recreated, old cursor is invalid — restart at first.
    if cursor < journal.first_usn || cursor > journal.next_usn {
        cursor = journal.first_usn;
    }

    let root = volume_root_of(path);
    let h = open_volume(drive_letter_of(&root))?;

    let mut out_entries: Vec<UsnEntry> = Vec::new();
    // 1MB out buffer holds ~hundreds of records per ioctl.
    let mut buf = vec![0u8; 1 << 20];

    // Safety: bound iterations so a huge journal + small limit can't loop forever.
    for _ in 0..512 {
        if out_entries.len() >= limit {
            break;
        }
        let input = ReadUsnJournalData {
            start_usn: cursor,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            usn_journal_id: journal.journal_id,
            min_major_version: 2,
            max_major_version: 4,
        };
        let mut returned: u32 = 0;
        let res = unsafe {
            windows::Win32::System::IO::DeviceIoControl(
                h,
                FSCTL_READ_USN_JOURNAL,
                Some((&input as *const ReadUsnJournalData) as *const _),
                std::mem::size_of::<ReadUsnJournalData>() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                buf.len() as u32,
                Some(&mut returned as *mut u32),
                None,
            )
        };
        if let Err(e) = res {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
            // ERROR_JOURNAL_NOT_ACTIVE (1179) / ERROR_JOURNAL_DELETE_IN_PROGRESS (1181)
            // mean no journal — surface clearly.
            bail!("FSCTL_READ_USN_JOURNAL failed: {} (is USN journal enabled on {}?)", e, root.display());
        }
        if returned <= 8 {
            break; // only next-USN header, no records.
        }
        // First 8 bytes = next USN after this batch.
        let next_usn = i64::from_le_bytes(buf[0..8].try_into().unwrap());
        let mut off = 8usize;
        while off + 4 <= returned as usize && out_entries.len() < limit {
            let rec_len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            if rec_len < 60 || off + rec_len > returned as usize {
                break;
            }
            if let Some(e) = parse_usn_record(&buf[off..off + rec_len]) {
                out_entries.push(e);
            }
            if rec_len == 0 {
                break;
            }
            off += rec_len;
        }
        if next_usn <= cursor {
            cursor = next_usn;
            break;
        }
        cursor = next_usn;
        if cursor >= journal.next_usn {
            break;
        }
    }
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
    Ok((out_entries, cursor))
}

#[cfg(windows)]
fn parse_usn_record(rec: &[u8]) -> Option<UsnEntry> {
    // USN_RECORD_V2/V3 common prefix (little-endian):
    // u32 RecordLength, u16 Major, u16 Minor,
    // u64 FRN, u64 ParentFRN, i64 Usn, i64 Time,
    // u32 Reason, u32 SourceInfo, u32 SecurityId, u32 Attrs,
    // u16 NameLen, u16 NameOff, [u16 name...]
    if rec.len() < 60 {
        return None;
    }
    let major = u16::from_le_bytes(rec[4..6].try_into().ok()?);
    if major != 2 && major != 3 {
        return None; // V4 (ReFS) skipped in v0.
    }
    let frn = u64::from_le_bytes(rec[8..16].try_into().ok()?);
    let parent = u64::from_le_bytes(rec[16..24].try_into().ok()?);
    let usn = i64::from_le_bytes(rec[24..32].try_into().ok()?);
    let reason = u32::from_le_bytes(rec[40..44].try_into().ok()?);
    let attrs = u32::from_le_bytes(rec[52..56].try_into().ok()?);
    let name_len = u16::from_le_bytes(rec[56..58].try_into().ok()?) as usize;
    let name_off = u16::from_le_bytes(rec[58..60].try_into().ok()?) as usize;
    if name_off + name_len > rec.len() || name_len % 2 != 0 {
        return None;
    }
    let w: Vec<u16> = rec[name_off..name_off + name_len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&w);
    // 0x10000000 = FILE_ATTRIBUTE_DIRECTORY
    let is_dir = attrs & 0x1000_0000 != 0;
    Some(UsnEntry {
        frn,
        parent_frn: parent,
        usn,
        reason,
        reason_str: reason_to_str(reason),
        name,
        is_dir,
    })
}
