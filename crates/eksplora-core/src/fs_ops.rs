//! Local file operations behind the context menu: copy, rename, delete,
//! move, and undoing deletes by restoring from the Recycle Bin.
//! All validate first and return plain errors the UI can display.
//! Callers refresh the index afterwards (rescan); these functions only
//! touch the filesystem.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Duplicate a file or directory next to itself, Explorer-style:
/// `notes.txt` -> `notes - Copy.txt`, then `notes - Copy (2).txt`, …
/// Directories copy recursively (symlinks/reparse points are skipped, same
/// as the scanner). Returns the new path.
pub fn copy_path(src: &Path) -> Result<PathBuf> {
    if !src.exists() {
        anyhow::bail!("no longer exists: {}", src.display());
    }
    let dst = copy_target(src)?;
    if src.is_dir() {
        copy_dir_recursive(src, &dst)?;
    } else if src.is_file() {
        std::fs::copy(src, &dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    } else {
        anyhow::bail!("not a file or directory: {}", src.display());
    }
    Ok(dst)
}

/// Rename a file or directory within its own folder. `new_name` is a bare
/// name (no separators); the previous name is untouched on any error.
pub fn rename_entry(path: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        anyhow::bail!("name is empty");
    }
    if new_name.contains(['/', '\\', '\0']) {
        anyhow::bail!("name must not contain path separators");
    }
    if !path.exists() {
        anyhow::bail!("no longer exists: {}", path.display());
    }
    let parent = path.parent().with_context(|| format!("no parent: {}", path.display()))?;
    if path.file_name().map(|n| n.to_string_lossy() == new_name).unwrap_or(false) {
        anyhow::bail!("same name — nothing to do");
    }
    let dst = parent.join(new_name);
    if dst.exists() {
        anyhow::bail!("name already exists");
    }
    std::fs::rename(path, &dst)
        .with_context(|| format!("rename {} -> {}", path.display(), dst.display()))?;
    Ok(dst)
}

/// Duplicate with an explicit new name (context-menu "Duplicate" panel).
/// Refuses when the name is taken instead of auto-numbering — the UI shows
/// the message and lets the user pick another name.
pub fn duplicate_path(src: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        anyhow::bail!("name is empty");
    }
    if new_name.contains(['/', '\\', '\0']) {
        anyhow::bail!("name must not contain path separators");
    }
    if !src.exists() {
        anyhow::bail!("no longer exists: {}", src.display());
    }
    let parent = src.parent().with_context(|| format!("no parent: {}", src.display()))?;
    let dst = parent.join(new_name);
    if dst.exists() {
        anyhow::bail!("name already exists");
    }
    if src.is_dir() {
        copy_dir_recursive(src, &dst)?;
    } else if src.is_file() {
        std::fs::copy(src, &dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    } else {
        anyhow::bail!("not a file or directory: {}", src.display());
    }
    Ok(dst)
}

/// Paste a copied file/folder into `dir`. Keeps the original name when free,
/// otherwise appends `-copy` (`notes-copy.txt`, then `notes-copy (2).txt`),
/// so pasting into the source's own folder never collides. Returns the new
/// path. Missing sources are reported, not silently skipped.
pub fn paste_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    if !src.exists() {
        anyhow::bail!("source no longer exists: {}", src.display());
    }
    if !dir.is_dir() {
        anyhow::bail!("not a folder: {}", dir.display());
    }
    let name = src
        .file_name()
        .with_context(|| format!("no file name: {}", src.display()))?
        .to_string_lossy()
        .into_owned();
    let dst = paste_target(dir, &name, src.is_dir());
    if src.is_dir() {
        copy_dir_recursive(src, &dst)?;
    } else if src.is_file() {
        std::fs::copy(src, &dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    } else {
        anyhow::bail!("not a file or directory: {}", src.display());
    }
    Ok(dst)
}

/// Next free sibling for pasting `name` into `dir`: the plain name when
/// free, else `{stem}-copy{ext}`, `{stem}-copy (2){ext}`, … Directories
/// treat the whole name as the stem (extension would be meaningless).
fn paste_target(dir: &Path, name: &str, is_dir: bool) -> PathBuf {
    let direct = dir.join(name);
    if !direct.exists() {
        return direct;
    }
    let (stem, ext) = if is_dir {
        (name.to_string(), String::new())
    } else {
        match name.rfind('.').filter(|&i| i > 0) {
            Some(i) => (name[..i].to_string(), name[i..].to_string()),
            None => (name.to_string(), String::new()),
        }
    };
    let mut candidate = dir.join(format!("{}-copy{}", stem, ext));
    let mut n = 2u32;
    while candidate.exists() {
        candidate = dir.join(format!("{}-copy ({}){}", stem, n, ext));
        n += 1;
        if n > 10_000 {
            // Practically unreachable (10k same-stem siblings); timestamp
            // suffix guarantees termination instead of looping forever.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            return dir.join(format!("{}-copy-{:x}{}", stem, nanos, ext));
        }
    }
    candidate
}

/// Send a file or directory to the Recycle Bin (undoable). Windows only —
/// refusing elsewhere beats silently destroying data.
pub fn delete_path(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("no longer exists: {}", path.display());
    }
    crate::windows_integration::move_to_recycle_bin(path)
}

/// Move a file or directory into another folder (drag and drop).
/// Keeps the name; refuses on conflict instead of overwriting. Dropping an
/// item into its own folder is a no-op success. Moving a folder into itself
/// or one of its children is refused. Returns the final path.
pub fn move_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    if !src.exists() {
        anyhow::bail!("no longer exists: {}", src.display());
    }
    if !dir.is_dir() {
        anyhow::bail!("not a folder: {}", dir.display());
    }
    let name = src
        .file_name()
        .with_context(|| format!("no file name: {}", src.display()))?;
    let dst = dir.join(name);
    if dst == *src {
        return Ok(dst); // dropped into its own folder — nothing to do
    }
    if dst.exists() {
        anyhow::bail!("name already exists");
    }
    if src.is_dir() && dst.starts_with(src) {
        anyhow::bail!("can't move a folder into itself");
    }
    std::fs::rename(src, &dst)
        .with_context(|| format!("move {} -> {}", src.display(), dst.display()))?;
    Ok(dst)
}

/// A Recycle Bin entry matched to an original path, parsed from its `$I`
/// metadata file. The `$R` data file shares the `$I` file's name with the
/// leading `I` swapped for `R` (`$Iab12cd.txt` <-> `$Rab12cd.txt`).
#[derive(Debug, Clone, PartialEq)]
pub struct BinItem {
    /// Full original path, as stored in the `$I` file.
    pub original_path: PathBuf,
    /// Original size in bytes (from the `$I` header).
    pub size: u64,
    /// Deletion time as a Windows FILETIME.
    pub deleted_at: u64,
    /// Path of the `$I` metadata file.
    pub info_path: PathBuf,
    /// Path of the `$R` entry holding the actual contents (file or dir).
    pub data_path: PathBuf,
}

/// Restore the most recently deleted Recycle Bin copy of `original` back to
/// `original` (Ctrl+Z for deletes). Same-volume rename plus `$I` cleanup —
/// exactly what Explorer's restore does, without any COM. Refuses when the
/// target exists or the parent folder is gone.
pub fn restore_from_bin(original: &Path) -> Result<PathBuf> {
    let bin_root = drive_bin_root(original)?;
    restore_from_bin_under(&bin_root, original)
}

/// `restore_from_bin` with an explicit bin root (tests point this at a fake
/// `$Recycle.Bin` tree instead of a real drive root).
fn restore_from_bin_under(bin_root: &Path, original: &Path) -> Result<PathBuf> {
    let found = find_in_bin_under(bin_root, original)?.with_context(|| {
        format!(
            "not found in Recycle Bin — it may have been emptied: {}",
            original.display()
        )
    })?;
    if original.exists() {
        anyhow::bail!("already exists: {}", original.display());
    }
    if let Some(parent) = original.parent() {
        if !parent.is_dir() {
            anyhow::bail!("original folder no longer exists: {}", parent.display());
        }
    }
    if !found.data_path.exists() {
        anyhow::bail!("Recycle Bin data is missing for {}", original.display());
    }
    std::fs::rename(&found.data_path, original).with_context(|| {
        format!(
            "restore {} -> {}",
            found.data_path.display(),
            original.display()
        )
    })?;
    // Drop the metadata last: with the `$I` file gone, Explorer no longer
    // lists the entry. Best-effort — the restore itself already succeeded.
    let _ = std::fs::remove_file(&found.info_path);
    Ok(original.to_path_buf())
}

/// Latest-deleted bin entry for `original`, if any. Scans every SID folder
/// (other users' entries simply fail to read and are skipped).
fn find_in_bin_under(bin_root: &Path, original: &Path) -> Result<Option<BinItem>> {
    let want = norm_bin_path(original);
    let sids = std::fs::read_dir(bin_root).with_context(|| {
        format!("cannot read Recycle Bin at {}", bin_root.display())
    })?;
    let mut best: Option<BinItem> = None;
    for sid in sids {
        let sid = match sid {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if !sid.is_dir() {
            continue;
        }
        let infos = match std::fs::read_dir(&sid) {
            Ok(rd) => rd,
            Err(_) => continue, // another user's folder — not ours to read
        };
        for info in infos {
            let info_path = match info {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if !is_info_file(&info_path) {
                continue;
            }
            let bytes = match std::fs::read(&info_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (size, deleted_at, original_path) = match parse_bin_info(&bytes) {
                Ok(v) => v,
                Err(_) => continue, // not ours / corrupt — skip, don't fail
            };
            if norm_bin_path(&original_path) != want {
                continue;
            }
            let data_path = paired_data_path(&info_path);
            let newer = best.as_ref().map(|b| deleted_at > b.deleted_at).unwrap_or(true);
            if newer {
                best = Some(BinItem {
                    original_path,
                    size,
                    deleted_at,
                    info_path: info_path.clone(),
                    data_path,
                });
            }
        }
    }
    Ok(best)
}

/// `$Recycle.Bin` directory for the drive holding `path`
/// (`C:\foo\bar.txt` -> `C:\$Recycle.Bin`).
fn drive_bin_root(path: &Path) -> Result<PathBuf> {
    let root = path
        .ancestors()
        .last()
        .with_context(|| format!("no drive root: {}", path.display()))?;
    Ok(root.join("$Recycle.Bin"))
}

fn is_info_file(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().starts_with("$I"))
        .unwrap_or(false)
}

fn paired_data_path(info_path: &Path) -> PathBuf {
    let name = info_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut data_name = String::from("$R");
    data_name.push_str(name.strip_prefix("$I").unwrap_or(&name));
    info_path.with_file_name(data_name)
}

/// Compare helper: backslashes, no trailing separators, lowercase.
/// Both sides go through the same function, so `C:\` ( -> `c:` ) still
/// matches itself.
fn norm_bin_path(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Parse an `$I` Recycle Bin metadata file. Two layouts exist in the wild
/// (verified against a live Win11 bin, which uses v2 exclusively):
/// - v1: u64 header (1), u64 original size, u64 deletion FILETIME, then the
///   null-terminated UTF-16 original path at offset 24.
/// - v2: u64 header (2), u64 original size, u64 deletion FILETIME, u32 path
///   length in UTF-16 code units *including* the null terminator at offset
///   24, then the path itself at offset 28.
/// Returns (size, deleted_at, original_path).
fn parse_bin_info(bytes: &[u8]) -> Result<(u64, u64, PathBuf)> {
    if bytes.len() < 28 {
        anyhow::bail!("$I file too short ({} bytes)", bytes.len());
    }
    let u64_at = |off: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[off..off + 8]);
        u64::from_le_bytes(b)
    };
    let header = u64_at(0);
    let size = u64_at(8);
    let deleted_at = u64_at(16);
    if header == 2 {
        let mut len_b = [0u8; 4];
        len_b.copy_from_slice(&bytes[24..28]);
        let len = u32::from_le_bytes(len_b) as usize;
        if len == 0 {
            anyhow::bail!("$I path length is zero");
        }
        if 28 + len * 2 > bytes.len() {
            anyhow::bail!("$I path overruns the file");
        }
        let mut wchars: Vec<u16> = Vec::with_capacity(len);
        for i in 0..len - 1 {
            let off = 28 + i * 2;
            wchars.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
        }
        if wchars.is_empty() {
            anyhow::bail!("$I path is empty");
        }
        let path_str =
            String::from_utf16(&wchars).with_context(|| "$I path is not valid UTF-16")?;
        return Ok((size, deleted_at, PathBuf::from(path_str)));
    }
    if header != 1 {
        anyhow::bail!("bad $I header ({})", header);
    }
    // UTF-16LE path from offset 24 up to the first null (bounded by EOF).
    let mut wchars: Vec<u16> = Vec::new();
    let mut off = 24;
    loop {
        if off + 2 > bytes.len() {
            anyhow::bail!("$I path not null-terminated");
        }
        let w = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        if w == 0 {
            break;
        }
        wchars.push(w);
        off += 2;
    }
    if wchars.is_empty() {
        anyhow::bail!("$I path is empty");
    }
    let path_str =
        String::from_utf16(&wchars).with_context(|| "$I path is not valid UTF-16")?;
    Ok((size, deleted_at, PathBuf::from(path_str)))
}

/// Next free ` - Copy` sibling for `src`.
fn copy_target(src: &Path) -> Result<PathBuf> {
    let parent = src.parent().with_context(|| format!("no parent: {}", src.display()))?;
    let (stem, ext) = if src.is_dir() {
        (src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), String::new())
    } else {
        let stem = src
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = src
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        (stem, ext)
    };
    if stem.is_empty() {
        anyhow::bail!("cannot copy: {}", src.display());
    }
    let mut candidate = parent.join(format!("{} - Copy{}", stem, ext));
    let mut n = 2u32;
    while candidate.exists() {
        candidate = parent.join(format!("{} - Copy ({}){}", stem, n, ext));
        n += 1;
        if n > 10_000 {
            anyhow::bail!("too many copies of {}", src.display());
        }
    }
    Ok(candidate)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("create dir {}", dst.display()))?;
    let rd = std::fs::read_dir(src)
        .with_context(|| format!("read dir {}", src.display()))?;
    for child in rd {
        let child = child.with_context(|| format!("read entry in {}", src.display()))?;
        let ft = child
            .file_type()
            .with_context(|| format!("stat {}", child.path().display()))?;
        // Never follow symlinks/reparse points (matches the scanner).
        if ft.is_symlink() {
            continue;
        }
        let target = dst.join(child.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&child.path(), &target)?;
        } else if ft.is_file() {
            std::fs::copy(child.path(), &target).with_context(|| {
                format!("copy {} -> {}", child.path().display(), target.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eksplora-test-{}-{}-{}", std::process::id(), name, rand_suffix()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:x}", nanos ^ (std::process::id() << 16))
    }

    #[test]
    fn copies_file_with_copy_suffix() {
        let root = tmp_root("copy-file");
        std::fs::write(root.join("notes.txt"), "hello").unwrap();
        let dst = copy_path(&root.join("notes.txt")).unwrap();
        assert_eq!(dst, root.join("notes - Copy.txt"));
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");
        // Second copy picks the numbered variant.
        let dst2 = copy_path(&root.join("notes.txt")).unwrap();
        assert_eq!(dst2, root.join("notes - Copy (2).txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copies_dir_recursively() {
        let root = tmp_root("copy-dir");
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), "a").unwrap();
        std::fs::write(root.join("sub").join("deep").join("b.txt"), "b").unwrap();
        let dst = copy_path(&root.join("sub")).unwrap();
        assert_eq!(dst, root.join("sub - Copy"));
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(dst.join("deep").join("b.txt")).unwrap(), "b");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn renames_and_rejects_bad_names() {        let root = tmp_root("rename");
        std::fs::write(root.join("old.txt"), "x").unwrap();
        let dst = rename_entry(&root.join("old.txt"), "new.txt").unwrap();
        assert_eq!(dst, root.join("new.txt"));
        assert!(!root.join("old.txt").exists());
        assert!(rename_entry(&root.join("new.txt"), "").is_err());
        assert!(rename_entry(&root.join("new.txt"), "a/b").is_err());
        assert!(rename_entry(&root.join("new.txt"), "new.txt").is_err()); // same
        std::fs::write(root.join("taken.txt"), "t").unwrap();
        assert!(rename_entry(&root.join("new.txt"), "taken.txt").is_err()); // clash
        assert!(root.join("new.txt").exists()); // untouched by failed renames
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicates_with_explicit_name_and_refuses_clash() {
        let root = tmp_root("duplicate");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let dst = duplicate_path(&root.join("a.txt"), "a-copy.txt").unwrap();
        assert_eq!(dst, root.join("a-copy.txt"));
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "a");
        // Conflict is an error (UI shows the message), not auto-numbering.
        assert!(duplicate_path(&root.join("a.txt"), "a-copy.txt").is_err());
        assert!(duplicate_path(&root.join("a.txt"), "").is_err());
        assert!(duplicate_path(&root.join("missing.txt"), "x.txt").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paste_keeps_name_then_appends_copy() {
        let root = tmp_root("paste");
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(root.join("d")).unwrap();
        // Free name elsewhere: unchanged.
        let p1 = paste_into(&root.join("a.txt"), &other).unwrap();
        assert_eq!(p1, other.join("a.txt"));
        // Same folder: -copy suffix, then numbered.
        let p2 = paste_into(&root.join("a.txt"), &root).unwrap();
        assert_eq!(p2, root.join("a-copy.txt"));
        let p3 = paste_into(&root.join("a.txt"), &root).unwrap();
        assert_eq!(p3, root.join("a-copy (2).txt"));
        // Dirs keep the whole name as stem.
        let d1 = paste_into(&root.join("d"), &root).unwrap();
        assert_eq!(d1, root.join("d-copy"));
        // Missing source / non-dir target are errors.
        assert!(paste_into(&root.join("gone.txt"), &root).is_err());
        assert!(paste_into(&root.join("a.txt"), &root.join("a.txt")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn moves_files_and_dirs_with_guards() {
        let root = tmp_root("move");
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::write(root.join("sub").join("deep").join("b.txt"), "b").unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        // File move keeps content.
        let m1 = move_into(&root.join("a.txt"), &target).unwrap();
        assert_eq!(m1, target.join("a.txt"));
        assert_eq!(std::fs::read_to_string(&m1).unwrap(), "a");
        // Dir move carries children.
        let m2 = move_into(&root.join("sub"), &target).unwrap();
        assert_eq!(m2, target.join("sub"));
        assert_eq!(std::fs::read_to_string(m2.join("deep").join("b.txt")).unwrap(), "b");
        // Dropping into its own folder is a no-op success.
        let m3 = move_into(&target.join("a.txt"), &target).unwrap();
        assert_eq!(m3, target.join("a.txt"));
        // Clash refuses instead of overwriting.
        std::fs::write(root.join("a.txt"), "new").unwrap();
        assert!(move_into(&root.join("a.txt"), &target).is_err());
        assert!(target.join("a.txt").exists()); // untouched
        // Cannot move a folder into itself or its children.
        assert!(move_into(&target.join("sub"), &target.join("sub")).is_err());
        assert!(move_into(&target.join("sub"), &target.join("sub").join("deep")).is_err());
        assert!(target.join("sub").exists()); // untouched
        // Missing source / non-dir target are errors.
        assert!(move_into(&root.join("gone.txt"), &target).is_err());
        assert!(move_into(&target.join("a.txt"), &target.join("a.txt")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build one `$I` metadata blob: header(1) + size + FILETIME + UTF-16 path.
    fn bin_info_bytes(size: u64, deleted_at: u64, original: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&deleted_at.to_le_bytes());
        for w in original.encode_utf16() {
            b.extend_from_slice(&w.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        b
    }

    #[test]
    fn parses_bin_info_and_rejects_garbage() {
        let (size, at, path) = parse_bin_info(&bin_info_bytes(42, 777, "C:\\Users\\x\\a.txt")).unwrap();
        assert_eq!((size, at), (42, 777));
        assert_eq!(path, PathBuf::from("C:\\Users\\x\\a.txt"));
        assert!(parse_bin_info(&[0u8; 10]).is_err()); // truncated
        let mut bad = bin_info_bytes(1, 1, "C:\\x");
        bad[0] = 9; // unknown header version
        assert!(parse_bin_info(&bad).is_err());
        let mut no_null = bin_info_bytes(1, 1, "C:\\x");
        no_null.truncate(no_null.len() - 2); // cut the terminator
        assert!(parse_bin_info(&no_null).is_err());
    }

    /// Build a v2 `$I` blob: header(2) + size + FILETIME + u32 wchar length
    /// (including the null) + UTF-16 path + null.
    fn bin_info_bytes_v2(size: u64, deleted_at: u64, original: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&2u64.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&deleted_at.to_le_bytes());
        let w: Vec<u16> = original.encode_utf16().collect();
        b.extend_from_slice(&((w.len() + 1) as u32).to_le_bytes());
        for c in w {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        b
    }

    #[test]
    fn parses_bin_info_v2_layout() {
        let (size, at, path) =
            parse_bin_info(&bin_info_bytes_v2(2352118, 999, "C:\\Users\\michal\\Desktop\\a.wav")).unwrap();
        assert_eq!((size, at), (2352118, 999));
        assert_eq!(path, PathBuf::from("C:\\Users\\michal\\Desktop\\a.wav"));
        // Zero length / overrun / truncated are errors, not panics.
        let mut zero = bin_info_bytes_v2(1, 1, "C:\\x");
        zero[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_bin_info(&zero).is_err());
        let mut over = bin_info_bytes_v2(1, 1, "C:\\x");
        over.truncate(over.len() - 4);
        assert!(parse_bin_info(&over).is_err());
        assert!(parse_bin_info(&[0u8; 10]).is_err());
    }

    /// Fake `$Recycle.Bin` tree: `bin/<sid>/$I...` + `$R...` pairs.
    /// `v2` selects the header-2 `$I` layout (what current Windows writes).
    fn fake_bin(root: &Path, sid: &str, stem: &str, ext: &str, original: &str, content: &[u8], deleted_at: u64) {
        fake_bin_ver(root, sid, stem, ext, original, content, deleted_at, false);
    }

    fn fake_bin_ver(root: &Path, sid: &str, stem: &str, ext: &str, original: &str, content: &[u8], deleted_at: u64, v2: bool) {
        let dir = root.join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let info = if v2 {
            bin_info_bytes_v2(content.len() as u64, deleted_at, original)
        } else {
            bin_info_bytes(content.len() as u64, deleted_at, original)
        };
        std::fs::write(dir.join(format!("$I{}{}", stem, ext)), info).unwrap();
        std::fs::write(dir.join(format!("$R{}{}", stem, ext)), content).unwrap();
    }

    #[test]
    fn restores_latest_match_from_fake_bin() {
        let root = tmp_root("restore");
        let bin = root.join("bin");
        let orig = root.join("docs").join("notes.txt").to_string_lossy().into_owned();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        // Two deletions of the same path: the newer one wins.
        fake_bin(&bin, "SID-1", "aaaaaa", ".txt", &orig, b"old", 100);
        fake_bin(&bin, "SID-2", "bbbbbb", ".txt", &orig, b"new", 200);
        // An unrelated entry must not match.
        let other = root.join("docs").join("other.txt").to_string_lossy().into_owned();
        fake_bin(&bin, "SID-1", "cccccc", ".txt", &other, b"z", 300);
        let back = restore_from_bin_under(&bin, Path::new(&orig)).unwrap();
        assert_eq!(back, PathBuf::from(&orig));
        assert_eq!(std::fs::read_to_string(&orig).unwrap(), "new");
        assert!(!bin.join("SID-2").join("$Ibbbbbb.txt").exists()); // metadata cleaned
        assert!(!bin.join("SID-2").join("$Rbbbbbb.txt").exists()); // data moved back
        // Restoring twice refuses: the target now exists.
        fake_bin(&bin, "SID-1", "dddddd", ".txt", &orig, b"again", 400);
        assert!(restore_from_bin_under(&bin, Path::new(&orig)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_refuses_when_bin_has_no_match() {
        let root = tmp_root("restore-miss");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let orig = root.join("docs").join("gone.txt");
        assert!(restore_from_bin_under(&bin, &orig).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restores_from_v2_bin_entries() {
        let root = tmp_root("restore-v2");
        let bin = root.join("bin");
        let orig = root.join("docs").join("loop.wav").to_string_lossy().into_owned();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        fake_bin_ver(&bin, "SID-9", "xxxxxx", ".wav", &orig, b"audio", 500, true);
        let back = restore_from_bin_under(&bin, Path::new(&orig)).unwrap();
        assert_eq!(back, PathBuf::from(&orig));
        assert_eq!(std::fs::read(&orig).unwrap(), b"audio");
        assert!(!bin.join("SID-9").join("$Ixxxxxx.wav").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restores_directories_not_just_files() {
        // Regression test: `$R` entries for deleted folders are directories,
        // so the restore check must accept anything that exists, not files.
        let root = tmp_root("restore-dir");
        let bin = root.join("bin");
        let sid = bin.join("SID-7");
        std::fs::create_dir_all(&sid).unwrap();
        let orig = root.join("docs").join("Nowy folder");
        std::fs::create_dir_all(&orig).unwrap();
        let orig_s = orig.to_string_lossy().into_owned();
        std::fs::create_dir_all(sid.join("$R777777")).unwrap();
        std::fs::write(sid.join("$R777777").join("inside.txt"), "in").unwrap();
        std::fs::write(
            sid.join("$I777777"),
            bin_info_bytes_v2(0, 600, &orig_s),
        )
        .unwrap();
        // Simulate the delete: original is gone, data lives in the bin.
        std::fs::remove_dir_all(&orig).unwrap();
        let back = restore_from_bin_under(&bin, &orig).unwrap();
        assert_eq!(back, orig);
        assert_eq!(std::fs::read_to_string(back.join("inside.txt")).unwrap(), "in");
        assert!(!sid.join("$I777777").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
