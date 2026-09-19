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
// Depth of `full` relative to `base` (direct child = 1).
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

  const rowVirtualizer = useVirtualizer({
    count: hits.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 30,
    overscan: 12,
  });

  const totalSize = useMemo(() => rowVirtualizer.getTotalSize(), [rowVirtualizer, hits]);
  const browsing = !query.trim();
  const ghost = ghostParts();

  // `nav` records what triggered the request: typed text (null) or a history
  // button. The scan-done handler uses it to decide stack updates, so a
  // back-navigation can never be mistaken for a fresh typed path.
  async function doScan(p: string, nav: "back" | "fwd" | "up" | null) {
    if (!p) return;
    // scan_dir returns immediately (work continues on a bg thread);
    // results arrive via the scan-done event.
    navRef.current = nav;
    targetRef.current = p;
    setScanning(true);
    setScanError("");
    setProgress(null);
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
  }, [query, limit, fuzzy, depth, scan]);

  useEffect(() => {
    let offProgress = () => {};
    let offPartial = () => {};
    let offDone = () => {};
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
          setScanning(false);
        });
        const s = await invoke<string>("sysinfo", {});
        setSysinfo(s);
        const p = await invoke<string>("desktop_path");
        if (p) setRoot(p); // auto-scan effect picks it up after 0.5s
        setQuery("");
      } catch (e) {
        setSysinfo(String(e));
      }
    })();
    return () => {
      offProgress();
      offPartial();
      offDone();
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
            onBlur={() => setCompOpen(false)}
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
            ? `browsing: ${hits.length} items · layers ≤ ${depth}`
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
        className="current-path"
        title={activePath ? `${activePath} — click to copy` : "Nothing indexed yet"}
        onClick={copyActivePath}
      >
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
      {scanning && (
        <div className="progress-track">
          <div className="progress-fill" />
        </div>
      )}
      <div className="list" ref={parentRef}>
        {hits.length === 0 && !scanning && scan ? (
          <div className="empty">{browsing ? "Directory is empty." : `No results for "${query}".`}</div>
        ) : (
          <div style={{ height: totalSize, position: "relative" }}>
            {rowVirtualizer.getVirtualItems().map((v) => {
              const h = hits[v.index];
              if (!h) return null;
              // Indent against the indexed path, not the text being typed —
              // otherwise rows jump right on the first keystroke.
              const d = relDepth(h.path, activePath || root);
              // Dirs show item count, files show size, in both modes.
              const lastCol = h.is_dir ? h.child_count : formatSize(h.size);
              const tip = !browsing && h.matched ? `${h.score} · ${h.path}` : h.path;
              return (
                <div
                  key={v.key}
                  className="row"
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    boxSizing: "border-box",
                    height: v.size,
                    transform: `translateY(${v.start}px)`,
                    paddingLeft: 10 + (d - 1) * 18,
                    paddingRight: 10,
                  }}
                  title={tip}
                >
                  {h.is_dir ? <FolderIcon /> : <FileIcon />}
                  <span className="path">
                    {!browsing && h.matched ? highlightName(h.name, query) : h.name}
                    <span className="score">{h.is_dir ? " :"+lastCol+"" : ""}</span>
                  </span>
                  <span className="score">{h.is_dir ? "" : lastCol}</span>
                </div>
              );
            })}
          </div>
        )}
      </div>
      <div className="sysinfo">{sysinfo}</div>
    </div>
  );
}
