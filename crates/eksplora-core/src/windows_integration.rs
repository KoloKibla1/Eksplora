//! Windows integration base: known folders, attributes, volume checks.
//! USN journal helper is a stub that detects readiness (admin + NTFS)
//! without requiring elevation for the base app.

use anyhow::Result;
use std::path::PathBuf;

#[cfg(windows)]
pub fn known_folders() -> Result<Vec<(String, PathBuf)>> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::UI::Shell::*;

    // (display name, KNOWNFOLDERID)
    let ids: &[(&str, &windows::core::GUID)] = &[
        ("Desktop", &FOLDERID_Desktop),
        ("Documents", &FOLDERID_Documents),
        ("Downloads", &FOLDERID_Downloads),
        ("Pictures", &FOLDERID_Pictures),
        ("Music", &FOLDERID_Music),
        ("Videos", &FOLDERID_Videos),
        ("Profile", &FOLDERID_Profile),
        ("LocalAppData", &FOLDERID_LocalAppData),
        ("RoamingAppData", &FOLDERID_RoamingAppData),
    ];
    let mut out = Vec::new();
    for (name, id) in ids {
        // SHGetKnownFolderPath(rfid, flags, token) -> Result<PWSTR>
        let hr = unsafe {
            SHGetKnownFolderPath(
                *id as *const _,
                KNOWN_FOLDER_FLAG(0),
                HANDLE::default(),
            )
        };
        match hr {
            Ok(p) => {
                let s = unsafe { p.to_string() }.unwrap_or_default();
                unsafe {
                    windows::Win32::System::Com::CoTaskMemFree(Some(
                        p.as_ptr() as _,
                    ))
                };
                if !s.is_empty() {
                    out.push((name.to_string(), PathBuf::from(s)));
                }
            }
            Err(_) => continue,
        }
    }
    Ok(out)
}

#[cfg(not(windows))]
pub fn known_folders() -> Result<Vec<(String, PathBuf)>> {
    Ok(vec![("Home".into(), dirs_fallback())])
}

#[cfg(not(windows))]
fn dirs_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 0x400000 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS (OneDrive placeholder).
pub fn is_cloud_placeholder(attributes: u32) -> bool {
    attributes & 0x0040_0000 != 0
}

/// Check if a volume path (e.g. `C:\`) is NTFS and whether we can open
/// the volume handle (needs admin). Returns (fs_name, can_open_volume).
/// Base app must work when can_open_volume == false — fall back to scan+watch.
#[cfg(windows)]
pub fn volume_readiness(root: &std::path::Path) -> (String, bool) {
    use std::os::windows::ffi::OsStrExt;

    // GetVolumeInformationW needs root like `C:\`.
    let root_str = root
        .ancestors()
        .find(|p| p.parent().is_none() || p.to_string_lossy().ends_with(":\\"))
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    let root_w: Vec<u16> = root_str.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut fs_name = [0u16; 32];
    let ok = unsafe {
        windows::Win32::Storage::FileSystem::GetVolumeInformationW(
            windows::core::PCWSTR(root_w.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    };
    let fs = if ok.is_ok() {
        String::from_utf16_lossy(&fs_name)
            .trim_matches('\0')
            .to_string()
    } else {
        "unknown".into()
    };

    // Try opening \\.\C: — fails without admin. Expected on normal PCs.
    // Use std::fs so we don't depend on CreateFileW bindings across versions.
    let drive_letter = root_str.to_string_lossy().chars().next().unwrap_or('C');
    let vol_path = format!("\\\\.\\{}:", drive_letter);
    let can_open = std::fs::File::open(&vol_path).is_ok();
    (format!("{} (can_open_volume={})", fs.trim_matches(char::from(0)), can_open), can_open)
}

#[cfg(not(windows))]
pub fn volume_readiness(_root: &std::path::Path) -> (String, bool) {
    ("non-windows".into(), false)
}

pub fn usn_status(root: &std::path::Path) -> String {
    let (info, can_open) = volume_readiness(root);
    if can_open {
        format!("USN ready on {} — incremental journal possible", info)
    } else {
        format!(
            "USN unavailable ({}). Running in portable mode: parallel scan + watcher. Elevate for USN/MFT boost.",
            info
        )
    }
}
