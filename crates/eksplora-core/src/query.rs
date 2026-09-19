use crate::indexer::Index;
use crate::types::FileEntry;
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub limit: usize,
    pub fuzzy: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self { limit: 100, fuzzy: true }
    }
}

#[derive(Debug, Clone)]
pub struct ListOptions {
    pub limit: usize,
    pub dirs_first: bool,
    /// How many levels below `dir` to include (1 = direct children only).
    /// None = all levels.
    pub max_depth: Option<usize>,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self { limit: 2000, dirs_first: true, max_depth: Some(1) }
    }
}

#[derive(Debug, Clone)]
pub struct MatchedEntry<'a> {
    pub entry: &'a FileEntry,
    pub score: u32,
}

/// Directory-listing mode for empty queries: entries under `dir` up to
/// `max_depth` levels deep (1 = direct children only, no recursion).
/// Ordered as a real tree (DFS pre-order): each directory is immediately
/// followed by its own children, siblings dirs-first then alphabetical.
/// `child_count` = number of direct children in the index (0 for files).
#[derive(Debug, Clone, Copy)]
pub struct ListedEntry<'a> {
    pub entry: &'a FileEntry,
    pub child_count: usize,
}

pub fn list_dir<'a>(index: &'a Index, dir: &Path, opts: &ListOptions) -> Vec<ListedEntry<'a>> {
    let limit = opts.limit.clamp(1, 5000);
    let max = opts.max_depth.unwrap_or(usize::MAX).clamp(1, 64);

    let counts = count_children(&index.entries, dir);

    let items: Vec<&FileEntry> = index
        .entries
        .par_iter()
        .filter(|e| match e.path.strip_prefix(dir) {
            Ok(rel) => {
                let d = rel.components().count();
                d >= 1 && d <= max
            }
            Err(_) => false,
        })
        .collect();
    let ordered = order_dfs(items, dir, opts.dirs_first);

    let mut out: Vec<ListedEntry> = ordered
        .into_iter()
        .map(|e| ListedEntry {
            entry: e,
            child_count: if e.is_dir {
                counts.get(e.path.as_path()).copied().unwrap_or(0)
            } else {
                0
            },
        })
        .collect();
    out.truncate(limit);
    out
}

/// Direct-children counts for every directory under `dir` (no depth cap).
fn count_children<'a>(entries: &'a [FileEntry], dir: &Path) -> HashMap<&'a Path, usize> {
    let mut counts: HashMap<&'a Path, usize> = HashMap::new();
    for e in entries {
        if e.path.strip_prefix(dir).is_ok() {
            if let Some(parent) = e.path.parent() {
                *counts.entry(parent).or_default() += 1;
            }
        }
    }
    counts
}

/// Order entries as a real tree (DFS pre-order): each directory is
/// immediately followed by its own children; siblings dirs-first
/// (optional) then alphabetical. Entries unreachable from `dir` are
/// appended at the end instead of silently dropped.
fn order_dfs<'a>(items: Vec<&'a FileEntry>, dir: &Path, dirs_first: bool) -> Vec<&'a FileEntry> {
    let mut by_parent: HashMap<&'a Path, Vec<&'a FileEntry>> = HashMap::new();
    for e in items {
        if let Some(parent) = e.path.parent() {
            by_parent.entry(parent).or_default().push(e);
        }
    }
    for kids in by_parent.values_mut() {
        if dirs_first {
            kids.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then(a.name_lower.cmp(&b.name_lower))
            });
        } else {
            kids.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));
        }
    }
    let mut ordered: Vec<&FileEntry> = Vec::with_capacity(by_parent.len());
    dfs_list(dir, &by_parent, &mut ordered);

    let emitted: HashSet<&'a Path> = ordered.iter().map(|e| e.path.as_path()).collect();
    let mut orphans: Vec<&FileEntry> = by_parent
        .values()
        .flatten()
        .filter(|e| !emitted.contains(e.path.as_path()))
        .copied()
        .collect();
    orphans.sort_by(|a, b| a.path.cmp(&b.path));
    ordered.extend(orphans);
    ordered
}

fn dfs_list<'a>(
    parent: &Path,
    by_parent: &HashMap<&'a Path, Vec<&'a FileEntry>>,
    out: &mut Vec<&'a FileEntry>,
) {
    if let Some(kids) = by_parent.get(parent) {
        for k in kids {
            out.push(*k);
            if k.is_dir {
                dfs_list(&k.path, by_parent, out);
            }
        }
    }
}

/// Two-stage search:
/// 1. Fast case-insensitive substring (parallel, rayon). Good for 90% of queries.
/// 2. If fuzzy enabled and not enough hits, nucleo fuzzy over the rest.
///
/// Empty query returns no hits — callers should use `list_dir` for the
/// browse mode instead.
pub fn search<'a>(index: &'a Index, query: &str, opts: &QueryOptions) -> Vec<MatchedEntry<'a>> {
    search_inner(index, query, opts, None).unwrap_or_default()
}

/// Cancelable search for interactive use. Returns `None` when `cancel` was
/// set — the caller must drop the result (a newer query is already running).
/// The flag is checked between stages and every 4096 entries of the fuzzy
/// pass (stage 1 itself is one fast parallel pass and is not interruptible).
pub fn search_cancelable<'a>(
    index: &'a Index,
    query: &str,
    opts: &QueryOptions,
    cancel: &AtomicBool,
) -> Option<Vec<MatchedEntry<'a>>> {
    search_inner(index, query, opts, Some(cancel))
}

/// Filtered-tree search for interactive use: ranked matches (same engine as
/// `search`) plus their ancestor chains up to (excluding) `dir`, ordered as
/// a real tree — so a match in a lower layer shows in place, nested under
/// its parents. Depth filters do NOT apply here: matches show "anyways".
/// `matched` marks real hits (score > 0); ancestors are context rows with
/// score 0. `child_count` is the total direct-children count in both cases.
/// Returns `None` when `cancel` was set.
#[derive(Debug, Clone, Copy)]
pub struct TreeEntry<'a> {
    pub entry: &'a FileEntry,
    pub score: u32,
    pub matched: bool,
    pub child_count: usize,
}

pub fn search_tree_cancelable<'a>(
    index: &'a Index,
    dir: &Path,
    query: &str,
    opts: &QueryOptions,
    cancel: &AtomicBool,
) -> Option<Vec<TreeEntry<'a>>> {
    let cancelled = || cancel.load(Ordering::Relaxed);
    let matches = search_inner(index, query, opts, Some(cancel))?;
    if cancelled() {
        return None;
    }
    if matches.is_empty() {
        return Some(Vec::new());
    }

    // Visible set: matches + ancestors up to (excluding) `dir`.
    let mut visible: HashSet<&'a Path> = HashSet::with_capacity(matches.len() * 4);
    let mut scores: HashMap<&'a Path, u32> = HashMap::with_capacity(matches.len());
    for m in &matches {
        let p = m.entry.path.as_path();
        visible.insert(p);
        scores.insert(p, m.score);
        let mut par = p.parent();
        while let Some(a) = par {
            if a == dir || a.strip_prefix(dir).is_err() {
                break;
            }
            visible.insert(a);
            par = a.parent();
        }
    }
    if cancelled() {
        return None;
    }

    let counts = count_children(&index.entries, dir);
    let items: Vec<&'a FileEntry> = index
        .entries
        .par_iter()
        .filter(|e| visible.contains(e.path.as_path()))
        .collect();
    if cancelled() {
        return None;
    }
    let ordered = order_dfs(items, dir, true);
    Some(
        ordered
            .into_iter()
            .map(|e| {
                let p = e.path.as_path();
                TreeEntry {
                    entry: e,
                    score: scores.get(p).copied().unwrap_or(0),
                    matched: scores.contains_key(p),
                    child_count: if e.is_dir {
                        counts.get(p).copied().unwrap_or(0)
                    } else {
                        0
                    },
                }
            })
            .collect(),
    )
}

fn search_inner<'a>(
    index: &'a Index,
    query: &str,
    opts: &QueryOptions,
    cancel: Option<&AtomicBool>,
) -> Option<Vec<MatchedEntry<'a>>> {
    let cancelled = || cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false);
    if cancelled() {
        return None;
    }

    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(Vec::new());
    }
    let limit = opts.limit.clamp(1, 5000);

    // Stage 1: substring, parallel. Score exact-name-match higher.
    let mut hits: Vec<MatchedEntry<'a>> = index
        .entries
        .par_iter()
        .filter_map(|e| {
            if e.name_lower.contains(&q) {
                let score = if e.name_lower == q {
                    10_000
                } else if e.name_lower.starts_with(&q) {
                    5_000
                } else {
                    1_000
                };
                Some(MatchedEntry { entry: e, score })
            } else {
                None
            }
        })
        .collect();

    if cancelled() {
        return None;
    }

    // Sort substring hits: dirs/files mixed, shortest name first (usually best).
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.entry.name.len().cmp(&b.entry.name.len()))
    });
    if hits.len() >= limit || !opts.fuzzy {
        hits.truncate(limit);
        return Some(hits);
    }

    // Stage 2: fuzzy fill with nucleo (fzf-quality), in chunks so a
    // newer keystroke can cancel a slow pass over a huge index.
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut fuzzy: Vec<(u16, &'a FileEntry)> = Vec::new();
    for chunk in index.entries.chunks(4096) {
        if cancelled() {
            return None;
        }
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        let q32 = Utf32Str::new(&q, &mut buf1);
        for e in chunk {
            // Skip already-matched to avoid dupes.
            if e.name_lower.contains(&q) {
                continue;
            }
            let h32 = Utf32Str::new(&e.name_lower, &mut buf2);
            if let Some(score) = matcher.fuzzy_match(h32, q32) {
                fuzzy.push((score, e));
            }
        }
    }
    if cancelled() {
        return None;
    }
    fuzzy.sort_by(|a, b| b.0.cmp(&a.0));
    for (score, entry) in fuzzy.into_iter().take(limit - hits.len()) {
        hits.push(MatchedEntry {
            entry,
            score: u32::from(score),
        });
    }
    Some(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(p: &str, is_dir: bool) -> FileEntry {
        FileEntry::from_path(PathBuf::from(p), is_dir, 0, 0, 0)
    }

    fn sample_index() -> Index {
        let mut idx = Index::new();
        idx.upsert(entry("C:\\Root", true));
        idx.upsert(entry("C:\\Root\\a.txt", false));
        idx.upsert(entry("C:\\Root\\sub", true));
        idx.upsert(entry("C:\\Root\\sub\\deep.txt", false));
        idx
    }

    #[test]
    fn lists_direct_children_only() {
        let idx = sample_index();
        let kids = list_dir(
            &idx,
            Path::new("C:\\Root"),
            &ListOptions { limit: 100, dirs_first: true, max_depth: Some(1) },
        );
        let names: Vec<&str> = kids.iter().map(|e| e.entry.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
        assert!(!names.contains(&"deep.txt"));
        assert!(!names.contains(&"Root"));
        // dirs_first: "sub" before "a.txt"
        assert_eq!(kids[0].entry.name, "sub");
    }

    #[test]
    fn max_depth_two_includes_grandchildren() {
        let idx = sample_index();
        let kids = list_dir(
            &idx,
            Path::new("C:\\Root"),
            &ListOptions { limit: 100, dirs_first: true, max_depth: Some(2) },
        );
        let names: Vec<&str> = kids.iter().map(|e| e.entry.name.as_str()).collect();
        assert!(names.contains(&"deep.txt"));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn children_nest_directly_under_parents() {
        let idx = sample_index();
        let kids = list_dir(
            &idx,
            Path::new("C:\\Root"),
            &ListOptions { limit: 100, dirs_first: true, max_depth: Some(2) },
        );
        let names: Vec<&str> = kids.iter().map(|e| e.entry.name.as_str()).collect();
        // DFS pre-order: sub is immediately followed by its child deep.txt,
        // not pushed to the bottom after the whole first layer.
        let sub_pos = names.iter().position(|&n| n == "sub").unwrap();
        assert_eq!(names[sub_pos + 1], "deep.txt");
    }

    #[test]
    fn child_counts_cover_direct_children() {
        let idx = sample_index();
        let kids = list_dir(
            &idx,
            Path::new("C:\\Root"),
            &ListOptions { limit: 100, dirs_first: true, max_depth: Some(1) },
        );
        let sub = kids.iter().find(|e| e.entry.name == "sub").unwrap();
        assert_eq!(sub.child_count, 1); // deep.txt, even though depth=1 hides it
        let file = kids.iter().find(|e| e.entry.name == "a.txt").unwrap();
        assert_eq!(file.child_count, 0);
    }

    #[test]
    fn unlimited_depth_includes_all() {
        let idx = sample_index();
        let kids = list_dir(&idx, Path::new("C:\\Root"), &ListOptions { limit: 100, dirs_first: true, max_depth: None });
        assert_eq!(kids.len(), 3);
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let idx = sample_index();
        let hits = search(&idx, "   ", &QueryOptions::default());
        assert!(hits.is_empty());
    }

    #[test]
    fn substring_search_still_works() {
        let idx = sample_index();
        let hits = search(&idx, "deep", &QueryOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "deep.txt");
    }

    #[test]
    fn pre_cancelled_search_returns_none() {
        let idx = sample_index();
        let flag = AtomicBool::new(true);
        assert!(search_cancelable(&idx, "a", &QueryOptions::default(), &flag).is_none());
    }

    #[test]
    fn uncancelled_search_returns_some() {
        let idx = sample_index();
        let flag = AtomicBool::new(false);
        let hits = search_cancelable(&idx, "a", &QueryOptions::default(), &flag);
        assert!(hits.is_some());
    }

    #[test]
    fn search_tree_nests_match_under_ancestors() {
        let idx = sample_index();
        let flag = AtomicBool::new(false);
        let rows = search_tree_cancelable(&idx, Path::new("C:\\Root"), "deep", &QueryOptions::default(), &flag)
            .expect("not cancelled");
        let names: Vec<&str> = rows.iter().map(|r| r.entry.name.as_str()).collect();
        // Match plus its ancestor chain, nested: sub directly above deep.txt.
        assert_eq!(names, vec!["sub", "deep.txt"]);
        assert!(!rows[0].matched);
        assert_eq!(rows[0].score, 0);
        assert!(rows[1].matched);
        assert!(rows[1].score > 0);
        assert_eq!(rows[0].child_count, 1);
    }

    #[test]
    fn search_tree_empty_query_gives_no_rows() {
        let idx = sample_index();
        let flag = AtomicBool::new(false);
        let rows = search_tree_cancelable(&idx, Path::new("C:\\Root"), "   ", &QueryOptions::default(), &flag)
            .expect("not cancelled");
        assert!(rows.is_empty());
    }

    #[test]
    fn search_tree_cancelled_returns_none() {
        let idx = sample_index();
        let flag = AtomicBool::new(true);
        assert!(search_tree_cancelable(&idx, Path::new("C:\\Root"), "deep", &QueryOptions::default(), &flag).is_none());
    }
}
