use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    /// File name only (no parent), e.g. `notes.txt`
    pub name: String,
    /// Lowercased name for fast case-insensitive search. Precomputed once at scan.
    pub name_lower: String,
    pub ext: String,
    pub size: u64,
    /// Unix millis. 0 if unknown.
    pub mtime_ms: i64,
    pub is_dir: bool,
    /// Raw Win32 attributes (FILE_ATTRIBUTE_*). 0 on non-Windows / unknown.
    pub attributes: u32,
}

impl FileEntry {
    pub fn from_path(path: PathBuf, is_dir: bool, size: u64, mtime_ms: i64, attributes: u32) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let name_lower = name.to_lowercase();
        Self {
            path,
            name,
            name_lower,
            ext,
            size,
            mtime_ms,
            is_dir,
            attributes,
        }
    }

    #[inline]
    pub fn is_hidden(&self) -> bool {
        // 0x2 = FILE_ATTRIBUTE_HIDDEN
        self.attributes & 0x2 != 0 || self.name.starts_with('.')
    }
}
