# Eksplora --- Architecture & Fast File Search Plan

## 1. Core stack

**Recommended stack:**

-   **Rust** --- filesystem scanning, indexing, Windows API integration,
    search engine/backend
-   **Tauri 2** --- desktop application shell and Rust ↔ frontend IPC
-   **React + Vite** --- UI
-   **SQLite + FTS5** --- persistent index/storage
-   **nucleo** --- fast fuzzy filename matching
-   **notify** --- filesystem change watching
-   **Windows APIs** --- Explorer integration and optional NTFS
    optimizations

This is a strong fit for a Windows-first, search-oriented file manager.
Tauri keeps the UI iteration fast while allowing the
performance-critical filesystem work to stay in Rust.

------------------------------------------------------------------------

## 2. Search architecture: three tiers

### Tier 1 --- Cold scan

Run a full filesystem scan when a location is indexed for the first
time.

Recommended approach:

-   parallel directory walking
-   avoid naïve recursive `std::fs::read_dir` as the primary scanner
-   consider `jwalk` or a custom work queue using `rayon`/`crossbeam`
-   on Windows, investigate `FindFirstFileExW` with
    `FIND_FIRST_EX_LARGE_FETCH`
-   avoid unnecessary metadata/stat calls during the first pass
-   tolerate `Access Denied` and continue scanning

The objective is to build the initial filename index as quickly as
practical.

The exact scan time will depend heavily on disk type, filesystem,
directory structure, antivirus activity, permissions, and number of
files. Treat figures such as "500k--1M files in 10--30 seconds" as
performance targets to benchmark, not guaranteed results.

------------------------------------------------------------------------

### Tier 2 --- Incremental updates

A fast search application should not repeatedly rescan the entire disk.

#### Normal mode

Use filesystem notifications, for example through `notify` and Windows
`ReadDirectoryChangesW`.

Advantages:

-   does not normally require administrator privileges
-   works well for keeping an active directory tree up to date

Limitation:

-   notifications are not a complete historical record
-   changes can be missed while the application is closed

Therefore, the application should periodically reconcile the index with
the filesystem.

#### NTFS USN Journal

For NTFS volumes, investigate the USN Journal APIs:

-   `FSCTL_QUERY_USN_JOURNAL`
-   `FSCTL_ENUM_USN_DATA`

The journal can provide filesystem-change information without requiring
a complete directory scan after every restart.

Important:

-   availability and required privileges depend on the operation and
    system configuration
-   it is NTFS-specific
-   it is not a universal replacement for filesystem scanning

Recommended design:

``` text
Application starts
        │
        ├── USN available and usable?
        │        ├── YES → process journal delta
        │        └── NO  → watcher + reconciliation scan
        │
        └── normal filesystem watcher remains active
```

USN should be an optimization, not a hard dependency.

------------------------------------------------------------------------

### Tier 3 --- Query

The search path should be entirely separate from the scanning path.

Store at minimum:

``` text
path
name
normalized/lowercase name
extension
size
mtime
```

SQLite provides persistence.

For filename search:

-   load a compact search structure into RAM
-   use `nucleo` for fuzzy matching
-   avoid running a full SQL query for every keystroke if the complete
    filename index can already be searched efficiently in memory

Ranking can follow a deterministic order such as:

``` text
exact filename
    ↓
prefix
    ↓
substring
    ↓
fuzzy match
```

Additional ranking signals can include:

-   path depth
-   file extension
-   recently used/opened files
-   directory relevance
-   match position

Do not depend on "accessed time" alone because Windows access-time
behavior can be disabled or unreliable for ranking purposes.

------------------------------------------------------------------------

## 3. Filename search vs. content search

These are different problems.

### Filename search

This should be the primary feature.

Target:

``` text
User types:
pho

Results appear immediately:
Photos
photo.jpg
Photoshop.exe
C:\Users\...\Photos
```

The filename index should be optimized for extremely low query latency.

### Content search

Content indexing is significantly more expensive.

It should be a separate subsystem introduced after filename search works
well.

Possible later options:

-   Windows Search integration
-   dedicated text extraction/indexing
-   `tantivy` or another full-text engine if the project eventually
    needs a large custom content index

Do not introduce a heavy content-search engine into v1 unless there is a
concrete requirement for it.

------------------------------------------------------------------------

## 4. Windows compatibility

The application should work without administrator privileges by default.

### Filesystems

  -----------------------------------------------------------------------
  Filesystem/location                 Strategy
  ----------------------------------- -----------------------------------
  NTFS                                Full scan + watcher + optional USN
                                      optimization

  ReFS                                Scan/watcher where supported; no
                                      NTFS USN dependency

  exFAT/FAT32                         Scan + available filesystem
                                      notifications

  USB drives                          Scan + watcher where available

  SMB/network shares                  Scan/watch where supported; expect
                                      different behavior

  OneDrive                            Avoid forcing hydration of
                                      cloud-only files

  SharePoint/Cloud placeholders       Read metadata without unnecessarily
                                      downloading data

  `\\wsl$`                            Treat carefully; live access can be
                                      supported without trying to force a
                                      normal Windows disk-indexing model
  -----------------------------------------------------------------------

Never make access to one filesystem or special path type a reason for
the entire indexer to fail.

------------------------------------------------------------------------

## 5. Permissions

Default behavior:

``` text
No administrator privileges
        ↓
Scan everything accessible to the current user
        ↓
Access Denied
        ↓
Skip + record/log the error
        ↓
Continue scanning
```

An optional elevated component can later provide additional
NTFS-specific functionality.

The application should never require elevation merely to perform basic
filename search.

------------------------------------------------------------------------

## 6. Tauri + React architecture

Recommended structure:

``` text
React / Vite
    │
    │ Tauri IPC
    ▼
Rust application
    │
    ├── Scanner
    │
    ├── Watcher
    │
    ├── USN Journal adapter
    │
    ├── Indexer
    │       └── SQLite
    │
    ├── In-memory filename index
    │
    └── Query engine
            └── nucleo
```

The frontend should never directly scan the filesystem.

Rust owns:

-   filesystem access
-   indexing
-   search
-   Windows APIs
-   process/file operations

React owns:

-   search box
-   result list
-   keyboard navigation
-   file previews
-   settings
-   UI state

------------------------------------------------------------------------

## 7. IPC/search performance

Every keystroke should not create unnecessary work.

Recommended flow:

``` text
Keyboard input
      ↓
~50 ms debounce
      ↓
Cancel previous query
      ↓
Rust search
      ↓
Return only visible/top results
      ↓
Virtualized React list
```

Use cancellation so that:

``` text
query = "photo"
```

does not finish after:

``` text
query = "photos"
```

and overwrite the newer results.

For large result sets use virtualization.

Never render tens of thousands of DOM nodes just because the search
returned tens of thousands of files.

------------------------------------------------------------------------

## 8. UI performance

Recommended React components/libraries:

-   **TanStack Virtual** for large result lists
-   **cmdk** for command/search-palette behavior

Icons and thumbnails should be loaded lazily.

Example:

``` text
Search result
    ↓
show filename immediately
    ↓
load icon
    ↓
load thumbnail only if needed/visible
```

The search result should not wait for thumbnail generation.

------------------------------------------------------------------------

## 9. Explorer integration

Use Windows APIs where native behavior matters.

Potential APIs/components:

-   `SHGetKnownFolderPath` --- known Windows folders
-   `IShellItemImageFactory` --- shell thumbnails/icons
-   `IFileOperation` --- native file operations
-   `SHChangeNotify` --- notify Explorer about changes

For v1, use a custom context menu rather than attempting to reproduce
every Explorer shell extension.

Native drag-and-drop, overlay icons, preview handlers and complete
shell-extension parity can be added later if required.

------------------------------------------------------------------------

## 10. Long paths and Unicode

Windows paths require careful handling.

The application should:

-   use Unicode Windows APIs such as the `W` variants
-   handle case-insensitive Windows filenames correctly
-   normalize names consistently for search
-   use long-path support where available
-   configure the application manifest appropriately
-   avoid assuming that a path fits into traditional `MAX_PATH` limits

Do not corrupt or simplify filenames merely for indexing.

------------------------------------------------------------------------

## 11. Build targets

Initial target:

``` text
x86_64-pc-windows-msvc
```

Later:

``` text
aarch64-pc-windows-msvc
```

WebView2 should be handled through an appropriate Evergreen/runtime
strategy so the application can run on supported Windows installations
where the runtime is not already present.

Test on both clean and typical Windows installations rather than
assuming every Windows 10 PC has identical WebView2 state.

------------------------------------------------------------------------

## 12. Performance goals

Treat these as engineering targets, not guarantees:

### Initial indexing

Goal:

``` text
500k–1M files
→ benchmark toward tens of seconds on a modern NVMe SSD
```

The benchmark must record:

-   filesystem
-   drive type
-   CPU
-   RAM
-   number of files
-   number of directories
-   antivirus state
-   access-denied count

### Search

Target:

``` text
typing
  ↓
debounce
  ↓
Rust query
  ↓
results
```

The UI should feel effectively instantaneous for normal filename
searches.

Measure actual latency instead of assuming a fixed number such as
`<20 ms` on every machine.

------------------------------------------------------------------------

# Development roadmap

## Phase 1 --- Rust CLI

Do **not** start with the full Tauri UI.

Build:

``` text
explora-cli
```

Responsibilities:

1.  Select root directory
2.  Parallel scan
3.  Extract filename/path metadata
4.  Insert into SQLite
5.  Report:
    -   files scanned
    -   directories scanned
    -   skipped paths
    -   errors
    -   total time

Example:

``` text
explora-cli scan C:\
```

This phase gives a measurable foundation for the entire project.

------------------------------------------------------------------------

## Phase 2 --- Search engine

Add:

``` text
explora-cli search "photo"
```

Implement:

-   exact matching
-   prefix matching
-   substring matching
-   fuzzy matching
-   deterministic ranking
-   query cancellation

Benchmark against the size of the actual index.

------------------------------------------------------------------------

## Phase 3 --- Incremental indexing

Add:

``` text
ReadDirectoryChangesW / notify
```

Then add NTFS USN Journal support as an optimization.

Test:

-   file creation
-   deletion
-   rename
-   move
-   modification
-   application restart
-   changes while application is closed

------------------------------------------------------------------------

## Phase 4 --- Tauri + React

Only after the backend works:

``` text
Rust backend
      ↕
Tauri IPC
      ↕
React UI
```

Create:

-   global/search-focused UI
-   keyboard navigation
-   virtualized results
-   file icons
-   basic file actions

------------------------------------------------------------------------

## Phase 5 --- Windows integration

Add:

-   Open
-   Open containing folder
-   Copy
-   Move
-   Delete
-   Recycle Bin
-   Rename
-   basic context menu
-   Explorer refresh notifications

------------------------------------------------------------------------

## Phase 6 --- Advanced features

Possible future additions:

-   content search
-   previews
-   thumbnails
-   duplicate-file detection
-   saved searches
-   filters
-   extensions
-   file type categories
-   recent files
-   indexing statistics
-   optional elevated helper
-   more complete Explorer integration

------------------------------------------------------------------------

# Final architecture

``` text
                    ┌─────────────────────┐
                    │      React UI       │
                    │  Vite + TanStack    │
                    │      Virtual        │
                    └──────────┬──────────┘
                               │
                          Tauri IPC
                               │
                    ┌──────────▼──────────┐
                    │     Rust Core       │
                    ├─────────────────────┤
                    │ Query Engine         │
                    │      ↓               │
                    │    nucleo            │
                    ├─────────────────────┤
                    │ In-memory Index      │
                    ├─────────────────────┤
                    │ SQLite + FTS5        │
                    ├─────────────────────┤
                    │ Indexer              │
                    ├─────────────────────┤
                    │ Watcher              │
                    │      ↓               │
                    │ ReadDirectoryChanges │
                    ├─────────────────────┤
                    │ USN Journal adapter  │
                    ├─────────────────────┤
                    │ Parallel Scanner     │
                    └──────────┬──────────┘
                               │
                       Windows filesystem
```

## Recommendation

The overall architecture is sound.

The main correction is to treat several performance numbers and Windows
capability claims as **benchmarks/implementation targets rather than
guarantees**. The most important design decision is separating:

1.  initial scanning,
2.  incremental indexing,
3.  querying.

Build the Rust scanner and SQLite index first. Once that backend is fast
and measurable, put Tauri + React on top of it.

**Project working name: `Eksplora`.**
