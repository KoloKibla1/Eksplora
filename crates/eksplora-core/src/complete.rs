//! Path autocompletion for the path field: Tab shows suggestions based on
//! the input, Tab cycles, Enter confirms. Directories only — the field
//! selects a scan root, so files would be dead ends.

use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Completion {
    /// Full path to insert on confirm.
    pub path: String,
    /// Short label shown in the popup.
    pub name: String,
    pub is_dir: bool,
}

fn completion_for(path: &Path) -> Option<Completion> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    Some(Completion {
        path: path.to_string_lossy().into_owned(),
        name,
        is_dir: true,
    })
}

#[cfg(windows)]
fn list_drives() -> Vec<Completion> {
    // Bitmask from GetLogicalDrives: bit 0 = A:, 1 = B:, ...
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let path = format!("{}:\\", (b'A' + i) as char);
            Completion { path: path.clone(), name: path, is_dir: true }
        })
        .collect()
}

#[cfg(not(windows))]
fn list_drives() -> Vec<Completion> {
    vec![Completion { path: "/".to_string(), name: "/".to_string(), is_dir: true }]
}

fn known_root_completions() -> Vec<Completion> {
    let mut out = list_drives();
    if let Ok(folders) = crate::windows_integration::known_folders() {
        for (label, path) in folders {
            if let Some(mut c) = completion_for(&path) {
                c.name = format!("{} ({})", label, c.name);
                if !out.iter().any(|e| e.path == c.path) {
                    out.push(c);
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    out
}

fn expand_tilde(input: &str) -> String {
    if input == "~" || input.starts_with("~/") || input.starts_with("~\\") {
        if let Ok(folders) = crate::windows_integration::known_folders() {
            if let Some((_, profile)) = folders.iter().find(|(n, _)| n == "Profile") {
                return format!("{}{}", profile.to_string_lossy(), &input[1..]);
            }
        }
    }
    input.to_string()
}

/// Complete `input` to child directories. Empty input suggests drives +
/// known folders; input without a separator filters those by prefix;
/// otherwise the parent dir is listed and filtered by the partial name.
/// Never errors on missing/unreadable dirs — returns what exists.
pub fn complete_path(input: &str, limit: usize) -> Vec<Completion> {
    let limit = limit.clamp(1, 100);
    let t = expand_tilde(input.trim());

    if t.is_empty() {
        return known_root_completions().into_iter().take(limit).collect();
    }

    let last_sep = t.rfind(|c| c == '/' || c == '\\');
    let Some(pos) = last_sep else {
        // No separator yet: filter drives + known folders by prefix.
        let q = t.to_lowercase();
        return known_root_completions()
            .into_iter()
            .filter(|c| {
                c.path.to_lowercase().starts_with(&q) || c.name.to_lowercase().starts_with(&q)
            })
            .take(limit)
            .collect();
    };

    let (parent_str, partial) = t.split_at(pos + 1);
    let parent = Path::new(if parent_str.is_empty() { "\\" } else { parent_str });
    let q = partial.to_lowercase();
    let read = match std::fs::read_dir(parent) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Completion> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|f| f.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&q) {
                return None;
            }
            completion_for(&e.path())
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eksplora-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn empty_input_suggests_roots() {
        let comps = complete_path("", 100);
        assert!(!comps.is_empty());
        assert!(comps.iter().any(|c| c.path.ends_with(":\\")));
        let _ = comps;
    }

    #[test]
    fn completes_child_dirs_not_files() {
        let root = tmp_root("complete");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("alpine")).unwrap();
        std::fs::write(root.join("alpha.txt"), "x").unwrap();
        let prefix = format!("{}{}alp", root.display(), std::path::MAIN_SEPARATOR);
        let comps = complete_path(&prefix, 50);
        let names: Vec<&str> = comps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"alpine"));
        assert!(!names.contains(&"alpha.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trailing_separator_lists_all_child_dirs() {
        let root = tmp_root("completetrail");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let prefix = format!("{}{}", root.display(), std::path::MAIN_SEPARATOR);
        let comps = complete_path(&prefix, 50);
        assert!(comps.iter().any(|c| c.name == "sub"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_parent_gives_no_completions() {
        let comps = complete_path("C:\\definitely-not-here-eksplora\\par", 50);
        assert!(comps.is_empty());
    }
}
