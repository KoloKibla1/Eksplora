import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useVirtualizer } from "@tanstack/react-virtual";

type SearchHit = {
  path: string;
  name: string;
  score: number;
  is_dir: boolean;
  size: number;
  child_count: number;
  matched: boolean;
};

type ScanProgress = {
  path: string;
  files: number;
  dirs: number;
  // Live per-folder counts from the walk stream: cumulative
  // (dir path, direct children walked so far), only for dirs that
  // changed since the previous tick.
  counts: [string, number][];
};

type ScanPartial = {
  path: string;
  len: number;
  files: number;
  dirs: number;
  duration_ms: number;
};

type ScanDone = {
  path: string;
  len: number;
  files: number;
  dirs: number;
  duration_ms: number;
  cancelled: boolean;
};

type ScanInfo = {
  len: number;
  files: number;
  dirs: number;
  duration_ms: number;
};

// Normalize for comparison: trim, drop trailing slashes (except `C:\`),
// uppercase drive letter. Avoids rescans over cosmetic differences.
function normPath(p: string): string {
  const t = p.trim();
  if (/^[A-Za-z]:[\\/]?$/.test(t)) return `${t[0].toUpperCase()}:\\`;
  return t.replace(/[\\/]+$/, "");
}

// Display helper: folder paths in the input always end with `\`.
// Empty input stays empty; paths already ending in `\` or `/` are untouched.
function withTrailingSep(p: string): string {
  if (!p.trim()) return p;
  if (/[\\/]$/.test(p)) return p;
  return `${p}\\`;
}

// Human file size: 0 B, 1.2 KB, 34 MB…
function formatSize(b: number): string {
  if (!b || b <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = b;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${u === 0 || v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

type Completion = {
  path: string;
  name: string;
  is_dir: boolean;
};

// Bold the matched substring inside a hit name (substring matches only;
// fuzzy-only hits render plain).
function highlightName(name: string, query: string): React.ReactNode {
  const q = query.trim().toLowerCase();
  if (!q) return name;
  const i = name.toLowerCase().indexOf(q);
  if (i < 0) return name;
  return (
    <>
      {name.slice(0, i)}
      <b>{name.slice(i, i + q.length)}</b>
      {name.slice(i + q.length)}
    </>
  );
}
// Drag-and-drop payload: the moved item's full path. `text/plain` carries
// the same value as a fallback (some webviews drop custom mime types).
const DRAG_MIME = "text/eksplora-path";
// Depth of `full` relative to `base` (direct child = 1). Drives the
// per-level row indent.
function relDepth(full: string, base: string): number {
  const b = base.replace(/[\\/]+$/, "");
  const rest = full.startsWith(b) ? full.slice(b.length) : full;
  const parts = rest.split(/[\\/]+/).filter(Boolean);
  return Math.max(1, parts.length);
}

const FolderIcon = () => (
  <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
    <path
      d="M1.5 4.2c0-.9.7-1.7 1.7-1.7h2.9l1.3 1.6h5.4c.9 0 1.7.8 1.7 1.7v4.5c0 .9-.8 1.7-1.7 1.7H3.2c-1 0-1.7-.8-1.7-1.7V4.2z"
      fill="#7aa2ff"
    />
  </svg>
);

const FileIcon = () => (
  <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
    <path
      d="M3.6 1.5h4.9l3 3v7.9c0 .7-.5 1.1-1.1 1.1H3.6c-.6 0-1.1-.4-1.1-1.1V2.6c0-.6.5-1.1 1.1-1.1zm4.9 1v2.6h2.7l-2.7-2.6z"
      fill="#9aa3b2"
    />
  </svg>
);

export default function App() {
  const [root, setRoot] = useState("C:\\Users\\");
  const [query, setQuery] = useState("");
  const [limit, setLimit] = useState(500);
  const [fuzzy, setFuzzy] = useState(false);
  const [depth, setDepth] = useState(1); // committed (backend) value
  const [depthUI, setDepthUI] = useState(1); // slider position
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [scan, setScan] = useState<ScanInfo | null>(null);
  const [qms, setQms] = useState(0);
  const [sysinfo, setSysinfo] = useState("");
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState("");
  const [progress, setProgress] = useState<ScanProgress | null>(null);
  // Live per-folder counts streamed with scan-progress (~4Hz). A ref, not
  // state: ticks arrive faster than renders should copy — the map (tens of
  // thousands of keys worst case) is mutated in place and a tick counter
  // re-renders only when a value actually changed. Cleared on every new
  // scan and on scan-done, when the index becomes authoritative again.
  const liveRef = useRef<Record<string, number>>({});
  const [, setLiveTick] = useState(0);
  const [activePath, setActivePath] = useState("");
  const [copied, setCopied] = useState(false);
  const copyTimer = useRef<number | null>(null);
  const [comp, setComp] = useState<Completion[]>([]);
  const [compIdx, setCompIdx] = useState(0);
  const [compOpen, setCompOpen] = useState(false);
  const parentRef = useRef<HTMLDivElement>(null);
  const compListRef = useRef<HTMLDivElement>(null);
  const pathInputRef = useRef<HTMLInputElement>(null);
  const measureRef = useRef<CanvasRenderingContext2D | null>(null);
  // Monotonic id: only the latest search may write results.
  // Older responses (including backend-"cancelled" ones) are dropped.
  const seqRef = useRef(0);
  // Last path we actually indexed — avoids rescanning an unchanged path.
  const scannedRef = useRef("");
  // Path of the latest requested scan. Stale progress/done events
  // (from a scan superseded by typing a new path) are dropped by comparing.
  const targetRef = useRef("");
  const depthTimer = useRef<number | null>(null);
  // Back/forward history of successfully scanned paths.
  const [backStack, setBackStack] = useState<string[]>([]);
  const [fwdStack, setFwdStack] = useState<string[]>([]);
  // Mirrors activePath for use inside event callbacks.
  const activeRef = useRef("");
  // Which history navigation (if any) triggered the in-flight scan.
  const navRef = useRef<"back" | "fwd" | "up" | null>(null);
  // Live completion fetch debounce + staleness guard.
  const compTimer = useRef<number | null>(null);
  const compSeq = useRef(0);
  // Inline tree-expand: dir paths unfolded below their row. The path field
  // and history are untouched — expanding never navigates.
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // Fetched direct children per expanded dir (refreshed as the index grows).
  const [kids, setKids] = useState<Record<string, SearchHit[]>>({});
  // Mirror for use inside async callbacks (state would be stale there).
  const expandedRef = useRef<Set<string>>(new Set());
  // Dir whose children are playing the expand animation. Set on unfold,
  // cleared shortly after so later re-renders don't replay it.
  const [reveal, setReveal] = useState<string | null>(null);
  const revealTimer = useRef<number | null>(null);
  // Dir whose subtree is playing the collapse animation. Rows stay mounted
  // until the fold finishes, then the dir is actually removed from expanded.
  const [collapsing, setCollapsing] = useState<string | null>(null);
  const collapseTimer = useRef<number | null>(null);
  // True when `row` sits strictly inside `dir` (separator-boundary aware,
  // so `C:\AB\…` never matches dir `C:\A`).
  function isUnderReveal(row: string, dir: string): boolean {
    if (row === dir || !row.startsWith(dir)) return false;
    const c = row.charAt(dir.length);
    return c === "\\" || c === "/";
  }
  // Focused file row (single click). Dirs expand instead of selecting.
  const [selected, setSelected] = useState<string | null>(null);
  // Drop target highlight while dragging over a dir row or quick item.
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  // Finish flourish on the path preview after a scan completes.
  const [justDone, setJustDone] = useState(false);
  // Guards toggleDir against the second half of a double-click (which
  // would otherwise expand-then-collapse instantly).
  const lastToggleRef = useRef<{ path: string; t: number } | null>(null);
  // Right-click context menu target + position. Panel = open flyout
  // ('rename'/'duplicate' input or 'delete' confirm) next to its item.
  const [ctx, setCtx] = useState<{ hit: SearchHit; x: number; y: number } | null>(null);
  const [ctxPanel, setCtxPanel] = useState<"rename" | "duplicate" | null>(null);
  const [menuError, setMenuError] = useState("");
  const [renameVal, setRenameVal] = useState("");
  const [dupVal, setDupVal] = useState("");
  const renameInputRef = useRef<HTMLInputElement>(null);

  // Quick access: user-pinned directories shown in the left sidebar.
  // Persisted in localStorage; clicking a pin opens it in the file list.
  const [quickDirs, setQuickDirs] = useState<string[]>(() => {
    try {
      const raw = localStorage.getItem("eksplora.quickDirs");
      if (!raw) return [];
      const arr: unknown = JSON.parse(raw);
      if (!Array.isArray(arr)) return [];
      return arr.filter((p): p is string => typeof p === "string" && !!p.trim()).slice(0, 100);
    } catch {
      return [];
    }
  });

  useEffect(() => {
    try {
      localStorage.setItem("eksplora.quickDirs", JSON.stringify(quickDirs));
    } catch {
      // storage full / unavailable — pins just won't survive reload
    }
  }, [quickDirs]);

  // Short label for the sidebar: last segment only (`C:\Users\Bob` -> `Bob`,
  // `C:\` stays `C:\`).
  function quickName(p: string): string {
    const t = p.replace(/[\\/]+$/, "");
    if (/^[A-Za-z]:$/.test(t)) return `${t[0].toUpperCase()}:\\`;
    const i = Math.max(t.lastIndexOf("\\"), t.lastIndexOf("/"));
    const base = i >= 0 ? t.slice(i + 1) : t;
    return base || p;
  }

  function isQuickPinned(p: string): boolean {
    if (!p.trim()) return false;
    const n = normPath(p).toLowerCase();
    return quickDirs.some((q) => normPath(q).toLowerCase() === n);
  }

  function addQuickDir() {
    pinDir(activePath);
  }

  function pinDir(p: string) {
    if (!p || isQuickPinned(p)) return;
    setQuickDirs((q) => [...q, p]);
  }

  function openQuickDir(p: string) {
    setRoot(withTrailingSep(p));
    doScan(p, null);
  }

  function removeQuickDir(p: string) {
    const n = normPath(p).toLowerCase();
    setQuickDirs((q) => q.filter((x) => normPath(x).toLowerCase() !== n));
  }

  // How many pinned dirs share each display name. Names appearing more
  // than once get their full path shown underneath for disambiguation.
  const quickNameCounts = useMemo(() => {
    const m = new Map<string, number>();
    for (const p of quickDirs) {
      const n = quickName(p).toLowerCase();
      m.set(n, (m.get(n) ?? 0) + 1);
    }
    return m;
  }, [quickDirs]);

  // Default duplicate name: `notes-copy.txt`, `sub-copy`.
  function duplicateDefault(name: string, isDir: boolean): string {
    if (isDir) return `${name}-copy`;
    const i = name.lastIndexOf(".");
    if (i > 0) return `${name.slice(0, i)}-copy${name.slice(i)}`;
    return `${name}-copy`;
  }

  // Flat render list: base hits with fetched children spliced below each
  // expanded dir, recursively (nested expands nest deeper).
  const displayHits = useMemo(() => {
    if (expanded.size === 0) return hits;
    const out: SearchHit[] = [];
    const visit = (h: SearchHit) => {
      out.push(h);
      if (h.is_dir && expanded.has(h.path)) {
        for (const c of kids[h.path] ?? []) visit(c);
      }
    };
    hits.forEach(visit);
    return out;
  }, [hits, expanded, kids]);

  const rowVirtualizer = useVirtualizer({
    count: displayHits.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 30,
    overscan: 12,
  });

  const totalSize = useMemo(() => rowVirtualizer.getTotalSize(), [rowVirtualizer, displayHits]);
  const browsing = !query.trim();
  const ghost = ghostParts();
  // Order of freshly revealed rows within the unfolded dir, for the
  // staggered cascade (first child slides, then the next, …).
  const revealOrder = useMemo(() => {
    const m = new Map<string, number>();
    if (!browsing || collapsing != null || reveal == null) return m;
    let n = 0;
    for (const h of displayHits) {
      if (h.path !== reveal && isUnderReveal(h.path, reveal)) m.set(h.path, n++);
    }
    return m;
  }, [displayHits, reveal, browsing, collapsing]);
  // Reverse order of rows folding away, for the bottom-to-top collapse:
  // the last visible descendant goes first.
  const collapseOrder = useMemo(() => {
    const m = new Map<string, number>();
    if (!browsing || collapsing == null) return m;
    const rows: string[] = [];
    for (const h of displayHits) {
      if (h.path !== collapsing && isUnderReveal(h.path, collapsing)) rows.push(h.path);
    }
    rows.forEach((p, i) => m.set(p, rows.length - 1 - i));
    return m;
  }, [displayHits, collapsing, browsing]);
  // Flyout panels (rename/duplicate) open leftwards when the menu sits
  // close to the right viewport edge.
  const flyLeft = ctx != null && ctx.x > window.innerWidth - 480;

  // `nav` records what triggered the request: typed text (null) or a history
  // button. The scan-done handler uses it to decide stack updates, so a
  // back-navigation can never be mistaken for a fresh typed path.
  async function doScan(p: string, nav: "back" | "fwd" | "up" | null) {
    if (!p) return;
    // scan_dir returns immediately (work continues on a bg thread);
    // results arrive via the scan-done event.
    navRef.current = nav;
    targetRef.current = p;
    // New root, new tree: expanded rows belong to the old index.
    expandedRef.current = new Set();
    setExpanded(new Set());
    setKids({});
    setSelected(null);
    setJustDone(false);
    setScanning(true);
    setScanError("");
    setProgress(null);
    liveRef.current = {};
    setReveal(null);
    if (revealTimer.current) {
      window.clearTimeout(revealTimer.current);
      revealTimer.current = null;
    }
    setCollapsing(null);
    if (collapseTimer.current) {
      window.clearTimeout(collapseTimer.current);
      collapseTimer.current = null;
    }
    try {
      await invoke<boolean>("scan_dir", { path: p });
    } catch (e) {
      if (targetRef.current !== p) return;
      setScanning(false);
      setScanError(String(e));
    }
  }

  // Dynamic scan: 0.5s after the user stops typing a path, index it.
  // Clearing the field cancels the running scan. Typing a new path
  // cancels the old one — that is the stop mechanism, no button needed.
  useEffect(() => {
    const p = normPath(root);
    if (!p) {
      targetRef.current = "";
      invoke("scan_cancel", {}).catch(() => {});
      setScanning(false);
      setProgress(null);
      liveRef.current = {};
      return;
    }
    if (p === scannedRef.current || p === targetRef.current) return;
    const h = setTimeout(() => {
      doScan(p, null);
    }, 500);
    return () => clearTimeout(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [root]);

  // Keep the keyboard-selected suggestion visible while arrowing
  // through a scrollable popup.
  useEffect(() => {
    if (!compOpen) return;
    compListRef.current
      ?.querySelector('[data-active="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [compIdx, compOpen, comp]);

  // Layers slider commits 0.2s after release so dragging doesn't
  // re-run a full-index listing on every tick.
  function onDepth(v: number) {
    setDepthUI(v);
    if (depthTimer.current) window.clearTimeout(depthTimer.current);
    depthTimer.current = window.setTimeout(() => setDepth(v), 200);
  }

  // Tab completion on the path field. The popup opens live while typing
  // (no Tab needed); Tab fills the highlighted suggestion, arrows switch it,
  // Enter confirms, Escape closes. Confirmed paths gain a trailing "\".
  function pickCompletion(c: Completion) {
    const p = c.path.endsWith("\\") || c.path.endsWith("/") ? c.path : `${c.path}\\`;
    setRoot(p);
    setCompOpen(false);
  }

  async function fetchComp(input: string) {
    const mySeq = ++compSeq.current;
    try {
      const r = await invoke<Completion[]>("complete_path", { input, limit: 50 });
      if (compSeq.current !== mySeq) return; // stale, user kept typing
      if (r.length === 0) {
        setCompOpen(false);
        return;
      }
      setComp(r);
      setCompIdx(0);
      setCompOpen(true);
    } catch {
      // unreadable parent — no popup, typing continues
    }
  }

  function onPathChange(v: string) {
    setRoot(v);
    setCompOpen(false);
    if (compTimer.current) window.clearTimeout(compTimer.current);
    if (!v.trim()) return;
    compTimer.current = window.setTimeout(() => fetchComp(v), 120);
  }

  // Leaving the path field appends a trailing `\` when missing,
  // so the input always shows a folder path ending with a separator.
  function onPathBlur() {
    setCompOpen(false);
    setRoot((prev) => withTrailingSep(prev));
  }

  async function onPathKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Escape") {
      setCompOpen(false);
      return;
    }
    if (e.key === "Enter") {
      if (compOpen && comp.length > 0) {
        e.preventDefault();
        pickCompletion(comp[compIdx % comp.length]);
      }
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const dir = e.key === "ArrowDown" ? 1 : -1;
      if (!compOpen || comp.length === 0) {
        fetchComp(root);
        return;
      }
      setCompIdx((i) => (i + dir + comp.length) % comp.length);
      return;
    }
    if (e.key !== "Tab") return;
    e.preventDefault();
    // Empty field: Tab opens the drive/folder list instead of moving focus.
    if (!compOpen || comp.length === 0) {
      fetchComp(root); // open only; next Tab fills
      return;
    }
    pickCompletion(comp[compIdx % comp.length]);
  }

  // Left edge of the suggestion popup: right after the last separator,
  // so the list opens exactly where the user is typing. Monospace font
  // means offset = char count × one advance width. 11px = input text
  // origin (1px border + 10px padding).
  function popupLeft(): number {
    const i = Math.max(root.lastIndexOf("\\"), root.lastIndexOf("/"));
    const chars = i < 0 ? 0 : i + 1;
    let w = 7.8;
    let scroll = 0;
    const el = pathInputRef.current;
    if (el) {
      if (!measureRef.current) {
        measureRef.current = document.createElement("canvas").getContext("2d");
      }
      const ctx = measureRef.current;
      if (ctx) {
        const cs = getComputedStyle(el);
        ctx.font = `${cs.fontWeight} ${cs.fontSize} ${cs.fontFamily}`;
        w = ctx.measureText("M").width || w;
      }
      scroll = el.scrollLeft || 0;
    }
    return Math.max(0, 11 + chars * w - scroll);
  }

  // Ghost remainder of the highlighted suggestion, rendered behind the
  // input. Follows the underline as you arrow through the list; Tab fills
  // exactly what the ghost shows (trailing "\" included).
  function ghostParts(): [string, string] | null {
    if (!compOpen || comp.length === 0) return null;
    const best = comp[compIdx % comp.length].path;
    const base = root ?? "";
    if (!best.toLowerCase().startsWith(base.toLowerCase()) || best.length <= base.length) {
      return null;
    }
    let rest = best.slice(base.length);
    if (!rest.endsWith("\\") && !rest.endsWith("/")) rest += "\\";
    return [base, rest];
  }

  function parentOf(p: string): string | null {
    const t = p.replace(/[\\/]+$/, "");
    if (/^[A-Za-z]:$/.test(t)) return null; // drive root: no parent
    const i = Math.max(t.lastIndexOf("\\"), t.lastIndexOf("/"));
    if (i <= 0) return null;
    const par = t.slice(0, i);
    return par.length === 2 && par[1] === ":" ? `${par}\\` : par;
  }

  // Clicking the preview path copies it to the clipboard.
  async function copyActivePath() {
    if (!activePath) return;
    try {
      await navigator.clipboard.writeText(activePath);
    } catch {
      const ta = document.createElement("textarea");
      ta.value = activePath;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    setCopied(true);
    if (copyTimer.current) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1200);
  }

  // History buttons scan immediately (no 0.5s wait); the auto-scan effect
  // sees targetRef already set and stays out of the way.
  function goBack() {    const prev = backStack[backStack.length - 1];
    if (!prev) return;
    const cur = activeRef.current;
    if (cur) setFwdStack((f) => [cur, ...f]);
    setBackStack((b) => b.slice(0, -1));
    setRoot(withTrailingSep(prev));
    doScan(prev, "back");
  }

  function goFwd() {
    const next = fwdStack[0];
    if (!next) return;
    const cur = activeRef.current;
    if (cur) setBackStack((b) => [...b, cur]);
    setFwdStack((f) => f.slice(1));
    setRoot(withTrailingSep(next));
    doScan(next, "fwd");
  }

  function goUp() {
    const par = activePath ? parentOf(activePath) : null;
    if (!par) return;
    setRoot(withTrailingSep(par));
    doScan(par, "up");
  }

  // Drag and drop: move the dragged item into `targetDir`, keeping its
  // name. Expands the target when it is a visible dir row so the moved
  // item shows up right away.
  async function moveIntoDir(srcPath: string, targetDir: string) {
    if (!srcPath || !targetDir) return;
    try {
      // NOTE: Rust `target_dir` arrives as camelCase `targetDir`.
      const newPath = await invoke<string>("move_entry", { path: srcPath, targetDir });
      if (targetDir !== activeRef.current && !expandedRef.current.has(targetDir)) {
        const next = new Set(expandedRef.current);
        next.add(targetDir);
        expandedRef.current = next;
        setExpanded(new Set(next));
      }
      setSelected(newPath);
      refresh();
    } catch (e) {
      setScanError(String(e));
    }
  }

  // Read the dragged path back (custom mime first, plain-text fallback).
  function dragSrc(e: React.DragEvent): string {
    return e.dataTransfer.getData(DRAG_MIME) || e.dataTransfer.getData("text/plain");
  }

  // Row clicks: single click toggles folders inline and focuses files;
  // double-click opens folders as the new root and launches files. The
  // path field, history and scan are untouched by expanding.
  function clickRow(h: SearchHit) {
    setSelected(h.path);
    if (h.is_dir) {
      toggleDir(h.path);
    }
  }

  // Double-click a directory opens it as the new root (history preserved),
  // same as Open in the context menu.
  function openDir(path: string) {
    setRoot(withTrailingSep(path));
    doScan(path, null);
  }

  async function openFile(path: string) {
    setSelected(path);
    try {
      await invoke("open_path", { path });
    } catch (e) {
      setScanError(String(e));
    }
  }

  // Bumped after copy/rename/delete: the backend already patched the live
  // index, so this just re-runs the search + expanded layers. No rescan,
  // no progress UI, tree/selection otherwise preserved.
  const [refreshSeq, setRefreshSeq] = useState(0);
  function refresh() {
    expandedRef.current.forEach((p) => {
      fetchKids(p);
    });
    setRefreshSeq((n) => n + 1);
  }

  function openCtx(e: React.MouseEvent, h: SearchHit) {
    e.preventDefault();
    setSelected(h.path);
    setCtxPanel(null);
    setMenuError("");
    // Prefill the rename field with the full current name (extension
    // included); the stem gets auto-selected on panel open below.
    setRenameVal(h.name);
    setDupVal(duplicateDefault(h.name, h.is_dir));
    // Clamp so the ~230px menu never leaves the viewport.
    setCtx({
      hit: h,
      x: Math.max(4, Math.min(e.clientX, window.innerWidth - 244)),
      y: Math.max(4, Math.min(e.clientY, window.innerHeight - 210)),
    });
  }

  function closeCtx() {
    setCtx(null);
    setCtxPanel(null);
    setMenuError("");
  }

  // "Open" from the menu follows the path-field semantics: a directory
  // becomes the new root (history preserved), a file launches.
  function ctxOpen() {
    const h = ctx?.hit;
    if (!h) return;
    if (h.is_dir) {
      setRoot(withTrailingSep(h.path));
      closeCtx();
      doScan(h.path, null);
      return;
    }
    closeCtx();
    openFile(h.path);
  }

  // Copy stages the item for paste (in-app + OS clipboard) — nothing is
  // created, so no refresh is needed.
  async function ctxCopy() {
    const h = ctx?.hit;
    if (!h) return;
    closeCtx();
    try {
      await invoke<string>("copy_entry", { path: h.path });
    } catch (e) {
      setScanError(String(e));
    }
  }

  // Paste clipboard into the selected folder, or the current path when no
  // folder is selected. Expands the target when it is a visible dir row.
  async function pasteHere() {
    const sel = displayHits.find((h) => h.path === selected);
    const target = sel && sel.is_dir ? sel.path : activeRef.current;
    if (!target) return;
    try {
      // NOTE: Rust `target_dir` arrives as camelCase `targetDir`.
      const created = await invoke<string[]>("paste_entry", { targetDir: target });
      if (created.length === 0) return;
      if (target !== activeRef.current && !expandedRef.current.has(target)) {
        const next = new Set(expandedRef.current);
        next.add(target);
        expandedRef.current = next;
        setExpanded(new Set(next));
      }
      setSelected(created[0]);
      refresh();
    } catch (e) {
      setScanError(String(e));
    }
  }

  // Ctrl+Z restores the most recently deleted item from the Recycle Bin
  // to its original place. Skipped while typing (inputs keep native undo).
  async function undoDelete() {
    try {
      const restored = await invoke<string>("undo_entry", {});
      setSelected(restored);
      refresh();
    } catch (e) {
      setScanError(String(e));
    }
  }

  // Ctrl+V pastes, Ctrl+Z undoes a delete — both skipped while typing
  // (inputs keep their native paste/undo).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      const t = document.activeElement;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
      if (e.key === "v" || e.key === "V") {
        e.preventDefault();
        pasteHere();
      } else if (e.key === "z" || e.key === "Z") {
        e.preventDefault();
        undoDelete();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, displayHits]);

  async function ctxDuplicateCommit() {
    const h = ctx?.hit;
    const name = dupVal.trim();
    if (!h || !name) return;
    try {
      // Rust `new_name` arrives as camelCase (see rename below).
      const newPath = await invoke<string>("duplicate_entry", { path: h.path, newName: name });
      setSelected(newPath);
      closeCtx();
      refresh();
    } catch (e) {
      setMenuError(String(e));
    }
  }

  async function ctxRenameCommit() {
    const h = ctx?.hit;
    const name = renameVal.trim();
    if (!h || !name) return;
    try {
      // NOTE: Tauri exposes Rust `new_name` to JS as camelCase `newName`.
      const newPath = await invoke<string>("rename_entry", { path: h.path, newName: name });
      setSelected(newPath);
      closeCtx();
      refresh();
    } catch (e) {
      setMenuError(String(e));
    }
  }

  async function ctxDelete() {
    const h = ctx?.hit;
    if (!h) return;
    try {
      await invoke("delete_entry", { path: h.path });
      // Drop any expanded/cached state for the deleted subtree.
      if (expandedRef.current.has(h.path)) {
        const next = new Set(expandedRef.current);
        next.delete(h.path);
        expandedRef.current = next;
        setExpanded(new Set(next));
      }
      setKids((prev) => {
        if (!(h.path in prev)) return prev;
        const next = { ...prev };
        delete next[h.path];
        return next;
      });
      setSelected(null);
      closeCtx();
      refresh();
    } catch (e) {
      setMenuError(String(e));
    }
  }

  // Escape closes the context menu.
  useEffect(() => {
    if (!ctx) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeCtx();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [ctx]);

  // Explorer-style rename: when the panel opens, select the stem so typing
  // replaces the name but keeps the extension (dirs select all).
  useEffect(() => {
    if (ctxPanel !== "rename" || !ctx) return;
    const el = renameInputRef.current;
    if (!el) return;
    const name = ctx.hit.name;
    const i = ctx.hit.is_dir ? -1 : name.lastIndexOf(".");
    el.setSelectionRange(0, i > 0 ? i : name.length);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ctxPanel]);

  // Fetch one layer of direct children for an expanded dir. Stale responses
  // (new scan started, or collapsed meanwhile) are dropped.
  async function fetchKids(path: string) {
    const target = targetRef.current;
    try {
      const r = await invoke<SearchHit[]>("list_children", { path, limit: 2000 });
      if (targetRef.current !== target) return;
      if (!expandedRef.current.has(path)) return;
      setKids((prev) => ({ ...prev, [path]: r }));
    } catch {
      // Unreadable / vanished dir — row stays expanded but childless.
    }
  }

  function toggleDir(path: string) {
    // Second half of a double-click on the same dir: ignore, so a
    // double-click keeps the result of the first click instead of
    // toggling twice (expand → collapse flash).
    const now = performance.now();
    const last = lastToggleRef.current;
    if (last && last.path === path && now - last.t < 350) return;
    lastToggleRef.current = { path, t: now };
    if (expandedRef.current.has(path)) {
      // Collapse: bottom-to-top fold first, unmount after it finishes.
      // Empty dirs, and anything outside browse mode (the fold maps are
      // browse-only — otherwise this would be a dead pause), unmount now.
      if (collapseTimer.current) window.clearTimeout(collapseTimer.current);
      let n = 0;
      for (const h of displayHits) {
        if (h.path !== path && isUnderReveal(h.path, path)) n++;
      }
      if (n === 0 || !browsing) {
        const next = new Set(expandedRef.current);
        next.delete(path);
        expandedRef.current = next;
        setExpanded(new Set(next));
        return;
      }
      setCollapsing(path);
      const total = 130 + Math.min((n - 1) * 15, 300) + 80;
      collapseTimer.current = window.setTimeout(() => {
        const next = new Set(expandedRef.current);
        next.delete(path);
        expandedRef.current = next;
        setExpanded(new Set(next));
        // Only disarm our own fold — a newer collapse may be running.
        setCollapsing((c) => (c === path ? null : c));
        collapseTimer.current = null;
      }, total);
      return;
    }
    // Expanding cancels any in-flight collapse of this dir.
    if (collapseTimer.current) {
      window.clearTimeout(collapseTimer.current);
      collapseTimer.current = null;
    }
    setCollapsing(null);
    const next = new Set(expandedRef.current);
    next.add(path);
    expandedRef.current = next;
    setExpanded(new Set(next));
    // Play the staggered unfold on this dir's children, then disarm so
    // later re-renders (scans, searches) don't replay it — but never steal
    // a newer unfold's cascade.
    setReveal(path);
    if (revealTimer.current) window.clearTimeout(revealTimer.current);
    revealTimer.current = window.setTimeout(() => {
      setReveal((r) => (r === path ? null : r));
      revealTimer.current = null;
    }, 700);
    if (!kids[path]) fetchKids(path);
  }

  // Live search: debounced backend query, virtualized render.
  // Empty query = browse mode (tree up to `depth` levels deep).
  // Stale responses are discarded via seq guard; backend also cancels
  // superseded searches and reports them as "cancelled".
  useEffect(() => {
    if (!scan) return;
    const run = async () => {
      const mySeq = ++seqRef.current;
      const t = performance.now();
      try {
        const r = await invoke<SearchHit[]>("search_index", {
          query,
          limit,
          fuzzy,
          depth,
        });
        if (seqRef.current !== mySeq) return; // stale, a newer query is in flight
        setHits(r);
        setQms(performance.now() - t);
      } catch (e) {
        if (String(e).includes("cancelled")) return; // superseded, ignore
        if (seqRef.current !== mySeq) return;
        setSysinfo(String(e));
      }
    };
    if (browsing) {
      run(); // clearing the box shows the listing immediately
      return;
    }
    const h = setTimeout(run, 80);
    return () => clearTimeout(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query, limit, fuzzy, depth, scan, refreshSeq]);

  // Keep expanded layers live: every index refresh (partial or done)
  // re-fetches children of open dirs so the tree fills in as the
  // progressive scan streams deeper layers.
  useEffect(() => {
    if (!scan || expandedRef.current.size === 0) return;
    expandedRef.current.forEach((p) => {
      fetchKids(p);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scan, refreshSeq]);

  useEffect(() => {
    let offProgress = () => {};
    let offPartial = () => {};
    let offDone = () => {};
    let offFs = () => {};
    // First time a path becomes visible (first partial or final done),
    // publish it to the UI + history. Later partials for the same path
    // only refresh counts. Consumes navRef exactly once per path.
    const applyVisiblePath = (path: string) => {
      const prev = activeRef.current;
      if (path === prev) return;
      const nav = navRef.current;
      navRef.current = null;
      if (nav !== "back" && nav !== "fwd") {
        if (prev) setBackStack((b) => [...b, prev]);
        setFwdStack([]);
      }
      activeRef.current = path;
      setActivePath(path);
    };
    (async () => {
      try {
        offProgress = await listen<ScanProgress>("scan-progress", (e) => {
          if (e.payload.path !== targetRef.current) return; // stale scan
          setProgress(e.payload);
          // Fold live per-folder counts in (cumulative values, keyed by
          // lowercase path). Mutated in place — no per-tick copy — and the
          // view re-renders only when a value actually changed.
          const c = e.payload.counts;
          if (c && c.length > 0) {
            const m = liveRef.current;
            let touched = false;
            for (const [p, n] of c) {
              const k = p.toLowerCase();
              if (m[k] !== n) {
                m[k] = n;
                touched = true;
              }
            }
            if (touched) setLiveTick((t) => t + 1);
          }
        });
        // Progressive loading: shallow layer arrives first (~100ms),
        // deeper layers stream in. Each partial refreshes `scan`,
        // which re-runs the search effect below — the list grows live
        // instead of waiting for the whole walk.
        offPartial = await listen<ScanPartial>("scan-partial", (e) => {
          const d = e.payload;
          if (d.path !== targetRef.current) return; // superseded by a newer path
          setScan({ len: d.len, files: d.files, dirs: d.dirs, duration_ms: d.duration_ms });
          applyVisiblePath(d.path);
        });
        offDone = await listen<ScanDone>("scan-done", (e) => {
          const d = e.payload;
          if (d.path !== targetRef.current) return; // superseded by a newer path
          if (d.cancelled) return; // a fresher scan is in flight, keep waiting
          setScan({ len: d.len, files: d.files, dirs: d.dirs, duration_ms: d.duration_ms });
          scannedRef.current = d.path;
          // History normally already applied by the first partial;
          // this covers the (rare) case of done arriving with no partial.
          applyVisiblePath(d.path);
          setProgress(null);
          liveRef.current = {}; // index is authoritative again
          setScanning(false);
          // Finish flourish on the path preview (fills, then fades out).
          setJustDone(true);
        });
        // External change by another process (file added/deleted outside
        // the app): the backend already patched the live index, so just
        // re-run the search + expanded layers. No rescan, tree preserved.
        // `setRefreshSeq` is stable, so this is safe from a mount effect.
        offFs = await listen("fs-changed", () => {
          setRefreshSeq((n) => n + 1);
        });
        const s = await invoke<string>("sysinfo", {});
        setSysinfo(s);
        const p = await invoke<string>("desktop_path");
        if (p) setRoot(withTrailingSep(p)); // auto-scan effect picks it up after 0.5s
        setQuery("");
      } catch (e) {
        setSysinfo(String(e));
      }
    })();
    return () => {
      offProgress();
      offPartial();
      offDone();
      offFs();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="app">
      <div className="toolbar">
      
        {scanning && <span className="scanning">Scanning</span>}

        <div className="path-wrap">
          {ghost && (
            <div className="ghost" aria-hidden="true">
              <span className="ghost-typed">{ghost[0]}</span>
              <span className="ghost-rest">{ghost[1]}</span>
            </div>
          )}
          <input
            className="path-input"
            ref={pathInputRef}
            type="text"
            value={root}
            onChange={(e) => onPathChange(e.target.value)}
            onKeyDown={onPathKeyDown}
            onBlur={onPathBlur}
            placeholder="Type a folder path — suggestions as you type"
            spellCheck={false}
          />
          {compOpen && comp.length > 0 && (
            <div className="comp-list" ref={compListRef} style={{ left: popupLeft() }}>
              {comp.map((c, i) => (
                <div
                  key={c.path}
                  className={i === compIdx % comp.length ? "comp-item selected" : "comp-item"}
                  data-active={i === compIdx % comp.length}
                  title={c.path}
                  onMouseEnter={() => setCompIdx(i)}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pickCompletion(c);
                  }}
                >
                  <span className="comp-name">{c.name}</span>
                </div>
              ))}
            </div>
          )}
        </div>
        
        <input
          className="search-input"
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search..."
        />
      </div>
      <div className="meta">

        {/* this optios are saved for later */}
        {/* <label title="Layers shown in browse mode (1 = this folder only)">
          layers
          <input
            type="range"
            min={1}
            max={10}
            step={1}
            value={depthUI}
            onChange={(e) => onDepth(Number(e.target.value))}
          />
          <span className="depth-val">{depthUI}</span>
        </label>
        <label>
          <input type="checkbox" checked={fuzzy} onChange={(e) => setFuzzy(e.target.checked)} /> fuzzy
        </label>
        <input
          type="number"
          value={limit}
          min={10}
          max={5000}
          step={50}
          onChange={(e) => setLimit(Number(e.target.value))}
          style={{ width: 80 }}
        /> */}
        <span>index: {scan ? `${scan.len} entries (${scan.files}f/${scan.dirs}d, scan ${scan.duration_ms}ms)` : "not scanned"}</span>
        <span>
          {browsing
            ? `browsing: ${displayHits.length} items · layers ≤ ${depth}`
            : `query: ${hits.filter((h) => h.matched).length} matches · ${hits.length} rows in ${qms.toFixed(2)}ms`}
        </span>
        {scanError && <span className="error">{scanError}</span>}
      </div>
      <div className="nav-row">
        <button
          className="nav-btn"
          disabled={backStack.length === 0}
          onClick={goBack}
          title="Back to previous folder"
        >
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path d="M9 2.5 4.5 7 9 11.5" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
        <button
          className="nav-btn"
          disabled={fwdStack.length === 0}
          onClick={goFwd}
          title="Forward to next folder"
        >
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path d="M5 2.5 9.5 7 5 11.5" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
        <button
          className="nav-btn"
          disabled={!activePath || !parentOf(activePath)}
          onClick={goUp}
          title="Up one folder"
        >
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path d="M2.5 9 7 4.5 11.5 9" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </div>
      <div
        className={`current-path${justDone ? " complete" : ""}`}
        title={activePath ? `${activePath} — click to copy` : "Nothing indexed yet"}
        onClick={copyActivePath}
        onAnimationEnd={() => setJustDone(false)}
      >
        <button
          className={`pin-btn${isQuickPinned(activePath) ? " added" : ""}`}
          disabled={!activePath || isQuickPinned(activePath)}
          title={isQuickPinned(activePath) ? "Already in Quick access" : "Add this folder to Quick access"}
          onClick={(e) => {
            e.stopPropagation();
            addQuickDir();
          }}
        >
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path
              d="M7 1.2l1.7 3.6 3.9.5-2.9 2.7.7 3.9L7 10l-3.4 1.9.7-3.9L1.4 5.3l3.9-.5L7 1.2z"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinejoin="round"
            />
          </svg>
        </button>
        <FolderIcon />
        <span className="current-path-text">{activePath + "\\" || "—"}</span>
        {copied && <span className="copied">copied</span>}
        {scanning && progress && (
          <span className="prog-counts">
            {progress.files.toLocaleString()} files · {progress.dirs.toLocaleString()} dirs
          </span>
        )}
        {scanning && !progress && <span className="prog-counts">starting…</span>}
      </div>
      <div className="main-row">
        <aside className="quick">
          <div className="quick-title">Quick access</div>
          <div
            className="quick-list"
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
            }}
          >
            {quickDirs.length === 0 ? (
              <div className="quick-empty">Pin folders with the star above.</div>
            ) : (
              quickDirs.map((p, i) => (
                <div
                  key={normPath(p).toLowerCase()}
                  className={`quick-item${normPath(p).toLowerCase() === normPath(activePath).toLowerCase() ? " selected" : ""}${dropTarget === p ? " drop-target" : ""}`}
                  title={p}
                  style={{ animationDelay: `${Math.min(i * 25, 200)}ms` }}
                  onClick={() => openQuickDir(p)}
                  onDragEnter={(e) => {
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    setDropTarget(p);
                  }}
                  onDragOver={(e) => {
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    setDropTarget(p);
                  }}
                  onDragLeave={(e) => {
                    if (e.currentTarget.contains(e.relatedTarget as Node | null)) return;
                    setDropTarget((t) => (t === p ? null : t));
                  }}
                  onDrop={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    setDropTarget(null);
                    const src = dragSrc(e);
                    if (src) moveIntoDir(src, p);
                  }}
                >
                  <FolderIcon />
                  <span className="quick-text">
                    <span className="quick-name">{quickName(p)}</span>
                    {(quickNameCounts.get(quickName(p).toLowerCase()) ?? 0) > 1 && (
                      <span className="quick-path">{p}</span>
                    )}
                  </span>
                  <button
                    className="quick-remove"
                    title={`Remove ${quickName(p)} from Quick access`}
                    onClick={(e) => {
                      e.stopPropagation();
                      removeQuickDir(p);
                    }}
                  >
                    ×
                  </button>
                </div>
              ))
            )}
          </div>
        </aside>
        <div
          className="list"
          ref={parentRef}
          onDragOver={(e) => {
            // Allows drops on the list background (= the current folder).
            // Dir rows handle their own drops and stop propagation.
            e.preventDefault();
            e.dataTransfer.dropEffect = "move";
          }}
          onDrop={(e) => {
            e.preventDefault();
            const src = dragSrc(e);
            if (src && activeRef.current) moveIntoDir(src, activeRef.current);
          }}
        >
        {displayHits.length === 0 && !scanning && scan ? (
          <div className="empty">{browsing ? "Directory is empty." : `No results for "${query}".`}</div>
        ) : (
          <div style={{ height: totalSize, position: "relative" }}>
            {rowVirtualizer.getVirtualItems().map((v) => {
              const h = displayHits[v.index];
              if (!h) return null;
              // Indent against the indexed path, not the text being typed —
              // otherwise rows jump right on the first keystroke.
              const d = relDepth(h.path, activePath || root);
              // Dirs show item count, files show size, in both modes.
              // While scanning, overlay the live streamed count (cumulative
              // direct children walked so far); Math.max keeps the display
              // monotonic even if a progress tick arrives out of order.
              const live = scanning && h.is_dir ? liveRef.current[h.path.toLowerCase()] : undefined;
              const lastCol = h.is_dir ? Math.max(h.child_count, live ?? 0) : formatSize(h.size);
              const tip = !browsing && h.matched ? `${h.score} · ${h.path}` : h.path;
              // Expand/collapse choreography: freshly unfolded children
              // cascade top-down (25ms apart), folding rows go bottom-up
              // (15ms apart). Order maps stay empty outside their moment,
              // so scans and searches never replay anything.
              const rIdx = revealOrder.get(h.path);
              const cIdx = collapseOrder.get(h.path);
              const revealed = rIdx !== undefined;
              const folding = cIdx !== undefined;
              return (
                <div
                  key={v.key}
                  className={`row${selected === h.path ? " selected" : ""}${dropTarget === h.path ? " drop-target" : ""}${revealed ? " row-expand" : ""}${folding ? " row-collapse" : ""}`}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    boxSizing: "border-box",
                    height: v.size,
                    transform: `translateY(${v.start}px)`,
                    paddingLeft: 10 + (d - 1) * 24,
                    paddingRight: 10,
                    cursor: "pointer",
                    ...(revealed ? { animationDelay: `${Math.min(rIdx * 25, 350)}ms` } : null),
                    ...(folding ? { animationDelay: `${Math.min(cIdx * 15, 300)}ms` } : null),
                  }}
                  title={`${tip} — ${h.is_dir ? "click to expand/collapse · double-click to open" : "click to select · double-click to open"}`}
                  onClick={() => clickRow(h)}
                  onDoubleClick={() => {
                    if (h.is_dir) openDir(h.path);
                    else openFile(h.path);
                  }}
                  onContextMenu={(e) => openCtx(e, h)}
                  draggable
                  onDragStart={(e) => {
                    e.dataTransfer.setData(DRAG_MIME, h.path);
                    e.dataTransfer.setData("text/plain", h.path);
                    e.dataTransfer.effectAllowed = "move";
                    setSelected(h.path);
                  }}
                  onDragEnd={() => setDropTarget(null)}
                  // Both dragenter and dragover cancel the event: some
                  // engines only authorize the drop (move cursor instead of
                  // the red cross) when dragenter is canceled too. Files
                  // allow the cursor as well — their drop bubbles to the
                  // list background (= the current folder); only dirs
                  // highlight and take the drop themselves.
                  onDragEnter={(e) => {
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    if (h.is_dir) setDropTarget(h.path);
                  }}
                  onDragOver={(e) => {
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    if (h.is_dir) setDropTarget(h.path);
                  }}
                  onDragLeave={(e) => {
                    // Ignore moves between the row's own children.
                    if (e.currentTarget.contains(e.relatedTarget as Node | null)) return;
                    if (h.is_dir) setDropTarget((t) => (t === h.path ? null : t));
                  }}
                  onDrop={
                    h.is_dir
                      ? (e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          setDropTarget(null);
                          const src = dragSrc(e);
                          if (src) moveIntoDir(src, h.path);
                        }
                      : undefined
                  }
                >
                  {h.is_dir ? <FolderIcon /> : <FileIcon />}
                  <span className="path">
                    {!browsing && h.matched ? highlightName(h.name, query) : h.name}
                    <span className={`score${scanning && h.is_dir ? " counting" : ""}`}>
                      {h.is_dir ? " :" + lastCol + "" : ""}
                    </span>
                  </span>
                  <span className="score">{h.is_dir ? "" : lastCol}</span>
                </div>
              );
            })}
          </div>
        )}
        </div>
      </div>
      <div className="sysinfo">{sysinfo}</div>
      {ctx && (
        <>
          <div
            className="ctx-backdrop"
            onClick={closeCtx}
            onContextMenu={(e) => {
              e.preventDefault();
              closeCtx();
            }}
          />
          <div className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
            <div className="ctx-item" onClick={ctxOpen}>
              <span className="ctx-label">Open</span>
            </div>
            <div className="ctx-item" onClick={ctxCopy}>
              <span className="ctx-label">Copy</span>
            </div>
            {ctx.hit.is_dir && !isQuickPinned(ctx.hit.path) && (
              <div
                className="ctx-item"
                onClick={() => {
                  pinDir(ctx.hit.path);
                  closeCtx();
                }}
              >
                <span className="ctx-label">Pin to Quick access</span>
              </div>
            )}
            <div className="ctx-item-wrap">
              <div
                className={`ctx-item${ctxPanel === "duplicate" ? " active" : ""}`}
                onClick={() => {
                  setCtxPanel("duplicate");
                  if (ctx) setDupVal(duplicateDefault(ctx.hit.name, ctx.hit.is_dir));
                  setMenuError("");
                }}
              >
                <span className="ctx-label">Duplicate</span>
              </div>
              {ctxPanel === "duplicate" && (
                <div className={`ctx-sub col${flyLeft ? " left" : ""}`}>
                  <input
                    className="ctx-input"
                    autoFocus
                    value={dupVal}
                    placeholder={ctx.hit.name}
                    spellCheck={false}
                    onChange={(e) => setDupVal(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") ctxDuplicateCommit();
                      else if (e.key === "Escape") {
                        e.stopPropagation();
                        setCtxPanel(null);
                      }
                    }}
                  />
                  {menuError && (
                    <div className="ctx-field-error" key={menuError}>
                      {menuError}
                    </div>
                  )}
                </div>
              )}
            </div>
            <div className="ctx-item-wrap">
              <div
                className={`ctx-item${ctxPanel === "rename" ? " active" : ""}`}
                onClick={() => {
                  setCtxPanel("rename");
                  // (Re)prefill the full current name — extension included.
                  if (ctx) setRenameVal(ctx.hit.name);
                  setMenuError("");
                }}
              >
                <span className="ctx-label">Rename</span>
              </div>
              {ctxPanel === "rename" && (
                <div className={`ctx-sub col${flyLeft ? " left" : ""}`}>
                  <input
                    className="ctx-input"
                    autoFocus
                    ref={renameInputRef}
                    value={renameVal}
                    placeholder={ctx.hit.name}
                    spellCheck={false}
                    onChange={(e) => setRenameVal(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") ctxRenameCommit();
                      else if (e.key === "Escape") {
                        e.stopPropagation();
                        setCtxPanel(null);
                      }
                    }}
                  />
                  {menuError && (
                    <div className="ctx-field-error" key={menuError}>
                      {menuError}
                    </div>
                  )}
                </div>
              )}
            </div>
            <div className="ctx-item danger" onClick={ctxDelete}>
              <span className="ctx-label">Delete</span>
            </div>
            {!ctxPanel && menuError && <div className="ctx-error">{menuError}</div>}
          </div>
        </>
      )}
    </div>
  );
}
