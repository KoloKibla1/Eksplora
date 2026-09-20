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

/// Opens a file with its default application via ShellExecuteW("open").
/// Direct shell call — no `cmd` middleman, so no console flash and no
/// startup delay. Errors on association failures (return <= 32).
#[cfg(windows)]
pub fn open_with_default_app(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let ret = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if (ret.0 as isize) <= 32 {
        anyhow::bail!(
            "ShellExecute failed ({}) for {}",
            ret.0 as isize,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn open_with_default_app(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("open failed: {}", e))
}

/// Moves a file or directory to the Recycle Bin (undoable delete).
/// Uses SHFileOperationW with FO_DELETE + FOF_ALLOWUNDO; our own UI
/// confirms first, so the system dialog is suppressed (FOF_NOCONFIRMATION)
/// and no progress UI is shown (FOF_SILENT).
#[cfg(windows)]
pub fn move_to_recycle_bin(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{BOOL, HWND};
    use windows::Win32::UI::Shell::*;

    // SHFileOperationW requires double-null-terminated path lists.
    let mut from: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        pTo: windows::core::PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT).0 as u16,
        fAnyOperationsAborted: BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: windows::core::PCWSTR::null(),
    };
    let ret = unsafe { SHFileOperationW(&mut op) };
    if ret != 0 {
        anyhow::bail!("recycle failed ({:#X}) for {}", ret as u32, path.display());
    }
    if op.fAnyOperationsAborted.as_bool() {
        anyhow::bail!("recycle aborted for {}", path.display());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn move_to_recycle_bin(path: &std::path::Path) -> Result<()> {
    // No portable trash here — refuse rather than permanently delete.
    anyhow::bail!("recycle bin not supported on this platform ({})", path.display());
}

/// Places file paths on the OS clipboard as CF_HDROP (plus a
/// "Preferred DropEffect" of COPY), so the user can paste into Explorer as
/// well as back into the app. Retries briefly — the clipboard is often held
/// by another app for a few ms.
#[cfg(windows)]
pub fn set_clipboard_files(paths: &[std::path::PathBuf]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{BOOL, HANDLE, HWND, POINT, GlobalFree};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    use windows::Win32::UI::Shell::DROPFILES;

    // CF_HDROP = 15 (Win32::System::Ole — feature not enabled, value stable).
    const CF_HDROP: u32 = 15;
    // DROPEFFECT_COPY = 1 (same story).
    const DROP_COPY: u32 = 1;

    if paths.is_empty() {
        anyhow::bail!("nothing to copy");
    }

    // Payload: DROPFILES header (wide paths) + double-null-terminated list.
    let header_len = std::mem::size_of::<DROPFILES>();
    let mut files_w: Vec<u16> = Vec::new();
    for p in paths {
        files_w.extend(p.as_os_str().encode_wide().chain(Some(0)));
    }
    files_w.push(0);
    let total = header_len + files_w.len() * 2;

    unsafe {
        // The clipboard may be momentarily locked by another process.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(HWND::default()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            anyhow::bail!("clipboard is busy");
        }
        // From here every exit path must close the clipboard.
        let result: Result<()> = (|| {
            EmptyClipboard()?;
            // Moves `bytes` onto the clipboard under `format`. Ownership
            // passes to the OS on success; freed here on any failure.
            let put = |format: u32, bytes: &[u8]| -> Result<()> {
                let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
                let ptr = GlobalLock(h.clone()) as *mut u8;
                if ptr.is_null() {
                    let _ = GlobalFree(h);
                    anyhow::bail!("clipboard alloc failed");
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                // NOTE: no `?` — GlobalUnlock reports FALSE exactly when the
                // lock count reaches zero, i.e. on success.
                let _ = GlobalUnlock(h.clone());
                if SetClipboardData(format, HANDLE(h.0)).is_err() {
                    let _ = GlobalFree(h);
                    anyhow::bail!("clipboard set failed");
                }
                Ok(())
            };
            // CF_HDROP payload: DROPFILES header + wide file list.
            let mut payload = vec![0u8; total];
            let df = DROPFILES {
                pFiles: header_len as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: BOOL(0),
                fWide: BOOL(1),
            };
            payload[..header_len].copy_from_slice(std::slice::from_raw_parts(
                &df as *const DROPFILES as *const u8,
                header_len,
            ));
            let file_bytes =
                std::slice::from_raw_parts(files_w.as_ptr() as *const u8, files_w.len() * 2);
            payload[header_len..].copy_from_slice(file_bytes);
            put(CF_HDROP, &payload)?;
            // Preferred DropEffect = COPY so hosts paste as copy, not move.
            // Best-effort: failure here must not fail the copy.
            let fmt = RegisterClipboardFormatA(windows::core::s!(
                "Preferred DropEffect"
            ));
            if fmt != 0 {
                let _ = put(fmt, &DROP_COPY.to_le_bytes());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn set_clipboard_files(paths: &[std::path::PathBuf]) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("nothing to copy");
    }
    anyhow::bail!("system clipboard not supported on this platform");
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
