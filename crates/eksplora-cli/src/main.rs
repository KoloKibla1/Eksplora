use anyhow::Result;
use clap::{Parser, Subcommand};
use eksplora_core::{QueryOptions, ScanOptions};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "eksplora", about = "Eksplora perf base — scan/index/search")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parallel scan, print stats. The throughput baseline.
    Scan {
        path: PathBuf,
        /// Index dependency/cache dirs too (node_modules, .git, ...).
        #[arg(long)]
        no_skip_caches: bool,
    },
    /// Scan + build in-memory index + save sqlite. Measures index cost.
    Index {
        path: PathBuf,
        db: Option<PathBuf>,
        /// Index dependency/cache dirs too (node_modules, .git, ...).
        #[arg(long)]
        no_skip_caches: bool,
    },
    /// sqlite save/load + query benchmark. Prints rows/sec and ms.
    BenchDb {
        path: PathBuf,
        #[arg(long, default_value = "eksplora-bench.db")]
        db: PathBuf,
    },
    /// Scan then run one query, print top hits + query latency.
    /// Empty query = browse mode: list entries up to --depth levels deep.
    Search {
        path: PathBuf,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        fuzzy: bool,
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Index dependency/cache dirs too (node_modules, .git, ...).
        #[arg(long)]
        no_skip_caches: bool,
    },
    /// Watch a dir and print coalesced events.
    Watch { path: PathBuf },
    /// Build index once, then apply watcher events incrementally (live).
    WatchIndex { path: PathBuf },
    /// Show known folders + volume/USN readiness (integration check).
    Sysinfo { path: Option<PathBuf> },
    /// Query USN journal info (needs admin + NTFS, else clear error).
    UsnQuery { path: Option<PathBuf> },
    /// Read USN deltas (needs admin + NTFS). Prints FRN/name/reason.
    UsnDelta {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        start_usn: Option<i64>,
    },
    /// Complete a path prefix to child directories (Tab backend).
    Complete {
        input: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Scan { path, no_skip_caches } => {
            let opts = ScanOptions { skip_caches: !no_skip_caches, ..Default::default() };
            let (entries, stats) = eksplora_core::scan(&path, &opts);
            println!(
                "scan {} -> files={} dirs={} errors={} skipped={} in {}ms ({} entries/s), total_entries={}",
                path.display(),
                stats.files,
                stats.dirs,
                stats.errors,
                stats.skipped,
                stats.duration_ms,
                stats.files_per_sec,
                entries.len()
            );
        }
        Cmd::Index { path, db, no_skip_caches } => {
            let t = Instant::now();
            let opts = ScanOptions { skip_caches: !no_skip_caches, ..Default::default() };
            let (entries, stats) = eksplora_core::scan(&path, &opts);
            let idx = eksplora_core::Index::from_entries(entries);
            let build_ms = t.elapsed().as_millis();
            println!(
                "indexed {} entries (scan {}ms + build total {}ms)",
                idx.len(),
                stats.duration_ms,
                build_ms
            );
            if let Some(db) = db {
                let t2 = Instant::now();
                idx.save_to_sqlite(&db)?;
                println!("saved to {} in {}ms", db.display(), t2.elapsed().as_millis());
            }
        }
        Cmd::BenchDb { path, db } => {
            let (entries, stats) = eksplora_core::scan(&path, &ScanOptions::default());
            let idx = eksplora_core::Index::from_entries(entries);
            println!(
                "scan: {} entries in {}ms ({} entries/s)",
                idx.len(),
                stats.duration_ms,
                stats.files_per_sec
            );
            let t = Instant::now();
            idx.save_to_sqlite(&db)?;
            let save_ms = t.elapsed().as_millis().max(1);
            let fsize = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
            println!(
                "save: {} rows in {}ms ({} rows/s), file={} bytes -> {}",
                idx.len(),
                save_ms,
                idx.len() as u128 * 1000 / save_ms,
                fsize,
                db.display()
            );
            let t = Instant::now();
            let loaded = eksplora_core::Index::load_from_sqlite(&db)?;
            let load_ms = t.elapsed().as_millis().max(1);
            println!(
                "load: {} rows in {}ms ({} rows/s)",
                loaded.len(),
                load_ms,
                loaded.len() as u128 * 1000 / load_ms
            );
            for q in ["e", "test", "eksplora"] {
                let t = Instant::now();
                let hits = eksplora_core::search(
                    &loaded,
                    q,
                    &QueryOptions { limit: 20, fuzzy: true },
                );
                let ms = t.elapsed().as_micros() as f64 / 1000.0;
                println!("query {:?} -> {} hits in {:.2}ms", q, hits.len(), ms);
            }
        }
        Cmd::Search {
            path,
            query,
            limit,
            fuzzy,
            depth,
            no_skip_caches,
        } => {
            let scan_opts = ScanOptions { skip_caches: !no_skip_caches, ..Default::default() };
            if query.trim().is_empty() {
                // Browse mode: entries up to `depth` levels deep, DFS tree order.
                let max = depth.clamp(1, 32);
                let opts = ScanOptions { max_depth: Some(max), skip_caches: !no_skip_caches, ..Default::default() };
                let (entries, stats) = eksplora_core::scan(&path, &opts);
                let refs: Vec<&eksplora_core::FileEntry> = entries
                    .iter()
                    .filter(|e| e.path.as_path() != path.as_path())
                    .collect();
                // Sibling groups (dirs first, alphabetical).
                let mut by_parent: std::collections::HashMap<&std::path::Path, Vec<&eksplora_core::FileEntry>> =
                    std::collections::HashMap::new();
                for e in refs {
                    if let Some(parent) = e.path.parent() {
                        by_parent.entry(parent).or_default().push(e);
                    }
                }
                for kids in by_parent.values_mut() {
                    kids.sort_by(|a, b| {
                        b.is_dir
                            .cmp(&a.is_dir)
                            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    });
                }
                // DFS pre-order: each dir immediately followed by its children.
                fn dfs<'a>(
                    parent: &std::path::Path,
                    by_parent: &std::collections::HashMap<&'a std::path::Path, Vec<&'a eksplora_core::FileEntry>>,
                    out: &mut Vec<&'a eksplora_core::FileEntry>,
                ) {
                    if let Some(kids) = by_parent.get(parent) {
                        for k in kids {
                            out.push(*k);
                            if k.is_dir {
                                dfs(&k.path, by_parent, out);
                            }
                        }
                    }
                }
                let mut ordered: Vec<&eksplora_core::FileEntry> = Vec::new();
                dfs(&path, &by_parent, &mut ordered);
                // Direct-children counts from the scanned set; dirs on the
                // depth boundary are counted via a single read_dir each.
                let mut counts: std::collections::HashMap<&std::path::Path, usize> =
                    std::collections::HashMap::new();
                for e in ordered.iter() {
                    if let Some(parent) = e.path.parent() {
                        *counts.entry(parent).or_default() += 1;
                    }
                }
                let shown = ordered.len().min(limit);
                println!(
                    "listing {} -> {} entries ({} level(s), {}ms)",
                    path.display(),
                    ordered.len(),
                    max,
                    stats.duration_ms
                );
                for e in ordered.iter().take(shown) {
                    let d = e
                        .path
                        .strip_prefix(&path)
                        .map(|r| r.components().count())
                        .unwrap_or(1)
                        .max(1);
                    let n = if e.is_dir {
                        if d == max {
                            // Boundary: children weren't scanned, count directly.
                            std::fs::read_dir(&e.path).map(|rd| rd.count()).unwrap_or(0)
                        } else {
                            counts.get(e.path.as_path()).copied().unwrap_or(0)
                        }
                    } else {
                        0
                    };
                    println!(
                        "  {}{}[{}] {}",
                        "  ".repeat(d - 1),
                        if e.is_dir {
                            format!("▸ {:>4} ", n)
                        } else {
                            "•      ".to_string()
                        },
                        if e.is_dir { "dir " } else { "file" },
                        e.path.display()
                    );
                }
                return Ok(());
            }
            let (entries, stats) = eksplora_core::scan(&path, &scan_opts);
            let idx = eksplora_core::Index::from_entries(entries);
            let t = Instant::now();
            let hits = eksplora_core::search(
                &idx,
                &query,
                &QueryOptions { limit, fuzzy },
            );
            let qms = t.elapsed().as_micros() as f64 / 1000.0;
            println!(
                "index={} entries (scan {}ms), query {:?} -> {} hits in {:.2}ms",
                idx.len(),
                stats.duration_ms,
                query,
                hits.len(),
                qms
            );
            for h in hits.iter().take(limit) {
                println!("  [{}] {}", h.score, h.entry.path.display());
            }
        }
        Cmd::Watch { path } => {
            println!("watching {} (Ctrl+C to stop)...", path.display());
            let handle = eksplora_core::watcher::watch(&path)?;
            for ev in handle.rx.iter() {
                println!("{:?} {:?}", ev.kind, ev.paths);
            }
        }
        Cmd::WatchIndex { path } => {
            let (entries, stats) = eksplora_core::scan(&path, &ScanOptions::default());
            let mut idx = eksplora_core::Index::from_entries(entries);
            println!(
                "base index: {} entries in {}ms. Watching {} (Ctrl+C)...",
                idx.len(),
                stats.duration_ms,
                path.display()
            );
            let handle = eksplora_core::watcher::watch(&path)?;
            for ev in handle.rx.iter() {
                let t = Instant::now();
                let (up, del) = idx.apply_event_paths(&ev.paths);
                println!(
                    "[{:?}] paths={} upserted={} removed={} index_len={} apply={}ms",
                    ev.kind,
                    ev.paths.len(),
                    up,
                    del,
                    idx.len(),
                    t.elapsed().as_millis()
                );
                for p in ev.paths.iter().take(5) {
                    println!("  {}", p.display());
                }
            }
        }
        Cmd::Complete { input, limit } => {
            let comps = eksplora_core::complete_path(input.as_deref().unwrap_or(""), limit);
            println!("{} completion(s) for {:?}", comps.len(), input.as_deref().unwrap_or(""));
            for c in comps.iter().take(limit) {
                println!("  {}  ({})", c.path, c.name);
            }
        }
        Cmd::Sysinfo { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("C:\\"));
            for (name, p) in eksplora_core::windows_integration::known_folders()? {
                println!("{:<15} {}", name, p.display());
            }
            println!("{}", eksplora_core::windows_integration::usn_status(&root));
            println!("{}", eksplora_core::usn::status_string(&root));
        }
        Cmd::UsnQuery { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("C:\\"));
            match eksplora_core::usn::query_journal(&root) {
                Ok(j) => println!(
                    "journal_id={:#x} first_usn={} next_usn={} lowest_valid={} max_usn={}",
                    j.journal_id, j.first_usn, j.next_usn, j.lowest_valid_usn, j.max_usn
                ),
                Err(e) => println!("USN query failed: {:#}", e),
            }
        }
        Cmd::UsnDelta { path, limit, start_usn } => {
            let root = path.unwrap_or_else(|| PathBuf::from("C:\\"));
            match eksplora_core::usn::read_deltas(&root, start_usn, limit) {
                Ok((entries, next)) => {
                    println!("{} deltas, next_usn={}", entries.len(), next);
                    for e in entries.iter().take(limit) {
                        println!(
                            "  usn={} frn={:#x} parent={:#x} dir={} {} {}",
                            e.usn, e.frn, e.parent_frn, e.is_dir, e.reason_str, e.name
                        );
                    }
                    println!("resume with: --start-usn {}", next);
                }
                Err(e) => println!("USN read failed: {:#}", e),
            }
        }
    }
    Ok(())
}
