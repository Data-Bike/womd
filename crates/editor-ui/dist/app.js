// WoMD editor — block-based WYSIWYG with click-to-edit source.
// Rendered Markdown by default; click a block to edit its raw source.

// ── Tauri API ───────────────────────────────────────────────────────────────
function tauriInvoke(cmd, args) {
  const tauri = window.__TAURI__;
  if (!tauri) throw new Error("Tauri runtime not available");
  const invoke = (tauri.core && tauri.core.invoke) || tauri.invoke;
  if (!invoke) throw new Error("Tauri invoke not found");
  return invoke(cmd, args);
}

// ── State ───────────────────────────────────────────────────────────────────
let currentText = "";
let isDirty = false;
let syntaxBlocks = [];
let debounceTimer = null;
let autosaveTimer = null;
let autosaveInnerTimer = null;
let activeTabId = 0;
let openTabs = [];
let gitPanelVisible = false;
let gitData = { status: null, branches: [], diff: [], log: [] };
let editingBlockIndex = -1;
let suppressRender = false;
let suppressBlur = false;  // prevent exitEditMode when clicking toolbar buttons
let isTearOffWindow = false; // true for torn-off windows — show only their own tab
let viewMode = "rendered"; // "rendered" | "source"
let fileTreeRoot = null; // current root directory path for the file tree
let fileTreeExpanded = new Set(); // expanded folder paths
let fileTreeClipboard = null; // { path, isDir, operation: "copy" | "cut" }

// ── Virtualization state ────────────────────────────────────────────────────
// For large documents, only visible blocks are rendered with full data.
// We render a large window (renderWindow) so scrolling doesn't trigger
// re-renders on every frame. Re-render only when scroll moves outside
// the rendered window.
const VIRTUALIZATION_THRESHOLD = 200; // blocks above this use virtualized rendering
const RENDER_WINDOW = 100; // blocks to render at once (visible + buffer)
const AVG_BLOCK_HEIGHT = 60; // default estimate for unknown block heights
let blockMeta = [];       // lightweight metadata for all blocks
let blockCache = new Map(); // index -> full SyntaxBlock (source + AST)
let blockHeights = [];    // cached real heights per block index (px)
let virtualizedMode = false;
let visibleRange = { start: 0, end: 50 }; // currently visible blocks
let renderedRange = { start: 0, end: 0 }; // currently rendered blocks (>= visibleRange)
let parsedOffset = 0;     // how far parsing has progressed (lazy loading)
let totalLen = 0;         // total document length in bytes
let hasMoreToParse = false; // true if there are unparsed chunks
let chunkParseInProgress = false; // prevent concurrent chunk parsing

// ── Settings state ─────────────────────────────────────────────────────────
const SETTINGS_KEY = "womd_settings";
const settings = loadSettings();

/// Available interface themes.
const THEMES = [
  { id: "mocha", name: "Catppuccin Mocha", dark: true, swatches: ["#1e1e2e", "#cdd6f4", "#89b4fa", "#a6e3a1", "#f38ba8"] },
  { id: "latte", name: "Catppuccin Latte", dark: false, swatches: ["#eff1f5", "#4c4f69", "#1e66f5", "#40a02b", "#d20f39"] },
  { id: "monokai", name: "Monokai", dark: true, swatches: ["#272822", "#f8f8f2", "#66d9ef", "#a6e22e", "#f92672"] },
  { id: "solarized-dark", name: "Solarized Dark", dark: true, swatches: ["#002b36", "#93a1a1", "#268bd2", "#859900", "#dc322f"] },
  { id: "github-dark", name: "GitHub Dark", dark: true, swatches: ["#0d1117", "#c9d1d9", "#58a6ff", "#7ee787", "#f85149"] },
  { id: "dracula", name: "Dracula", dark: true, swatches: ["#282a36", "#f8f8f2", "#bd93f9", "#50fa7b", "#ff5555"] },
  { id: "one-dark", name: "One Dark", dark: true, swatches: ["#282c34", "#abb2bf", "#61afef", "#98c379", "#e06c75"] },
];

/// Available syntax palettes (same as themes — each theme has its own syntax colors).
const SYNTAX_PALETTES = THEMES; // 1:1 mapping for now

function loadSettings() {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (raw) return JSON.parse(raw);
  } catch {}
  return { theme: "mocha", syntaxTheme: "mocha" };
}

function saveSettings() {
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch {}
}

/// Apply the current theme to the document.
function applyTheme() {
  document.documentElement.setAttribute("data-theme", settings.theme);
}

// ── DOM ─────────────────────────────────────────────────────────────────────
const blockEditor = document.getElementById("block-editor");

// Virtualized scroll handler — re-renders visible blocks on scroll for large documents.
// Uses a larger render window to avoid frequent re-renders during scrolling.
let scrollRenderPending = false;
blockEditor.addEventListener("scroll", () => {
  if (!virtualizedMode) return;
  // If editing a block, exit edit mode first — scrolling away from the edited
  // block should commit the edit and allow normal scroll handling.
  // Then return — restoreBlockElement is async and needs to finish before
  // we destroy and recreate all elements via renderVirtualizedBlocks.
  // The next scroll event (or explicit scrollTop change) will handle rendering.
  if (editingBlockIndex >= 0) {
    exitEditMode();
    return;
  }
  if (suppressRender) return;
  if (scrollRenderPending) return;
  scrollRenderPending = true;
  requestAnimationFrame(() => {
    scrollRenderPending = false;
    const prevStart = renderedRange.start;
    const prevEnd = renderedRange.end;
    updateVisibleRange();
    // Re-render if visible range moved outside the rendered window,
    // OR if we scrolled far beyond parsed blocks (need to show loading indicator).
    const parsedHeight = cumulativeHeight(syntaxBlocks.length);
    const scrolledBeyondParsed = blockEditor.scrollTop > parsedHeight * 0.9;
    if (visibleRange.start < prevStart || visibleRange.end > prevEnd || scrolledBeyondParsed) {
      renderVirtualizedBlocks();
    }
    // Always check for chunk parsing — PgDown can jump far beyond parsed region.
    maybeParseNextChunk();
  });
});
const fileNameEl = document.getElementById("file-name");
const dirtyIndicator = document.getElementById("dirty-indicator");
const gitBranchInfo = document.getElementById("git-branch-info");
const cursorPos = document.getElementById("cursor-pos");
const blockCount = document.getElementById("block-count");
const btnNew = document.getElementById("btn-new");
const btnOpen = document.getElementById("btn-open");
const btnSave = document.getElementById("btn-save");
const btnUndo = document.getElementById("btn-undo");
const btnRedo = document.getElementById("btn-redo");
const btnBold = document.getElementById("btn-bold");
const btnItalic = document.getElementById("btn-italic");
const btnStrike = document.getElementById("btn-strike");
const btnCode = document.getElementById("btn-code");
const btnLink = document.getElementById("btn-link");
const btnUl = document.getElementById("btn-ul");
const btnOl = document.getElementById("btn-ol");
const btnQuote = document.getElementById("btn-quote");
const btnHr = document.getElementById("btn-hr");
const btnCodeblock = document.getElementById("btn-codeblock");
const btnTask = document.getElementById("btn-task");
const btnGit = document.getElementById("btn-git");
const btnViewToggle = document.getElementById("btn-view-toggle");
const btnTreeToggle = document.getElementById("btn-tree-toggle");
const fileTreePanel = document.getElementById("file-tree-panel");
const fileTreeDivider = document.getElementById("file-tree-divider");
const fileTreeContent = document.getElementById("file-tree-content");
const fileTreeRootName = document.getElementById("file-tree-root-name");
const fileTreeFilter = document.getElementById("file-tree-filter");
const fileContextMenu = document.getElementById("file-context-menu");
const selHeading = document.getElementById("sel-heading");
const tabList = document.getElementById("tab-list");
const tabNewBtn = document.getElementById("tab-new");
const gitPanel = document.getElementById("git-panel");
const gitChanges = document.getElementById("git-changes");
const gitBranches = document.getElementById("git-branches");
const gitHistory = document.getElementById("git-history");
const gitDiff = document.getElementById("git-diff");
const gitStash = document.getElementById("git-stash");
const gitTags = document.getElementById("git-tags");
const gitRemotes = document.getElementById("git-remotes");
const gitDivider = document.getElementById("git-divider");
const contextMenu = document.getElementById("context-menu");
const btnSettings = document.getElementById("btn-settings");
const settingsModal = document.getElementById("settings-modal");
const settingsClose = document.getElementById("settings-close");

// ── File dialog ─────────────────────────────────────────────────────────────
async function openFileDialog() {
  try {
    const tauri = window.__TAURI__;
    if (tauri?.dialog?.open) {
      const r = await tauri.dialog.open({
        title: "Open Markdown File", multiple: false, directory: false,
        filters: [{ name: "Markdown", extensions: ["md", "markdown", "txt"] }],
      });
      if (r) return r;
    }
  } catch (e) { console.warn("dialog.open:", e); }
  try {
    const r = await tauriInvoke("plugin:dialog|open", {
      options: { title: "Open Markdown File", multiple: false, directory: false,
        filters: [{ name: "Markdown", extensions: ["md", "markdown", "txt"] }] },
    });
    if (r) return r;
  } catch (e) { console.warn("plugin:dialog|open:", e); }
  return openFileViaInput();
}

function openFileViaInput() {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file"; input.accept = ".md,.markdown,.txt";
    input.style.display = "none";
    input.addEventListener("change", () => {
      const file = input.files?.[0];
      if (file) {
        const reader = new FileReader();
        reader.onload = () => resolve({ path: file.name, content: reader.result });
        reader.onerror = () => resolve(null);
        reader.readAsArrayBuffer(file);
      } else resolve(null);
    });
    document.body.appendChild(input); input.click(); document.body.removeChild(input);
  });
}

async function saveFileDialog() {
  try {
    const tauri = window.__TAURI__;
    if (tauri?.dialog?.save) {
      return await tauri.dialog.save({
        title: "Save Markdown File", defaultPath: "untitled.md",
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
    }
  } catch (e) { /* fall through */ }
  try {
    return await tauriInvoke("plugin:dialog|save", {
      options: { title: "Save Markdown File", defaultPath: "untitled.md",
        filters: [{ name: "Markdown", extensions: ["md"] }] },
    });
  } catch (e) { /* fall through */ }
  return prompt("Save to path:");
}

// ── Backend commands ────────────────────────────────────────────────────────

async function newDocument() {
  const info = await tauriInvoke("new_document");
  currentText = info.text || "";
  activeTabId = info.tab_id;
  updateUI(info);
  await refreshTabs();
  blockHeights = [];
  await refreshSyntax();
  editorFocusFirst();
}

async function openDocument(path) {
  // Show loading indicator for large files.
  blockEditor.innerHTML = `<div style="padding:24px;color:var(--fg-muted);text-align:center"><h2>Loading...</h2><p>Parsing document, please wait.</p></div>`;
  try {
    const info = await tauriInvoke("open_document", { path });
    currentText = info.text || "";
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshTabs();
    // Clear cached block heights — new document, old heights are invalid.
    blockHeights = [];
    await refreshSyntax();
    refreshGitAll();
    syncDiffTabIfVisible();
    editorFocusFirst();
  } catch (e) {
    blockEditor.innerHTML = `<div style="padding:24px;color:#f38ba8"><h2>Failed to open file</h2><p>${escapeHtml(String(e))}</p></div>`;
  }
}

async function saveDocument(path) {
  flushEdits();
  await tauriInvoke("save_document", { path: path || null });
  isDirty = false;
  updateDirtyState();
  await refreshTabs();
}

async function doUndo() {
  flushEdits();
  const r = await tauriInvoke("undo");
  if (r.text != null) currentText = r.text;
  updateUI(r);
  await refreshSyntax();
  scheduleAutosave();
}

async function doRedo() {
  flushEdits();
  const r = await tauriInvoke("redo");
  if (r.text != null) currentText = r.text;
  updateUI(r);
  await refreshSyntax();
  scheduleAutosave();
}

async function refreshSyntax() {
  try {
    // For large documents, fetch only metadata; full block data loaded on demand.
    const meta = await tauriInvoke("get_syntax_tree_meta");
    blockMeta = meta;
    virtualizedMode = meta.length > VIRTUALIZATION_THRESHOLD;
    if (virtualizedMode) {
      // Clear block cache — block data will be loaded for visible blocks.
      // Don't clear blockHeights — preserving cached heights prevents scroll
      // jumps when refreshSyntax is called after an edit (sendReplaceBlock).
      // New/changed blocks will use the default estimate until measured.
      blockCache.clear();
      // Build a lightweight syntaxBlocks array with just metadata.
      // source/node are loaded on demand for visible blocks.
      syntaxBlocks = meta.map(m => ({ kind: m.kind, source: "", start: m.start, end: m.end, node: null }));
      // Check if there are more chunks to parse (lazy loading).
      const [parsed, total] = await tauriInvoke("get_parsed_offset");
      parsedOffset = parsed;
      totalLen = total;
      hasMoreToParse = parsed < total;
    } else {
      // Small document — fetch full syntax tree in one go.
      syntaxBlocks = await tauriInvoke("get_syntax_tree");
      blockCache.clear();
      parsedOffset = 0;
      totalLen = 0;
      hasMoreToParse = false;
    }
  }
  catch (e) {
    syntaxBlocks = [];
    blockMeta = [];
    virtualizedMode = false;
    hasMoreToParse = false;
  }
  if (!suppressRender) renderBlocks();
}

async function refreshTabs() {
  try {
    const data = await tauriInvoke("get_tabs");
    openTabs = data.tabs;
    activeTabId = data.active;
    // Tear-off windows only show their own tab, not all tabs from shared state.
    if (isTearOffWindow) {
      openTabs = openTabs.filter(t => t.id === activeTabId);
    }
    renderTabs();
  } catch (e) { /* non-fatal */ }
}

let switchTabInProgress = false;
async function switchTab(tabId) {
  if (tabId === activeTabId) return;
  if (switchTabInProgress) return; // Guard against rapid switching race conditions
  switchTabInProgress = true;
  try {
    flushEdits();
    const info = await tauriInvoke("switch_tab", { tabId });
    // Verify the tab is still the one we want (user may have switched again).
    if (info.tab_id !== tabId) return;
    currentText = info.text || "";
    activeTabId = info.tab_id;
    updateUI(info);
    blockHeights = [];
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
    // Sync diff tab if it's currently visible.
    syncDiffTabIfVisible();
    editorFocusFirst();
  } finally {
    switchTabInProgress = false;
  }
}

async function closeTab(tabId) {
  const tab = openTabs.find(t => t.id === tabId);
  if (tab && tab.is_dirty) {
    if (!confirm(t("msg.close.unsaved", { name: tab.file_name }))) return;
  }
  flushEdits();
  const result = await tauriInvoke("close_tab", { tabId });
  if (result) {
    currentText = result.text || "";
    activeTabId = result.tab_id;
    updateUI(result);
    await refreshSyntax();
  } else {
    await newDocument();
    return;
  }
  await refreshTabs();
  refreshGitAll();
}

// Tear off a tab into a new window (drag tab outside tab bar).
// Creates the window from JS using the Tauri WebviewWindow API,
// which uses the same URL as the current window — guaranteed to load.
// File path is passed via localStorage (shared across same-origin windows).
async function tearOffTab(tabId) {
  const tab = openTabs.find(t => t.id === tabId);
  if (!tab) return;
  if (!tab.file_path) {
    alert(t("msg.no.filepath"));
    return;
  }
  try {
    const label = "win-" + Date.now();
    // Store the file path in localStorage so the new window can read it.
    localStorage.setItem("tear_off_" + label, tab.file_path);

    // Create the new window using the Tauri JS API.
    const tauri = window.__TAURI__;
    if (tauri?.webviewWindow?.WebviewWindow) {
      const webview = new tauri.webviewWindow.WebviewWindow(label, {
        url: window.location.href,
        title: "WoMD — " + tab.file_name,
        width: 1024,
        height: 768,
        minWidth: 640,
        minHeight: 480,
      });
      webview.once("tauri://created", () => {
        console.log("New window created:", label);
      });
      webview.once("tauri://error", (e) => {
        console.error("Window creation error:", e);
        alert(t("msg.new.window.fail", { err: e }));
      });
    } else {
      // Fallback: try backend command (if JS API not available).
      await tauriInvoke("open_new_window", {
        filePath: tab.file_path,
        fileName: tab.file_name,
      });
    }
  } catch (e) {
    console.error("tear-off failed:", e);
    alert(t("msg.open.window.fail", { err: e }));
  }
}

// ── Send full-text edit to backend ──────────────────────────────────────────

async function sendReplace(start, end, newText) {
  const r = await tauriInvoke("replace_text", { args: { start, end, new_text: newText } });
  // For large documents, r.text is null — don't update currentText.
  if (r.text != null) currentText = r.text;
  isDirty = r.is_dirty;
  updateDirtyState();
  blockCount.textContent = `${r.block_count} ${t("status.blocks")}`;
  await refreshSyntax();
  refreshTabs();
  scheduleAutosave();
  return r;
}

// Replace a single block by index — backend uses exact byte spans (no JS/byte offset mismatch).
async function sendReplaceBlock(blockIndex, newSource) {
  const r = await tauriInvoke("replace_block", { blockIndex, newSource });
  if (r.text != null) currentText = r.text;
  isDirty = r.is_dirty;
  updateDirtyState();
  blockCount.textContent = `${r.block_count} ${t("status.blocks")}`;
  await refreshSyntax();
  refreshTabs();
  scheduleAutosave();
  return r;
}

function flushEdits() {
  clearTimeout(debounceTimer);
  // Don't flush while suppressRender is true — we're in a block transition
  // (enterEditMode called exitEditMode on the previous block, which called
  // sendReplaceBlock → scheduleAutosave). The autosave timer fires later
  // and calls flushEdits, but at that point we're editing a new block.
  // flushEdits would commit the NEW block's edit, resetting suppressRender
  // and editingBlockIndex, causing refreshSyntax → renderBlocks → scroll reset.
  if (editingBlockIndex >= 0 && !suppressRender) {
    // Commit current edit synchronously into currentText.
    const ta = blockEditor.querySelector(".md-block.editing .md-block-textarea");
    if (ta) commitEdit(ta.value);
  }
}

// ── Autosave ────────────────────────────────────────────────────────────────

function scheduleAutosave() {
  clearTimeout(autosaveTimer);
  clearTimeout(autosaveInnerTimer);
  if (!isDirty) return;
  autosaveTimer = setTimeout(async () => {
    const tab = openTabs.find(t => t.id === activeTabId);
    if (tab && tab.file_name !== "untitled.md" && tab.file_name !== "untitled" && isDirty) {
      try {
        flushEdits();
        autosaveInnerTimer = setTimeout(async () => {
          if (isDirty) { try { await saveDocument(); } catch (e) { console.error("autosave:", e); } }
        }, 400);
      } catch (e) { console.error("autosave:", e); }
    }
  }, 2000);
}

// ── Block rendering (WYSIWYG) ───────────────────────────────────────────────
// We get blocks from the backend's syntax tree. Each block has a kind and
// source text. We render the source as HTML (rendered Markdown) for display.
// Clicking a block switches it to a textarea with raw source for editing.

function renderBlocks() {
  if (editingBlockIndex >= 0) return; // don't re-render while editing
  if (suppressRender) return;
  blockEditor.innerHTML = "";

  // Source view mode — show raw Markdown.
  if (viewMode === "source") {
    blockEditor.classList.add("source-view");
    const pre = document.createElement("div");
    pre.className = "md-source-view";
    // For large documents, currentText is empty — load on demand.
    if (!currentText && virtualizedMode) {
      pre.textContent = "Loading source text...";
      blockEditor.appendChild(pre);
      // Load full text for source view (one-time, user explicitly switched).
      tauriInvoke("get_document_text").then(text => {
        currentText = text;
        pre.textContent = text;
      }).catch(e => {
        pre.textContent = "Error loading text: " + String(e);
      });
    } else {
      pre.textContent = currentText;
      blockEditor.appendChild(pre);
    }
    return;
  }
  blockEditor.classList.remove("source-view");

  if (syntaxBlocks.length === 0) {
    const empty = document.createElement("div");
    empty.className = "md-block empty-block";
    empty.textContent = t("msg.empty.doc");
    empty.addEventListener("click", () => {
      // Insert a new paragraph block.
      currentText += (currentText.length > 0 ? "\n\n" : "") + "";
      sendReplace(0, currentText.length, currentText).then(() => {
        // Enter edit mode on the new block.
        setTimeout(() => enterEditMode(syntaxBlocks.length - 1), 50);
      });
    });
    blockEditor.appendChild(empty);
    return;
  }

  if (virtualizedMode) {
    renderVirtualizedBlocks();
  } else {
    renderAllBlocks();
  }
}

/// Render all blocks (small documents).
function renderAllBlocks() {
  for (let i = 0; i < syntaxBlocks.length; i++) {
    const block = syntaxBlocks[i];
    if (block.kind === "blank-line") continue;
    if (block.kind === "link-ref-def") continue;
    const el = createBlockElement(i, block);
    blockEditor.appendChild(el);
  }
  addTrailingBlock();
}

/// Estimate total document height in pixels, including unparsed region.
/// Uses parsedOffset/totalLen ratio to extrapolate from parsed block count.
/// Get the cached height for a block, or the default estimate.
function getBlockHeight(i) {
  return blockHeights[i] || AVG_BLOCK_HEIGHT;
}

/// Measure and cache the real heights of rendered blocks after they are in the DOM.
/// This is called after renderVirtualizedBlocks to update blockHeights for
/// the currently rendered range. On the next render, spacers use these real
/// heights, making scrolling smooth (no jumps from height estimation errors).
function measureRenderedBlockHeights() {
  for (let i = renderedRange.start; i <= renderedRange.end && i < syntaxBlocks.length; i++) {
    const el = blockEditor.querySelector(`[data-block-index="${i}"]`);
    if (el) {
      const h = el.offsetHeight;
      if (h > 0) blockHeights[i] = h;
    }
  }
}

/// Calculate the cumulative height of blocks [0, end) using cached heights
/// or the default estimate for unknown blocks.
function cumulativeHeight(end) {
  let total = 0;
  for (let i = 0; i < end && i < syntaxBlocks.length; i++) {
    total += getBlockHeight(i);
  }
  return total;
}

/// Estimate total document height in pixels, including unparsed region.
function estimateTotalHeight() {
  const parsedHeight = cumulativeHeight(syntaxBlocks.length);
  if (!hasMoreToParse || parsedOffset === 0) {
    return parsedHeight;
  }
  // Extrapolate: if parsedOffset bytes produced syntaxBlocks.length blocks,
  // totalLen bytes would produce proportionally more.
  const ratio = totalLen / parsedOffset;
  const estimatedTotalBlocks = syntaxBlocks.length * ratio;
  const avgPerBlock = parsedHeight / syntaxBlocks.length;
  return parsedHeight + (estimatedTotalBlocks - syntaxBlocks.length) * avgPerBlock;
}

/// Render only visible blocks with spacers for virtualized scrolling (large documents).
/// Renders a large window (RENDER_WINDOW blocks) so scrolling doesn't trigger
/// re-renders on every frame. Re-render only when scroll moves outside the window.
/// Uses cached real block heights for spacer calculations to avoid scroll jumps.
function renderVirtualizedBlocks() {
  // Save scroll position before clearing (innerHTML resets scrollTop to 0).
  const savedScrollTop = blockEditor.scrollTop;
  blockEditor.innerHTML = "";
  // Compute visible range from saved scroll position.
  updateVisibleRange(savedScrollTop);

  // Expand to render window: center the visible range in a larger window.
  const visibleCount = visibleRange.end - visibleRange.start + 1;
  const extraAbove = Math.floor((RENDER_WINDOW - visibleCount) / 2);
  const extraBelow = RENDER_WINDOW - visibleCount - extraAbove;
  renderedRange = {
    start: Math.max(0, visibleRange.start - extraAbove),
    end: Math.min(syntaxBlocks.length - 1, visibleRange.end + extraBelow),
  };

  // Top spacer — uses cumulative cached heights for accuracy.
  const topSpacer = document.createElement("div");
  topSpacer.className = "md-virtual-spacer";
  topSpacer.id = "md-virtual-top-spacer";
  topSpacer.style.height = cumulativeHeight(renderedRange.start) + "px";
  blockEditor.appendChild(topSpacer);

  // Render blocks in the render window.
  let renderedAny = false;
  for (let i = renderedRange.start; i <= renderedRange.end && i < syntaxBlocks.length; i++) {
    renderedAny = true;
    const block = syntaxBlocks[i];
    if (block.kind === "blank-line") continue;
    if (block.kind === "link-ref-def") continue;
    const el = createBlockElement(i, block);
    // If block data not loaded yet, show placeholder and load async.
    if (!block.source && virtualizedMode) {
      el.innerHTML = `<div class="md-block-placeholder" style="padding:8px;color:var(--fg-muted)">…</div>`;
      loadBlockData(i, el);
    }
    blockEditor.appendChild(el);
  }

  // Bottom spacer — accounts for BOTH unparsed blocks below the render window
  // AND the remaining unparsed document region.
  const bottomSpacer = document.createElement("div");
  bottomSpacer.className = "md-virtual-spacer";
  bottomSpacer.id = "md-virtual-bottom-spacer";
  const totalHeight = estimateTotalHeight();
  const topSpacerHeight = cumulativeHeight(renderedRange.start);
  // Height of rendered blocks (use cached heights, not estimates).
  let renderedHeight = 0;
  for (let i = renderedRange.start; i <= renderedRange.end && i < syntaxBlocks.length; i++) {
    renderedHeight += getBlockHeight(i);
  }
  let bottomHeight = totalHeight - topSpacerHeight - renderedHeight;
  if (bottomHeight < 0) bottomHeight = 0;
  bottomSpacer.style.height = bottomHeight + "px";
  blockEditor.appendChild(bottomSpacer);

  // If user scrolled into the unparsed region, show a loading indicator.
  if (hasMoreToParse && !renderedAny && savedScrollTop > cumulativeHeight(syntaxBlocks.length) * 0.9) {
    const loading = document.createElement("div");
    loading.className = "md-chunk-loading";
    loading.textContent = "Loading more content...";
    loading.style.cssText = "padding:24px;text-align:center;color:var(--fg-muted);font-size:14px";
    blockEditor.appendChild(loading);
  }

  addTrailingBlock();

  // Restore scroll position (innerHTML reset it to 0).
  blockEditor.scrollTop = savedScrollTop;

  // Measure real block heights after DOM is updated (next frame) to
  // improve spacer accuracy for the next render.
  requestAnimationFrame(() => {
    measureRenderedBlockHeights();
  });
}

/// Update visibleRange based on scroll position.
/// Accepts optional scrollTop param (used when called after innerHTML reset,
/// since blockEditor.scrollTop will be 0 at that point).
/// Note: visibleRange is clamped to syntaxBlocks.length (parsed blocks only).
/// The bottom spacer accounts for the unparsed region so the user can scroll
/// beyond parsed blocks — maybeParseNextChunk triggers on scroll into that region.
function updateVisibleRange(scrollTopArg) {
  const scrollTop = scrollTopArg != null ? scrollTopArg : (blockEditor.scrollTop || 0);
  const viewportHeight = blockEditor.clientHeight || 600;
  // Use cumulative cached heights to find which blocks are visible.
  // This is more accurate than dividing by a fixed avgHeight, preventing
  // the visible range from being wrong when blocks have varying heights.
  let firstVisible = 0;
  let lastVisible = 0;
  let cumY = 0;
  for (let i = 0; i < syntaxBlocks.length; i++) {
    const h = getBlockHeight(i);
    if (cumY + h > scrollTop - 100) { // 100px buffer above
      firstVisible = i;
      break;
    }
    cumY += h;
  }
  cumY = 0;
  for (let i = 0; i < syntaxBlocks.length; i++) {
    const h = getBlockHeight(i);
    cumY += h;
    if (cumY > scrollTop + viewportHeight + 100) { // 100px buffer below
      lastVisible = i;
      break;
    }
    lastVisible = i;
  }
  visibleRange = { start: Math.max(0, firstVisible), end: Math.min(syntaxBlocks.length - 1, lastVisible) };
}

/// Trigger parsing of the next 10MB chunk if the user is scrolling near the
/// end of the currently-parsed region. This enables lazy loading of very
/// large files (>20MB) without parsing the entire file upfront.
/// Checks by pixel position: if the user is within 2 screens of the end of
/// parsed content, trigger the next chunk.
async function maybeParseNextChunk() {
  if (!hasMoreToParse || chunkParseInProgress) return;
  const scrollTop = blockEditor.scrollTop || 0;
  const viewportHeight = blockEditor.clientHeight || 600;
  // Estimate where the parsed content ends in pixels.
  const parsedContentHeight = cumulativeHeight(syntaxBlocks.length);
  // If user is within 2 viewport heights of the end of parsed content, trigger.
  if (scrollTop + viewportHeight * 2 < parsedContentHeight) return;
  chunkParseInProgress = true;
  try {
    const [newOffset, total, blockCount] = await tauriInvoke("parse_next_chunk");
    parsedOffset = newOffset;
    hasMoreToParse = newOffset < total;
    // Re-fetch metadata and re-render with the new blocks.
    const meta = await tauriInvoke("get_syntax_tree_meta");
    blockMeta = meta;
    syntaxBlocks = meta.map(m => ({ kind: m.kind, source: "", start: m.start, end: m.end, node: null }));
    blockCount.textContent = `${meta.length} ${t("status.blocks")}`;
    // Re-render to show the newly parsed blocks.
    renderVirtualizedBlocks();
    // Recursively check if we need more chunks (user might have scrolled very far).
    if (hasMoreToParse) {
      setTimeout(() => maybeParseNextChunk(), 50);
    }
  } catch (e) {
    console.error("Failed to parse next chunk:", e);
  } finally {
    chunkParseInProgress = false;
  }
}

/// Load full block data on demand and update the placeholder element.
async function loadBlockData(blockIndex, el) {
  if (blockCache.has(blockIndex)) {
    const block = blockCache.get(blockIndex);
    syntaxBlocks[blockIndex] = block;
    el.innerHTML = renderBlockHtml(block);
    attachBlockListeners(el, blockIndex);
    return;
  }
  try {
    const block = await tauriInvoke("get_block_data", { blockIndex });
    if (block) {
      blockCache.set(blockIndex, block);
      syntaxBlocks[blockIndex] = block;
      el.innerHTML = renderBlockHtml(block);
      attachBlockListeners(el, blockIndex);
    }
  } catch (e) {
    el.innerHTML = `<div style="padding:8px;color:var(--diff-del)">Error: ${escapeHtml(String(e))}</div>`;
  }
}

/// Attach event listeners to a block element (extracted from createBlockElement for virtualization).
function attachBlockListeners(el, index) {
  let mouseDownX = 0, mouseDownY = 0;
  el.addEventListener("mousedown", (e) => {
    mouseDownX = e.clientX; mouseDownY = e.clientY;
    // Set suppressBlur BEFORE blur fires — see createBlockElement for details.
    if (editingBlockIndex >= 0 && editingBlockIndex !== index) {
      suppressBlur = true;
      suppressRender = true;
    }
  });
  el.addEventListener("click", (e) => {
    if (e.target.closest("a")) return;
    if (editingBlockIndex === index) return;
    const dx = Math.abs(e.clientX - mouseDownX);
    const dy = Math.abs(e.clientY - mouseDownY);
    if (dx > 3 || dy > 3) return;
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed && sel.toString().trim().length > 0) return;
    e.stopPropagation();
    enterEditMode(index);
  });
}

/// Add trailing empty block for appending content.
function addTrailingBlock() {
  const trailing = document.createElement("div");
  trailing.className = "md-block empty-block";
  trailing.style.minHeight = "2em";
  trailing.addEventListener("click", () => {
    currentText += (currentText.length > 0 ? "\n\n" : "") + "New paragraph";
    sendReplace(0, currentText.length, currentText).then(() => {
      const idx = syntaxBlocks.length - 1;
      setTimeout(() => enterEditMode(idx), 50);
    });
  });
  blockEditor.appendChild(trailing);
}

function createBlockElement(index, block) {
  const el = document.createElement("div");
  el.className = "md-block";
  el.dataset.blockIndex = index;
  el.innerHTML = renderBlockHtml(block);
  // Track mousedown position to detect drag-selection vs simple click.
  let mouseDownX = 0, mouseDownY = 0;
  el.addEventListener("mousedown", (e) => {
    mouseDownX = e.clientX; mouseDownY = e.clientY;
    // If we're editing another block, set suppressBlur BEFORE the blur fires.
    // mousedown causes the textarea to lose focus → blur fires synchronously
    // after mousedown. Without this, blur → exitEditMode → renderBlocks →
    // innerHTML='' destroys all elements and resets scroll before the click
    // handler can call enterEditMode.
    if (editingBlockIndex >= 0 && editingBlockIndex !== index) {
      suppressBlur = true;
      suppressRender = true;
    }
  });
  el.addEventListener("click", (e) => {
    // If the click is on a link, let it bubble to blockEditor's link handler.
    if (e.target.closest("a")) return;
    if (editingBlockIndex === index) return;
    // Check if this was a drag-selection (mouse moved between mousedown and click).
    const dx = Math.abs(e.clientX - mouseDownX);
    const dy = Math.abs(e.clientY - mouseDownY);
    if (dx > 3 || dy > 3) return; // it was a selection, don't enter edit mode
    // Check if there's a text selection.
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed && sel.toString().trim().length > 0) return;
    e.stopPropagation();
    enterEditMode(index);
  });
  return el;
}

function renderBlockHtml(block) {
  // If we have a parsed AST node, use it for rich rendering.
  if (block.node) return renderAstNode(block.node);
  // Fallback: show source as a paragraph.
  return `<p>${escapeHtml(block.source)}</p>`;
}

// ── AST-based rendering ─────────────────────────────────────────────────────
// Walks the JSON AST tree from the backend and produces HTML.

function renderAstNode(node) {
  if (!node) return "";
  switch (node.type) {
    case "Heading":
      return `<h${node.level}>${renderAstChildren(node.children)}</h${node.level}>`;
    case "Paragraph":
      return `<p>${renderAstChildren(node.children)}</p>`;
    case "ThematicBreak":
      return "<hr/>";
    case "BlockQuote":
      return `<blockquote>${renderAstChildren(node.children)}</blockquote>`;
    case "List":
      return renderAstList(node);
    case "CodeBlock":
      return `<pre><code${node.language ? ` class="language-${escapeAttr(node.language)}"` : ""}>${escapeHtml(node.content)}</code></pre>`;
    case "Table":
      return renderAstTable(node);
    case "HtmlBlock":
      // Escape raw HTML to prevent XSS in Tauri webview (which has IPC access).
      return `<pre class="md-html-block">${escapeHtml(node.content)}</pre>`;
    case "LinkRefDef":
      // Hidden in WYSIWYG — link reference definitions are not visible content.
      return `<div style="display:none"></div>`;
    case "BlankLine":
      return ""; // Blank lines are spacing, not visible content
    // Inline nodes:
    case "Text":
      return escapeHtml(node.text);
    case "Emphasis":
      return `<em>${renderAstChildren(node.children)}</em>`;
    case "Strong":
      return `<strong>${renderAstChildren(node.children)}</strong>`;
    case "Strikethrough":
      return `<del>${renderAstChildren(node.children)}</del>`;
    case "CodeSpan":
      return `<code>${escapeHtml(node.text)}</code>`;
    case "Link":
      return `<a href="${escapeAttr(node.destination)}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""}>${renderAstChildren(node.children)}</a>`;
    case "Image":
      return `<img alt="${escapeAttr(node.alt)}" src="${escapeAttr(node.destination)}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""} />`;
    case "Autolink":
      return `<a href="${escapeAttr(node.url)}">${escapeHtml(node.url)}</a>`;
    case "HardBreak":
      return "<br/>";
    case "RawHtml":
      // Escape raw inline HTML to prevent XSS.
      return escapeHtml(node.content);
    default:
      return "";
  }
}

function renderAstChildren(children) {
  if (!children) return "";
  return children.map(c => renderAstNode(c)).join("");
}

function renderAstList(node) {
  const tag = node.ordered ? "ol" : "ul";
  let html = `<${tag}${node.ordered && node.start !== 1 ? ` start="${node.start}"` : ""}>`;
  for (const item of node.items) {
    if (item.task) {
      const checked = item.task === "done";
      // Task list item: checkbox + content
      const content = renderAstChildren(item.children);
      html += `<li class="task-item"><input type="checkbox" ${checked ? "checked" : ""} disabled /><span>${content}</span></li>`;
    } else {
      // Normal list item: render children blocks
      const content = item.children.map(c => renderAstNode(c)).join("");
      // If children are paragraphs, strip the <p> wrapper for tight lists.
      html += `<li>${content}</li>`;
    }
  }
  html += `</${tag}>`;
  return html;
}

function renderAstTable(node) {
  let html = "<table><thead><tr>";
  for (let i = 0; i < node.header.length; i++) {
    const cell = node.header[i];
    const align = node.alignments[i] || "none";
    const style = align !== "none" ? ` style="text-align:${align}"` : "";
    html += `<th${style}>${renderAstChildren(cell.children)}</th>`;
  }
  html += "</tr></thead><tbody>";
  for (const row of node.rows) {
    html += "<tr>";
    for (let i = 0; i < row.length; i++) {
      const cell = row[i];
      const align = node.alignments[i] || "none";
      const style = align !== "none" ? ` style="text-align:${align}"` : "";
      html += `<td${style}>${renderAstChildren(cell.children)}</td>`;
    }
    html += "</tr>";
  }
  html += "</tbody></table>";
  return html;
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
function escapeAttr(s) { return s.replace(/"/g, "&quot;").replace(/'/g, "&#39;"); }

/// Get the parent directory of a path (handles both / and \ separators).
function parentDir(p) {
  const idx = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  return idx >= 0 ? p.substring(0, idx) : p;
}
/// Get the base name (last segment) of a path.
function baseName(p) {
  const idx = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  return idx >= 0 ? p.substring(idx + 1) : p;
}
/// Join path segments with the correct separator for the path.
function joinPath(dir, name) {
  const sep = dir.includes("\\") && !dir.includes("/") ? "\\" : "/";
  return dir + sep + name;
}

// ── Click-to-edit ───────────────────────────────────────────────────────────

async function enterEditMode(blockIndex) {
  // If already editing another block, exit it first.
  // suppressBlur/suppressRender are already set by the mousedown handler
  // on the clicked block, but set them here as fallback for keyboard-initiated
  // transitions (e.g. Escape then click).
  if (editingBlockIndex >= 0) {
    suppressBlur = true;
    suppressRender = true;
    exitEditMode();
  }
  if (blockIndex < 0 || blockIndex >= syntaxBlocks.length) {
    suppressRender = false;
    return;
  }

  // Set editingBlockIndex BEFORE any await — during the async get_block_data
  // call, scroll events or other handlers might fire. If editingBlockIndex
  // is still -1, the scroll handler would call renderVirtualizedBlocks()
  // which does innerHTML='' and resets scroll.
  editingBlockIndex = blockIndex;
  suppressRender = true;

  // In virtualized mode, ensure block data is loaded before editing.
  if (virtualizedMode && !syntaxBlocks[blockIndex].source) {
    try {
      const block = await tauriInvoke("get_block_data", { blockIndex });
      if (block) {
        blockCache.set(blockIndex, block);
        syntaxBlocks[blockIndex] = block;
      }
    } catch (e) { /* fallback to empty source */ }
  }

  const blockEl = blockEditor.querySelector(`[data-block-index="${blockIndex}"]`);
  if (!blockEl) { editingBlockIndex = -1; suppressRender = false; return; }

  const source = syntaxBlocks[blockIndex].source || "";
  blockEl.classList.add("editing");
  blockEl.innerHTML = "";

  const ta = document.createElement("textarea");
  ta.className = "md-block-textarea";
  ta.value = source;
  ta.spellcheck = false;
  blockEl.appendChild(ta);

  // Auto-size.
  autoSizeTextarea(ta);

  // focus with preventScroll to avoid the browser scrolling to the element.
  ta.focus({ preventScroll: true });
  // Place cursor at end.
  ta.selectionStart = ta.value.length;
  ta.selectionEnd = ta.value.length;

  ta.addEventListener("input", () => autoSizeTextarea(ta));
  ta.addEventListener("blur", () => {
    if (suppressBlur) { suppressBlur = false; return; }
    exitEditMode();
  });
  // Mouse wheel over textarea — exit edit mode and scroll the container.
  // Without this, wheel events are captured by the textarea and blockEditor
  // never receives a scroll event, so the user can't scroll while editing.
  ta.addEventListener("wheel", (e) => {
    e.preventDefault();
    // Save scrollTop BEFORE exitEditMode — replacing textarea with rendered
    // HTML changes the block's height, causing the browser to adjust scrollTop.
    // We compute the target scroll position and set it AFTER restoreBlockElement
    // completes, because restoreBlockElement changes the DOM multiple times
    // (textarea → placeholder → rendered HTML), each time shifting scrollTop.
    const savedScrollTop = blockEditor.scrollTop;
    const targetScrollTop = savedScrollTop + e.deltaY;
    suppressBlur = false;
    const restorePromise = exitEditMode();
    // Set scrollTop immediately so the user sees the scroll response.
    blockEditor.scrollTop = targetScrollTop;
    // After restoreBlockElement finishes (DOM changes complete), re-set
    // scrollTop to the target — the browser may have shifted it during
    // async DOM updates. Then render.
    restorePromise.then(() => {
      if (editingBlockIndex < 0) {
        blockEditor.scrollTop = targetScrollTop;
        suppressRender = false;
        renderVirtualizedBlocks();
        maybeParseNextChunk();
      }
    });
  }, { passive: false });
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); exitEditMode(); return; }
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); exitEditMode(); return; }
    // PgUp/PgDown/Home/End — exit edit mode and scroll the container.
    // Without this, these keys move the cursor inside the textarea instead
    // of scrolling the document, so the user can't navigate while editing.
    if (e.key === "PageUp" || e.key === "PageDown" || e.key === "Home" || e.key === "End") {
      e.preventDefault();
      const viewportHeight = blockEditor.clientHeight || 600;
      const scrollDelta = e.key === "PageUp" ? -viewportHeight
                        : e.key === "PageDown" ? viewportHeight
                        : e.key === "Home" ? -blockEditor.scrollTop
                        : blockEditor.scrollHeight;
      const savedScrollTop = blockEditor.scrollTop;
      const targetScrollTop = savedScrollTop + scrollDelta;
      suppressBlur = false;
      const restorePromise = exitEditMode();
      blockEditor.scrollTop = targetScrollTop;
      restorePromise.then(() => {
        if (editingBlockIndex < 0) {
          blockEditor.scrollTop = targetScrollTop;
          suppressRender = false;
          renderVirtualizedBlocks();
          maybeParseNextChunk();
        }
      });
      return;
    }
    // Shift+Enter = newline in block. Plain Enter for single-line blocks = save.
    if (e.key === "Enter" && !e.shiftKey && !e.ctrlKey) {
      const kind = syntaxBlocks[blockIndex].kind;
      if (kind.startsWith("heading") || kind === "paragraph") {
        e.preventDefault();
        exitEditMode();
        return;
      }
    }
    // Formatting shortcuts operate on textarea selection.
    // stopPropagation prevents the document-level handler from also firing
    // (which would apply formatting a second time).
    if (e.ctrlKey || e.metaKey) {
      if (e.key === "b") { e.preventDefault(); e.stopPropagation(); toggleWrap(ta, "**", "**"); }
      if (e.key === "i") { e.preventDefault(); e.stopPropagation(); toggleWrap(ta, "*", "*"); }
      if (e.key === "k") { e.preventDefault(); e.stopPropagation(); insertLinkInTextarea(ta); }
    }
  });
}

function exitEditMode() {
  if (editingBlockIndex < 0) return Promise.resolve();
  const idx = editingBlockIndex;
  editingBlockIndex = -1;
  // If suppressRender is true, it was set by a mousedown handler for a
  // block transition (enterEditMode will be called next). In that case,
  // don't reset it — enterEditMode manages it.
  // If suppressRender is false (standalone exit: blur, scroll, Escape),
  // keep it false — restoreBlockElement will handle the DOM update.
  // If suppressRender is true but we're NOT in a transition (e.g. scroll
  // triggered exit), reset it so future renders work.
  // We detect transition by checking if suppressBlur is true (set by
  // mousedown for transitions, not for scroll/blur/Escape exits).
  if (suppressRender && !suppressBlur) {
    suppressRender = false;
  }
  let restorePromise = Promise.resolve();
  const blockEl = blockEditor.querySelector(`[data-block-index="${idx}"]`);
  if (blockEl) {
    const ta = blockEl.querySelector(".md-block-textarea");
    if (ta) {
      const newSource = ta.value;
      // Restore the block element in-place instead of full re-render.
      // Return the promise so callers can await completion before rendering.
      restorePromise = restoreBlockElement(idx, blockEl, newSource);
    }
    blockEditor.focus({ preventScroll: true });
  }
  // If suppressRender is still true (transition), don't reset — enterEditMode will.
  if (suppressRender) return restorePromise;
  suppressRender = false;
  return restorePromise;
}

/// Restore a single block element from textarea back to rendered HTML.
/// Avoids full renderBlocks() which would destroy all elements and reset scroll.
/// At the end, resets suppressRender to false if this is a standalone exit
/// (editingBlockIndex < 0). If editingBlockIndex >= 0 (transition to another
/// block), leaves suppressRender true so the new edit mode is not disrupted.
async function restoreBlockElement(idx, blockEl, newSource) {
  const oldSource = syntaxBlocks[idx].source;
  if (newSource === oldSource) {
    // Source unchanged — just restore rendered HTML in-place.
    blockEl.classList.remove("editing");
    // In virtualized mode, ensure block data (node + source) is loaded.
    if (virtualizedMode && !syntaxBlocks[idx].node) {
      try {
        const block = await tauriInvoke("get_block_data", { blockIndex: idx });
        if (block) {
          blockCache.set(idx, block);
          syntaxBlocks[idx] = block;
        }
      } catch (e) { /* fallback to empty */ }
    }
    // Save scrollTop before DOM change — replacing content changes block
    // height, causing the browser to adjust scrollTop.
    const savedScroll = blockEditor.scrollTop;
    blockEl.innerHTML = renderBlockHtml(syntaxBlocks[idx]);
    attachBlockListeners(blockEl, idx);
    blockEditor.scrollTop = savedScroll;
    // Cache the block's new height so renderVirtualizedBlocks uses it.
    requestAnimationFrame(() => {
      const el = blockEditor.querySelector(`[data-block-index="${idx}"]`);
      if (el && el.offsetHeight > 0) blockHeights[idx] = el.offsetHeight;
    });
    return;
  }
  // Source changed — send to backend, then update in-place.
  // Keep suppressRender=true during sendReplaceBlock → refreshSyntax to
  // prevent refreshSyntax from calling renderBlocks() (which would do
  // innerHTML='' and reset scroll). We'll update the block element manually.
  blockEl.classList.remove("editing");
  const savedScroll1 = blockEditor.scrollTop;
  blockEl.innerHTML = `<div class="md-block-placeholder" style="padding:8px;color:var(--fg-muted)">…</div>`;
  blockEditor.scrollTop = savedScroll1;
  const wasSuppressRender = suppressRender;
  suppressRender = true;
  try {
    await sendReplaceBlock(idx, newSource);
    // sendReplaceBlock → refreshSyntax rebuilds syntaxBlocks with empty
    // source/node (virtualized mode). Must reload block data before rendering.
    // Reload block data — refreshSyntax cleared it.
    try {
      const block = await tauriInvoke("get_block_data", { blockIndex: idx });
      if (block) {
        blockCache.set(idx, block);
        syntaxBlocks[idx] = block;
      }
    } catch (e) { /* fallback to empty */ }
    const el = blockEditor.querySelector(`[data-block-index="${idx}"]`);
    if (el) {
      const savedScroll2 = blockEditor.scrollTop;
      el.innerHTML = renderBlockHtml(syntaxBlocks[idx]);
      attachBlockListeners(el, idx);
      blockEditor.scrollTop = savedScroll2;
    }
  } catch (e) {
    // Restore original on error — reload block data first.
    try {
      const block = await tauriInvoke("get_block_data", { blockIndex: idx });
      if (block) {
        blockCache.set(idx, block);
        syntaxBlocks[idx] = block;
      }
    } catch (e2) { /* give up */ }
    const savedScroll3 = blockEditor.scrollTop;
    blockEl.innerHTML = renderBlockHtml(syntaxBlocks[idx]);
    attachBlockListeners(blockEl, idx);
    blockEditor.scrollTop = savedScroll3;
  }
  // Restore suppressRender to its previous value — if it was false before
  // (standalone exit), keep it false so the caller can render. If it was
  // true (transition), keep it true.
  suppressRender = wasSuppressRender;
  // Cache the block's new height so renderVirtualizedBlocks uses it.
  requestAnimationFrame(() => {
    const el = blockEditor.querySelector(`[data-block-index="${idx}"]`);
    if (el && el.offsetHeight > 0) blockHeights[idx] = el.offsetHeight;
  });
}

async function commitEdit(newSource) {
  // Legacy path — called from flushEdits before save/undo/redo.
  // exitEditMode now handles the in-place restoration directly.
  const idx = editingBlockIndex;
  if (idx < 0) return;
  editingBlockIndex = -1;
  suppressRender = false;

  if (idx >= syntaxBlocks.length) return;
  const oldSource = syntaxBlocks[idx].source;
  if (newSource === oldSource) {
    renderBlocks();
    return;
  }

  // Use replace_block: backend uses exact byte spans from the AST,
  // avoiding JS string index vs byte offset mismatch for non-ASCII text.
  await sendReplaceBlock(idx, newSource);
}

function autoSizeTextarea(ta) {
  ta.style.height = "auto";
  ta.style.height = ta.scrollHeight + "px";
}

function editorFocusFirst() {
  // No-op for block editor; user clicks to edit.
}

// ── Formatting on textarea (in edit mode) ───────────────────────────────────

function toggleWrap(ta, prefix, suffix) {
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const text = ta.value;
  const selText = text.substring(start, end);
  const before = text.substring(Math.max(0, start - prefix.length), start);
  const after = text.substring(end, end + suffix.length);

  if (before === prefix && after === suffix) {
    ta.value = text.substring(0, start - prefix.length) + selText + text.substring(end + suffix.length);
    ta.selectionStart = start - prefix.length;
    ta.selectionEnd = end - prefix.length;
  } else if (selText.startsWith(prefix) && selText.endsWith(suffix) && selText.length >= prefix.length + suffix.length) {
    const inner = selText.substring(prefix.length, selText.length - suffix.length);
    ta.value = text.substring(0, start) + inner + text.substring(end);
    ta.selectionStart = start;
    ta.selectionEnd = start + inner.length;
  } else {
    ta.value = text.substring(0, start) + prefix + selText + suffix + text.substring(end);
    ta.selectionStart = start + prefix.length;
    ta.selectionEnd = end + prefix.length;
  }
  ta.focus();
  autoSizeTextarea(ta);
}

function toggleLinePrefix(ta, prefix) {
  const start = ta.selectionStart;
  const text = ta.value;
  const lineStart = text.lastIndexOf("\n", start - 1) + 1;
  let lineEnd = text.indexOf("\n", start);
  if (lineEnd === -1) lineEnd = text.length;
  const line = text.substring(lineStart, lineEnd);
  if (line.startsWith(prefix)) {
    const stripped = line.substring(prefix.length);
    ta.value = text.substring(0, lineStart) + stripped + text.substring(lineEnd);
    ta.selectionStart = Math.max(lineStart, start - prefix.length);
    ta.selectionEnd = Math.max(lineStart, start - prefix.length);
  } else {
    ta.value = text.substring(0, lineStart) + prefix + line + text.substring(lineEnd);
    ta.selectionStart = start + prefix.length;
    ta.selectionEnd = start + prefix.length;
  }
  ta.focus();
  autoSizeTextarea(ta);
}

function toggleHeadingInTextarea(ta, level) {
  const start = ta.selectionStart;
  const text = ta.value;
  const lineStart = text.lastIndexOf("\n", start - 1) + 1;
  let lineEnd = text.indexOf("\n", start);
  if (lineEnd === -1) lineEnd = text.length;
  const line = text.substring(lineStart, lineEnd);
  const stripped = line.replace(/^#{1,6}\s+/, "");
  const prefix = level > 0 ? "#".repeat(level) + " " : "";
  ta.value = text.substring(0, lineStart) + prefix + stripped + text.substring(lineEnd);
  ta.selectionStart = lineStart + prefix.length;
  ta.selectionEnd = lineStart + prefix.length + stripped.length;
  ta.focus();
  autoSizeTextarea(ta);
}

function insertLinkInTextarea(ta) {
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const selText = ta.value.substring(start, end) || "link text";
  const url = prompt("Enter URL:", "https://");
  if (!url) return;
  const replacement = `[${selText}](${url})`;
  ta.value = ta.value.substring(0, start) + replacement + ta.value.substring(end);
  ta.selectionStart = start + 1;
  ta.selectionEnd = start + 1 + selText.length;
  ta.focus();
  autoSizeTextarea(ta);
}

// ── Toolbar actions ─────────────────────────────────────────────────────────
// Two modes:
// 1. Edit mode (textarea visible): operate on textarea selection directly.
// 2. Rendered mode (HTML visible): find selected text in block source, apply
//    formatting to source, send to backend, re-render.

function getActiveTextarea() {
  return blockEditor.querySelector(".md-block.editing .md-block-textarea");
}

// Apply inline formatting (bold, italic, code, strike) to current selection.
// Works in both rendered and edit modes.
async function applyInlineFormat(prefix, suffix) {
  const ta = getActiveTextarea();
  if (ta) {
    // Edit mode: operate on textarea selection.
    toggleWrap(ta, prefix, suffix);
    return;
  }
  // Rendered mode: find selection in rendered blocks.
  const sel = window.getSelection();
  if (!sel.rangeCount || sel.isCollapsed) {
    console.log("Select some text first.");
    return;
  }
  const selectedText = sel.toString().trim();
  if (!selectedText) return;

  // Find which block the selection is in.
  let blockEl = sel.anchorNode;
  while (blockEl && blockEl !== blockEditor) {
    if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
    blockEl = blockEl.parentNode;
  }
  if (!blockEl || blockEl === blockEditor) return;
  const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
  const block = syntaxBlocks[blockIdx];
  if (!block) return;

  // Find the selected text in the block's Markdown source.
  // The source has Markdown markers; rendered text has them stripped.
  // We search for the plain text in the source, skipping markers.
  const source = block.source;
  const result = findPlainTextInSource(source, selectedText);
  if (!result) {
    // Fallback: enter edit mode and let user format manually.
    enterEditMode(blockIdx);
    return;
  }

  // Check if text is already wrapped with this prefix/suffix.
  const before = source.substring(Math.max(0, result.start - prefix.length), result.start);
  const after = source.substring(result.end, result.end + suffix.length);
  let newSource;
  if (before === prefix && after === suffix) {
    // Unwrap: remove existing markers.
    newSource = source.substring(0, result.start - prefix.length) +
                source.substring(result.start, result.end) +
                source.substring(result.end + suffix.length);
  } else {
    // Wrap: add markers around the plain text.
    newSource = source.substring(0, result.start) +
                prefix + source.substring(result.start, result.end) + suffix +
                source.substring(result.end);
  }

  // Use replace_block with block index — backend handles byte spans correctly.
  await sendReplaceBlock(blockIdx, newSource);
}

// Apply line-prefix formatting (list, quote, heading) to current selection.
async function applyLineFormat(prefix) {
  const ta = getActiveTextarea();
  if (ta) {
    toggleLinePrefix(ta, prefix);
    return;
  }
  // Rendered mode: apply to the block's source.
  const sel = window.getSelection();
  let blockEl = null;
  if (sel.rangeCount > 0) {
    blockEl = sel.anchorNode;
    while (blockEl && blockEl !== blockEditor) {
      if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
      blockEl = blockEl.parentNode;
    }
  }
  if (!blockEl || blockEl === blockEditor) return;
  const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
  const block = syntaxBlocks[blockIdx];
  if (!block) return;

  const source = block.source;
  let newSource;
  if (source.startsWith(prefix)) {
    newSource = source.substring(prefix.length);
  } else {
    newSource = prefix + source;
  }
  // Use replace_block with block index — backend handles byte spans correctly.
  await sendReplaceBlock(blockIdx, newSource);
}

// Apply heading level to current block.
async function applyHeading(level) {
  const ta = getActiveTextarea();
  if (ta) {
    toggleHeadingInTextarea(ta, level);
    return;
  }
  // Rendered mode.
  const sel = window.getSelection();
  let blockEl = null;
  if (sel.rangeCount > 0) {
    blockEl = sel.anchorNode;
    while (blockEl && blockEl !== blockEditor) {
      if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
      blockEl = blockEl.parentNode;
    }
  }
  if (!blockEl || blockEl === blockEditor) return;
  const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
  const block = syntaxBlocks[blockIdx];
  if (!block) return;

  const source = block.source;
  const stripped = source.replace(/^#{1,6}\s+/, "").replace(/^={2,}\s*$\n?/m, "").replace(/^-{2,}\s*$\n?/m, "");
  const prefix = level > 0 ? "#".repeat(level) + " " : "";
  const newSource = prefix + stripped;
  await sendReplaceBlock(blockIdx, newSource);
}

// Insert a link around the current selection.
async function applyLink() {
  const ta = getActiveTextarea();
  if (ta) {
    insertLinkInTextarea(ta);
    return;
  }
  // Rendered mode.
  const sel = window.getSelection();
  if (!sel.rangeCount || sel.isCollapsed) {
    console.log("Select some text first.");
    return;
  }
  const selectedText = sel.toString().trim();
  if (!selectedText) return;
  const url = prompt("Enter URL:", "https://");
  if (!url) return;

  let blockEl = sel.anchorNode;
  while (blockEl && blockEl !== blockEditor) {
    if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
    blockEl = blockEl.parentNode;
  }
  if (!blockEl || blockEl === blockEditor) return;
  const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
  const block = syntaxBlocks[blockIdx];
  if (!block) return;

  const source = block.source;
  const result = findPlainTextInSource(source, selectedText);
  if (!result) return;

  const newSource = source.substring(0, result.start) +
    `[${source.substring(result.start, result.end)}](${url})` +
    source.substring(result.end);
  await sendReplaceBlock(blockIdx, newSource);
}

// Find a plain-text string in Markdown source, returning {start, end} byte offsets.
// Strips Markdown inline markers (**, *, _, ~~, `, etc.) from source to match.
function findPlainTextInSource(source, plainText) {
  // Build a map: for each position in the plain text, what position in the source it came from.
  // We strip markers character by character.
  const markers = ["**", "__", "*", "_", "~~", "`"];
  let i = 0; // position in source
  let plain = ""; // stripped text
  const sourcePos = []; // sourcePos[plainIdx] = sourceIdx

  while (i < source.length) {
    let matched = false;
    for (const m of markers) {
      if (source.substring(i, i + m.length) === m) {
        i += m.length; // skip marker
        matched = true;
        break;
      }
    }
    if (matched) continue;
    // Check for link/image syntax — skip the URL part.
    if (source[i] === "[" || source[i] === "]" || source[i] === "(" || source[i] === ")") {
      // For [text](url): skip ](url) but keep text
      if (source[i] === "]" && i + 1 < source.length && source[i+1] === "(") {
        // Skip ](url)
        i++; // skip ]
        if (source[i] === "(") {
          i++; // skip (
          while (i < source.length && source[i] !== ")") i++;
          if (i < source.length) i++; // skip )
        }
        continue;
      }
      if (source[i] === "[") { i++; continue; }
      if (source[i] === "!") { i++; continue; }
      // standalone ) or ] — skip
      i++;
      continue;
    }
    sourcePos.push(i);
    plain += source[i];
    i++;
  }

  // Find plainText in plain.
  const idx = plain.indexOf(plainText);
  if (idx < 0) return null;
  return {
    start: sourcePos[idx],
    end: sourcePos[idx + plainText.length - 1] + 1,
  };
}

// ── UI ──────────────────────────────────────────────────────────────────────

function updateUI(info) {
  fileNameEl.textContent = info.file_name;
  isDirty = info.is_dirty;
  updateDirtyState();
  blockCount.textContent = `${info.block_count} ${t("status.blocks")}`;
  cursorPos.textContent = `Ln 1, Col 1`;
}

function updateDirtyState() {
  if (isDirty) dirtyIndicator.classList.add("active");
  else dirtyIndicator.classList.remove("active");
}

// ── Tab bar with drag-and-drop ──────────────────────────────────────────────

function renderTabs() {
  tabList.innerHTML = "";
  for (const tab of openTabs) {
    const el = document.createElement("div");
    el.className = "tab" + (tab.id === activeTabId ? " active" : "");
    el.dataset.tabId = tab.id;
    el.draggable = true;

    const nameSpan = document.createElement("span");
    nameSpan.className = "tab-name";
    nameSpan.textContent = tab.file_name;
    el.appendChild(nameSpan);

    if (tab.is_dirty) {
      const dot = document.createElement("span");
      dot.className = "tab-dirty";
      el.appendChild(dot);
    }

    const closeBtn = document.createElement("span");
    closeBtn.className = "tab-close";
    closeBtn.textContent = "×";
    closeBtn.addEventListener("click", (e) => { e.stopPropagation(); closeTab(tab.id); });
    el.appendChild(closeBtn);

    el.addEventListener("click", () => switchTab(tab.id));
    el.addEventListener("mousedown", (e) => { if (e.button === 1) { e.preventDefault(); closeTab(tab.id); } });

    el.addEventListener("dragstart", (e) => {
      el.classList.add("dragging");
      e.dataTransfer.setData("text/plain", String(tab.id));
      e.dataTransfer.effectAllowed = "move";
      // Track drag start position for tear-off detection.
      el._dragStartInTabBar = true;
    });
    el.addEventListener("dragend", (e) => {
      el.classList.remove("dragging");
      // Check if the drop happened outside the tab bar (tear-off).
      const tabBar = document.getElementById("tab-bar");
      if (!tabBar) return;
      const barRect = tabBar.getBoundingClientRect();
      const outside = e.clientX < barRect.left || e.clientX > barRect.right ||
                      e.clientY < barRect.top || e.clientY > barRect.bottom;
      if (outside) {
        // Tear-off: open the dragged tab in a new window.
        const draggedId = parseInt(e.dataTransfer.getData("text/plain") || String(tab.id), 10);
        tearOffTab(draggedId);
      }
    });
    el.addEventListener("dragover", (e) => { e.preventDefault(); e.dataTransfer.dropEffect = "move"; el.classList.add("drag-over"); });
    el.addEventListener("dragleave", () => { el.classList.remove("drag-over"); });
    el.addEventListener("drop", (e) => {
      e.preventDefault(); el.classList.remove("drag-over");
      const draggedId = parseInt(e.dataTransfer.getData("text/plain"), 10);
      if (draggedId === tab.id) return;
      const fromIdx = openTabs.findIndex(t => t.id === draggedId);
      const toIdx = openTabs.findIndex(t => t.id === tab.id);
      if (fromIdx < 0 || toIdx < 0) return;
      const [moved] = openTabs.splice(fromIdx, 1);
      openTabs.splice(toIdx, 0, moved);
      renderTabs();
    });
    tabList.appendChild(el);
  }
}

// ── Git panel ───────────────────────────────────────────────────────────────

async function refreshGitAll() {
  await Promise.all([refreshGitStatus(), refreshGitBranches(), refreshGitLog(), refreshGitDiff(), refreshGitStash(), refreshGitTags(), refreshGitRemotes()]);
}

async function refreshGitStatus() {
  try { gitData.status = await tauriInvoke("get_git_status"); gitBranchInfo.textContent = gitData.status.branch ? `${t("git.branch")}: ${gitData.status.branch}` : ""; renderGitChanges(); }
  catch (e) { gitData.status = null; gitBranchInfo.textContent = ""; renderGitChanges(); }
}

async function refreshGitBranches() {
  try { gitData.branches = await tauriInvoke("git_branches"); renderGitBranches(); }
  catch (e) { gitData.branches = []; renderGitBranches(); }
}

async function refreshGitDiff() {
  try { gitData.diff = await tauriInvoke("git_diff"); renderGitDiff(); }
  catch (e) { gitData.diff = []; renderGitDiff(); }
}

async function refreshGitLog() {
  try { gitData.log = await tauriInvoke("git_log"); renderGitHistory(); }
  catch (e) { gitData.log = []; renderGitHistory(); }
}

function statusLetter(s) { return { Modified: "M", Added: "A", Deleted: "D", Renamed: "R", Untracked: "?", Conflicted: "C", Unmodified: " " }[s] || "?"; }

function renderGitChanges() {
  const s = gitData.status;
  if (!s) { gitChanges.innerHTML = `<div class="git-section-title">${t("git.no.repo")}</div>`; return; }
  let html = "";
  if (s.staged.length > 0) {
    html += `<div class="git-section-title">${t("git.stage")}</div>`;
    for (const f of s.staged) {
      html += renderFileDiffEntry(f, "staged");
    }
  }
  if (s.changes.length > 0) {
    html += `<div class="git-section-title">${t("git.changes")}</div>`;
    for (const f of s.changes) {
      html += renderFileDiffEntry(f, "changes");
    }
  }
  if (s.untracked.length > 0) {
    html += `<div class="git-section-title">${t("git.changes")}</div>`;
    for (const f of s.untracked) {
      html += renderFileDiffEntry(f, "untracked");
    }
  }
  if (s.staged.length > 0) {
    html += `<div class="git-section-title">${t("git.commit")}</div>`;
    html += '<input type="text" class="git-commit-input" id="commit-msg" placeholder="Commit message..." />';
    html += `<button class="git-action-btn" onclick="gitCommit()">${t("git.commit")}</button>`;
  }
  if (s.changes.length === 0 && s.staged.length === 0 && s.untracked.length === 0) {
    html += `<div style="padding:16px;color:var(--fg-muted);text-align:center">${t("git.no.changes")}</div>`;
  }
  gitChanges.innerHTML = html;
}

/// Render a file entry with expandable inline diff and right-click context menu.
function renderFileDiffEntry(f, section) {
  const path = escapeAttr(f.path);
  const safePath = escapeHtml(f.path);
  const letter = f.status === "Untracked" ? "?" : statusLetter(f.status);
  const cls = f.status === "Untracked" ? "Untracked" : f.status;
  return `<div class="git-file-diff-block" data-path="${path}" data-section="${section}" data-status="${f.status}">
    <div class="git-file-diff-header" onclick="gitToggleFileDiff(this)">
      <span class="chevron">▶</span>
      <span class="git-file-status git-status-${cls}">${letter}</span>
      <span class="git-file-path">${safePath}</span>
    </div>
    <div class="git-file-diff-inline hidden"></div>
  </div>`;
}

window.gitToggleFileDiff = async function(headerEl) {
  const block = headerEl.closest(".git-file-diff-block");
  const inlineDiv = block.querySelector(".git-file-diff-inline");
  const isExpanded = !inlineDiv.classList.contains("hidden");
  if (isExpanded) {
    inlineDiv.classList.add("hidden");
    headerEl.classList.remove("expanded");
    return;
  }
  headerEl.classList.add("expanded");
  inlineDiv.classList.remove("hidden");
  inlineDiv.innerHTML = `<div style="padding:8px;color:var(--fg-muted)">${t("git.loading.diff")}</div>`;
  const filePath = block.dataset.path;
  const status = block.dataset.status;
  try {
    if (status === "Untracked") {
      // Show entire file as new.
      const content = await tauriInvoke("git_read_file_at_revision", { filePath, revision: "" });
      inlineDiv.innerHTML = renderUntrackedDiff(content);
    } else {
      const diff = await tauriInvoke("git_diff_file", { filePath });
      inlineDiv.innerHTML = renderDiffHtml(diff);
    }
  } catch (e) {
    inlineDiv.innerHTML = `<div style="padding:8px;color:var(--diff-del)">${escapeHtml(String(e))}</div>`;
  }
};

function renderDiffHtml(diff) {
  if (!diff || !diff.hunks || diff.hunks.length === 0) {
    return '<div style="padding:8px;color:var(--fg-muted);text-align:center">No line-level changes</div>';
  }
  let html = "";
  for (const hunk of diff.hunks) {
    html += `<div class="diff-hunk-header">@@ -${hunk.old_start} +${hunk.new_start} @@</div>`;
    for (const line of hunk.lines) {
      const cls = line.kind === "insert" ? "insert" : line.kind === "delete" ? "delete" : "equal";
      const sign = line.kind === "insert" ? "+" : line.kind === "delete" ? "-" : " ";
      html += `<div class="diff-line ${cls}"><span class="diff-line-num">${line.old_no || ""}</span><span class="diff-line-num">${line.new_no || ""}</span><span class="diff-line-sign">${sign}</span><span class="diff-line-content">${escapeHtml(line.text)}</span></div>`;
    }
  }
  return html;
}

function renderUntrackedDiff(content) {
  if (!content) return '<div style="padding:8px;color:var(--fg-muted)">Empty file</div>';
  let html = "";
  let lineNo = 1;
  for (const line of content.split("\n")) {
    html += `<div class="diff-line insert"><span class="diff-line-num"></span><span class="diff-line-num">${lineNo}</span><span class="diff-line-sign">+</span><span class="diff-line-content">${escapeHtml(line)}</span></div>`;
    lineNo++;
  }
  return html;
}

function renderGitBranches() {
  if (!gitData.branches || gitData.branches.length === 0) { gitBranches.innerHTML = `<div style="padding:16px;color:var(--fg-muted);text-align:center">${t("git.no.branches")}</div>`; return; }
  let html = '<div class="git-section-title">Branches</div>';
  html += '<div class="git-inline-form"><input type="text" id="new-branch-name" placeholder="New branch name..." /><button class="git-action-btn" onclick="gitCreateBranch()">Create</button></div>';
  for (const b of gitData.branches) {
    const actions = b.is_current ? '' : `<div class="git-branch-actions"><button class="git-mini-btn" onclick="event.stopPropagation();gitCheckout('${escapeAttr(b.name)}')">${t("git.checkout")}</button><button class="git-mini-btn" onclick="event.stopPropagation();gitMergeBranch('${escapeAttr(b.name)}')">${t("git.merge")}</button><button class="git-mini-btn" onclick="event.stopPropagation();gitRebaseBranch('${escapeAttr(b.name)}')">${t("git.rebase")}</button><button class="git-mini-btn git-mini-danger" onclick="event.stopPropagation();gitDeleteBranch('${escapeAttr(b.name)}')">${t("git.delete.branch")}</button></div>`;
    html += `<div class="git-branch-entry ${b.is_current ? 'current' : ''}"><div class="git-branch-row" onclick="gitCheckout('${escapeAttr(b.name)}')"><span class="git-branch-icon">${b.is_current ? '●' : '○'}</span><span class="git-branch-name">${escapeHtml(b.name)}</span>${b.ahead > 0 ? `<span class="git-branch-ahead">↓${b.ahead}</span>` : ''}${b.behind > 0 ? `<span class="git-branch-behind">↑${b.behind}</span>` : ''}</div>${actions}</div>`;
  }
  gitBranches.innerHTML = html;
}

function renderGitHistory() {
  if (!gitData.log || gitData.log.length === 0) { gitHistory.innerHTML = `<div style="padding:16px;color:var(--fg-muted);text-align:center">${t("git.no.commits")}</div>`; return; }
  let html = '<div class="git-section-title">Commit History</div>';
  for (const c of gitData.log) {
    html += `<div class="git-commit-entry"><div class="git-commit-row"><div class="git-commit-sha">${escapeHtml(c.sha.substring(0, 8))}</div><div class="git-commit-msg">${escapeHtml(c.message)}</div><div class="git-commit-meta">${escapeHtml(c.author)} · ${escapeHtml(c.date)}</div></div><div class="git-commit-actions"><button class="git-mini-btn" onclick="event.stopPropagation();gitCherryPick('${escapeAttr(c.sha)}')">${t("git.cherry.pick")}</button><button class="git-mini-btn" onclick="event.stopPropagation();gitRevertCommit('${escapeAttr(c.sha)}')">${t("git.revert")}</button><button class="git-mini-btn" onclick="event.stopPropagation();gitResetSoft('${escapeAttr(c.sha)}')">${t("git.reset.soft")}</button><button class="git-mini-btn git-mini-danger" onclick="event.stopPropagation();gitResetHard('${escapeAttr(c.sha)}')">${t("git.reset.hard")}</button></div></div>`;
  }
  gitHistory.innerHTML = html;
}

function renderGitDiff() {
  // The Diff tab now shows the diff viewer (commit selectors).
  // This function is called by refreshGitAll but the actual rendering
  // happens via renderDiffViewer() when the tab is activated.
  if (!gitData.diff || gitData.diff.length === 0) {
    // Only overwrite if the diff viewer form hasn't been rendered yet.
    if (!document.getElementById("diff-mode-select")) {
      gitDiff.innerHTML = `<div style="padding:16px;color:var(--fg-muted);text-align:center">${t("git.no.changes")}</div>`;
    }
    return;
  }
  // If the diff viewer form is present, don't overwrite it.
  if (document.getElementById("diff-mode-select")) return;
  let html = "";
  for (const file of gitData.diff) {
    html += `<div class="diff-file-header">${escapeHtml(file.path)}</div>`;
    html += renderDiffHtml(file);
  }
  gitDiff.innerHTML = html;
}

// ── Git panel: Stash / Tags / Remotes / Diff viewer ────────────────────────

async function refreshGitStash() {
  try { gitData.stash = await tauriInvoke("git_stash_list"); renderGitStash(); }
  catch (e) { gitData.stash = []; renderGitStash(); }
}

async function refreshGitTags() {
  try { gitData.tags = await tauriInvoke("git_tags"); renderGitTags(); }
  catch (e) { gitData.tags = []; renderGitTags(); }
}

async function refreshGitRemotes() {
  try { gitData.remotes = await tauriInvoke("git_remotes"); renderGitRemotes(); }
  catch (e) { gitData.remotes = []; renderGitRemotes(); }
}

function renderGitStash() {
  let html = `<div class="git-section-title">${t("git.stash")}</div>`;
  html += '<button class="git-action-btn" onclick="gitStashPush()">Stash current changes</button>';
  const stash = gitData.stash || [];
  if (stash.length === 0) {
    html += '<div style="padding:16px;color:var(--fg-muted);text-align:center">No stashed changes</div>';
  } else {
    for (const s of stash) {
      html += `<div class="git-stash-entry"><div class="git-stash-info"><span class="git-stash-idx">stash@{${s.index}}</span><span class="git-stash-msg">${escapeHtml(s.message)}</span></div><div class="git-stash-actions"><button class="git-mini-btn" onclick="gitStashApply(${s.index})">${t("git.stash.apply")}</button><button class="git-mini-btn" onclick="gitStashPop(${s.index})">${t("git.stash.pop")}</button><button class="git-mini-btn git-mini-danger" onclick="gitStashDrop(${s.index})">${t("git.stash.drop")}</button></div></div>`;
    }
  }
  gitStash.innerHTML = html;
}

function renderGitTags() {
  let html = '<div class="git-section-title">Tags</div>';
  html += '<div class="git-inline-form"><input type="text" id="tag-name-input" placeholder="Tag name (e.g. v1.0)" /><input type="text" id="tag-msg-input" placeholder="Message (optional)" /><button class="git-action-btn" onclick="gitCreateTag()">Create</button></div>';
  const tags = gitData.tags || [];
  if (tags.length === 0) {
    html += '<div style="padding:16px;color:var(--fg-muted);text-align:center">No tags</div>';
  } else {
    for (const tag of tags) {
      html += `<div class="git-tag-entry"><span class="git-tag-icon">🏷</span><span class="git-tag-name">${escapeHtml(tag.name)}</span><span class="git-tag-target">${escapeHtml(tag.target)}</span>${tag.message ? `<span class="git-tag-msg">${escapeHtml(tag.message)}</span>` : ''}<button class="git-mini-btn git-mini-danger" onclick="gitDeleteTag('${escapeAttr(tag.name)}')">${t("git.delete.tag")}</button></div>`;
    }
  }
  gitTags.innerHTML = html;
}

function renderGitRemotes() {
  let html = '<div class="git-section-title">Remotes</div>';
  html += '<div class="git-inline-form"><input type="text" id="remote-name-input" placeholder="Remote name" /><input type="text" id="remote-url-input" placeholder="URL" /><button class="git-action-btn" onclick="gitAddRemote()">Add</button></div>';
  const remotes = gitData.remotes || [];
  if (remotes.length === 0) {
    html += '<div style="padding:16px;color:var(--fg-muted);text-align:center">No remotes</div>';
  } else {
    for (const r of remotes) {
      html += `<div class="git-remote-entry"><div class="git-remote-info"><span class="git-remote-name">${escapeHtml(r.name)}</span><span class="git-remote-url">${escapeHtml(r.fetch_url || r.url)}</span></div><div class="git-remote-actions"><button class="git-mini-btn" onclick="gitFetchRemote('${escapeAttr(r.name)}')">${t("git.fetch")}</button></div></div>`;
    }
  }
  html += '<div class="git-section-title" style="margin-top:12px">Push / Pull</div>';
  html += `<div class="git-inline-form"><input type="text" id="push-remote-input" placeholder="remote" /><input type="text" id="push-branch-input" placeholder="branch" /><button class="git-action-btn" onclick="gitPushToRemote(false)">${t("git.push")}</button><button class="git-action-btn git-mini-danger" onclick="gitPushToRemote(true)">Force</button><button class="git-action-btn" onclick="gitPullFromRemote()">${t("git.pull")}</button></div>`;
  gitRemotes.innerHTML = html;
}

// ── Git panel: Diff viewer (commit vs commit / working tree vs commit) ─────

function renderDiffViewer() {
  const log = gitData.log || [];
  let html = '<div class="git-section-title">Diff Viewer</div>';
  html += '<div class="git-diff-viewer-form">';
  html += '<select id="diff-mode-select"><option value="wt-vs-commit">Working tree vs commit</option><option value="commit-vs-commit">Commit vs commit</option></select>';
  // Commit dropdowns populated from log.
  const opts = log.map(c => `<option value="${escapeAttr(c.sha)}">${escapeHtml(c.sha.substring(0,8))} - ${escapeHtml(c.message.substring(0,40))}</option>`).join("");
  html += `<select id="diff-commit-a">${opts}</select>`;
  html += `<select id="diff-commit-b">${opts}</select>`;
  html += '<button class="git-action-btn" onclick="gitShowDiff()">Show diff</button>';
  html += '</div>';
  // File filter row.
  html += '<div class="git-diff-viewer-form">';
  html += '<label class="diff-filter-label"><input type="checkbox" id="diff-current-file-only" onchange="gitShowDiff()" /> Current file only</label>';
  html += '<input type="text" id="diff-file-filter" placeholder="Filter by path..." oninput="gitShowDiff()" style="flex:1;min-width:120px" />';
  html += `<span class="diff-current-file-name" id="diff-current-file-name"></span>`;
  html += '</div>';
  html += '<div id="diff-viewer-result"></div>';
  gitDiff.innerHTML = html;
  // Auto-show diff for current file if checkbox is checked (default: checked).
  // We pre-check the checkbox and auto-load on tab activation.
  const cb = document.getElementById("diff-current-file-only");
  if (cb) {
    cb.checked = diffFilterState.currentFileOnly;
    document.getElementById("diff-file-filter").value = diffFilterState.pathFilter;
  }
  // Update current file name display and auto-show.
  updateDiffCurrentFileName();
  // Auto-show diff on tab switch.
  if (diffFilterState.currentFileOnly || diffFilterState.pathFilter) {
    gitShowDiff();
  }
}

// State for diff filtering.
const diffFilterState = { currentFileOnly: true, pathFilter: "", currentFilePath: "" };

/// If the Diff tab is currently visible, update the current-file name and re-show diff.
async function syncDiffTabIfVisible() {
  const diffSection = document.getElementById("git-diff");
  if (!diffSection || diffSection.classList.contains("hidden")) return;
  // Diff tab is visible — update current file name and re-show diff.
  await updateDiffCurrentFileName();
  // Only auto-refresh if the form is already rendered.
  if (document.getElementById("diff-mode-select")) {
    gitShowDiff();
  }
}

/// Update the "current file" display and refresh diff if needed.
async function updateDiffCurrentFileName() {
  const span = document.getElementById("diff-current-file-name");
  if (!span) return;
  try {
    const path = await tauriInvoke("get_active_file_path");
    diffFilterState.currentFilePath = path || "";
    if (path) {
      // Show just the basename for readability.
      const base = path.split(/[\\/]/).pop() || path;
      span.textContent = "→ " + base;
      span.title = path;
    } else {
      span.textContent = "→ " + t("git.unsaved");
    }
  } catch {
    span.textContent = "";
  }
}

// ── Git actions (extended) ─────────────────────────────────────────────────

window.gitStashPush = async function() {
  const msg = prompt("Stash message (optional):");
  try { await tauriInvoke("git_stash_push", { message: msg || null }); await refreshGitAll(); }
  catch (e) { alert("Stash failed: " + e); }
};
window.gitStashPop = async function(index) { try { await tauriInvoke("git_stash_pop", { index }); await refreshGitAll(); } catch (e) { alert("Stash pop failed: " + e); } };
window.gitStashApply = async function(index) { try { await tauriInvoke("git_stash_apply", { index }); await refreshGitAll(); } catch (e) { alert("Stash apply failed: " + e); } };
window.gitStashDrop = async function(index) { if (!confirm(`Drop stash@{${index}}?`)) return; try { await tauriInvoke("git_stash_drop", { index }); await refreshGitAll(); } catch (e) { alert("Stash drop failed: " + e); } };

window.gitCreateBranch = async function() {
  const name = document.getElementById("new-branch-name")?.value?.trim();
  if (!name) { alert("Enter a branch name"); return; }
  try { await tauriInvoke("git_create_branch", { name }); await refreshGitAll(); }
  catch (e) { alert("Create branch failed: " + e); }
};
window.gitDeleteBranch = async function(name) { if (!confirm(`Delete branch "${name}"?`)) return; try { await tauriInvoke("git_delete_branch", { name, force: false }); await refreshGitAll(); } catch (e) { alert("Delete branch failed: " + e); } };
window.gitMergeBranch = async function(branch) {
  const strategy = prompt("Merge strategy: merge / ff-only / no-ff / squash", "merge");
  if (!strategy) return;
  try { await tauriInvoke("git_merge", { branch, strategy }); await refreshGitAll(); }
  catch (e) { alert("Merge failed: " + e); }
};
window.gitRebaseBranch = async function(branch) { if (!confirm(`Rebase onto "${branch}"?`)) return; try { await tauriInvoke("git_rebase", { branch }); await refreshGitAll(); } catch (e) { alert("Rebase failed: " + e); } };
window.gitCherryPick = async function(sha) { if (!confirm(`Cherry-pick ${sha.substring(0,8)}?`)) return; try { await tauriInvoke("git_cherry_pick", { commit: sha }); await refreshGitAll(); } catch (e) { alert("Cherry-pick failed: " + e); } };
window.gitRevertCommit = async function(sha) { if (!confirm(`Revert ${sha.substring(0,8)}?`)) return; try { await tauriInvoke("git_revert", { commit: sha }); await refreshGitAll(); } catch (e) { alert("Revert failed: " + e); } };
window.gitResetSoft = async function(sha) { if (!confirm(`Soft reset to ${sha.substring(0,8)}?`)) return; try { await tauriInvoke("git_reset_soft", { commit: sha }); await refreshGitAll(); } catch (e) { alert("Reset failed: " + e); } };
window.gitResetHard = async function(sha) { if (!confirm(`⚠ HARD reset to ${sha.substring(0,8)}? This discards all uncommitted changes!`)) return; try { await tauriInvoke("git_reset_hard", { commit: sha }); await refreshGitAll(); } catch (e) { alert("Reset failed: " + e); } };

window.gitCreateTag = async function() {
  const name = document.getElementById("tag-name-input")?.value?.trim();
  if (!name) { alert("Enter a tag name"); return; }
  const msg = document.getElementById("tag-msg-input")?.value?.trim() || null;
  try { await tauriInvoke("git_create_tag", { name, message: msg }); await refreshGitAll(); }
  catch (e) { alert("Create tag failed: " + e); }
};
window.gitDeleteTag = async function(name) { if (!confirm(`Delete tag "${name}"?`)) return; try { await tauriInvoke("git_delete_tag", { name }); await refreshGitAll(); } catch (e) { alert("Delete tag failed: " + e); } };

window.gitAddRemote = async function() {
  const name = document.getElementById("remote-name-input")?.value?.trim();
  const url = document.getElementById("remote-url-input")?.value?.trim();
  if (!name || !url) { alert("Enter remote name and URL"); return; }
  try { await tauriInvoke("git_add_remote", { name, url }); await refreshGitAll(); }
  catch (e) { alert("Add remote failed: " + e); }
};
window.gitFetchRemote = async function(remote) { try { await tauriInvoke("git_fetch_remote", { remote }); await refreshGitAll(); } catch (e) { alert("Fetch failed: " + e); } };
window.gitPushToRemote = async function(force) {
  const remote = document.getElementById("push-remote-input")?.value?.trim();
  const branch = document.getElementById("push-branch-input")?.value?.trim();
  if (!remote || !branch) { alert("Enter remote and branch"); return; }
  if (force && !confirm(`⚠ Force push to ${remote}/${branch}?`)) return;
  try { await tauriInvoke("git_push_to_remote", { remote, branch, force }); await refreshGitAll(); }
  catch (e) { alert("Push failed: " + e); }
};
window.gitPullFromRemote = async function() {
  const remote = document.getElementById("push-remote-input")?.value?.trim();
  const branch = document.getElementById("push-branch-input")?.value?.trim();
  if (!remote || !branch) { alert("Enter remote and branch"); return; }
  try { await tauriInvoke("git_pull_from_remote", { remote, branch }); await refreshGitAll(); }
  catch (e) { alert("Pull failed: " + e); }
};

window.gitShowDiff = async function() {
  const modeSelect = document.getElementById("diff-mode-select");
  const mode = modeSelect ? modeSelect.value : "working";
  const resultDiv = document.getElementById("diff-viewer-result");
  if (!resultDiv) return;
  // Read filter state from UI.
  const cb = document.getElementById("diff-current-file-only");
  const filterInput = document.getElementById("diff-file-filter");
  diffFilterState.currentFileOnly = cb ? cb.checked : false;
  diffFilterState.pathFilter = filterInput ? filterInput.value.trim() : "";

  // If "current file only" is checked, we need the active file's relative path.
  let currentRelPath = null;
  if (diffFilterState.currentFileOnly) {
    currentRelPath = await resolveCurrentFileRelativePath();
    if (!currentRelPath) {
      resultDiv.innerHTML = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No active file or file is not in the repository</div>';
      return;
    }
  }

  resultDiv.innerHTML = `<div style="padding:8px;color:var(--fg-muted)">${t("git.loading.diff")}</div>`;
  try {
    let diffs;
    if (mode === "wt-vs-commit") {
      const commitEl = document.getElementById("diff-commit-a");
      const commit = commitEl ? commitEl.value : "";
      diffs = await tauriInvoke("git_diff_vs_commit", { commit });
    } else {
      const aEl = document.getElementById("diff-commit-a");
      const bEl = document.getElementById("diff-commit-b");
      const a = aEl ? aEl.value : "";
      const b = bEl ? bEl.value : "";
      diffs = await tauriInvoke("git_diff_commits", { commitA: a, commitB: b });
    }

    // Apply filters.
    let filtered = diffs || [];
    if (diffFilterState.currentFileOnly && currentRelPath) {
      filtered = filtered.filter(f => f.path === currentRelPath || f.path.endsWith("/" + currentRelPath) || f.path === currentRelPath.replace(/\\/g, "/"));
    }
    if (diffFilterState.pathFilter) {
      const q = diffFilterState.pathFilter.toLowerCase();
      filtered = filtered.filter(f => f.path.toLowerCase().includes(q));
    }

    let html = "";
    if (filtered.length === 0) {
      html = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No differences' +
        (diffFilterState.currentFileOnly ? ' for current file' : '') + '</div>';
    } else {
      for (const file of filtered) {
        html += `<div class="diff-file-header">${escapeHtml(file.path)}</div>`;
        // Fetch line-level diff for this specific file.
        let fileDiff;
        if (mode === "wt-vs-commit") {
          const commitEl = document.getElementById("diff-commit-a");
          const commit = commitEl ? commitEl.value : "";
          fileDiff = await tauriInvoke("git_diff_file_vs_commit", { filePath: file.path, commit });
        } else {
          const aEl = document.getElementById("diff-commit-a");
          const bEl = document.getElementById("diff-commit-b");
          const a = aEl ? aEl.value : "";
          const b = bEl ? bEl.value : "";
          fileDiff = await tauriInvoke("git_diff_file_commits", { filePath: file.path, commitA: a, commitB: b });
        }
        html += renderDiffHtml(fileDiff);
      }
    }
    resultDiv.innerHTML = html;
  } catch (e) { resultDiv.innerHTML = `<div style="padding:8px;color:#f44">Diff error: ${escapeHtml(String(e))}</div>`; }
};

/// Resolve the active file's path relative to the git repo root.
async function resolveCurrentFileRelativePath() {
  try {
    const absPath = await tauriInvoke("get_active_file_path");
    if (!absPath) return null;
    // Get the git repo root.
    const root = await tauriInvoke("git_repo_root");
    if (!root) return null;
    // Compute relative path.
    const norm = (p) => p.replace(/\\/g, "/").toLowerCase();
    const abs = norm(absPath);
    const r = norm(root);
    let rel;
    if (abs.startsWith(r + "/")) {
      rel = absPath.substring(r.length + 1);
    } else if (abs === r) {
      rel = "";
    } else {
      // Not in repo.
      return null;
    }
    // Keep original separators from absPath for the relative part.
    return rel.replace(/\\/g, "/");
  } catch {
    return null;
  }
}

window.gitStageFile = async function(path) { try { await tauriInvoke("git_stage_file", { filePath: path }); await refreshGitAll(); } catch (e) { alert("Stage failed: " + e); } };
window.gitUnstageFile = async function(path) { try { await tauriInvoke("git_unstage_file", { filePath: path }); await refreshGitAll(); } catch (e) { alert("Unstage failed: " + e); } };
window.gitCommit = async function() { const msg = document.getElementById("commit-msg")?.value?.trim(); if (!msg) { alert("Enter a commit message"); return; } try { await tauriInvoke("git_commit", { message: msg }); await refreshGitAll(); } catch (e) { alert("Commit failed: " + e); } };
window.gitCheckout = async function(branch) { if (!confirm(`Checkout branch "${branch}"?`)) return; try { await tauriInvoke("git_checkout", { branch }); await refreshGitAll(); } catch (e) { alert("Checkout failed: " + e); } };

// ── Context menu actions for files in Changes ──────────────────────────────
window.gitDiscardFile = async function(path) {
  if (!confirm(`Discard changes to "${path}"? This cannot be undone.`)) return;
  try { await tauriInvoke("git_discard_file", { filePath: path }); await refreshGitAll(); }
  catch (e) { alert("Discard failed: " + e); }
};
window.gitRemoveUntracked = async function(path) {
  if (!confirm(`Delete untracked file "${path}"? This cannot be undone.`)) return;
  try { await tauriInvoke("git_remove_untracked", { filePath: path }); await refreshGitAll(); }
  catch (e) { alert("Remove failed: " + e); }
};

// Right-click context menu on file entries in Changes.
gitChanges.addEventListener("contextmenu", (e) => {
  const block = e.target.closest(".git-file-diff-block");
  if (!block) return;
  e.preventDefault();
  const path = block.dataset.path;
  const section = block.dataset.section;
  const status = block.dataset.status;
  const items = [];
  // View diff (expand inline).
  items.push({ label: t("git.diff"), action: "view-diff" });
  items.push({ separator: true });
  if (section === "staged") {
    items.push({ label: t("git.unstage"), action: "unstage" });
  } else if (section === "changes") {
    items.push({ label: t("git.stage"), action: "stage" });
    items.push({ separator: true });
    items.push({ label: t("git.reset.hard"), action: "discard", danger: true });
  } else if (section === "untracked") {
    items.push({ label: t("git.stage"), action: "stage" });
    items.push({ separator: true });
    items.push({ label: t("fc.delete"), action: "delete", danger: true });
  }
  contextMenuHandlers["view-diff"] = () => {
    const header = block.querySelector(".git-file-diff-header");
    if (header.classList.contains("expanded")) return;
    window.gitToggleFileDiff(header);
  };
  contextMenuHandlers["stage"] = () => window.gitStageFile(path);
  contextMenuHandlers["unstage"] = () => window.gitUnstageFile(path);
  contextMenuHandlers["discard"] = () => window.gitDiscardFile(path);
  contextMenuHandlers["delete"] = () => window.gitRemoveUntracked(path);
  showContextMenu(e.clientX, e.clientY, items);
});

document.querySelectorAll(".git-tab").forEach(btn => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".git-tab").forEach(b => b.classList.remove("active"));
    btn.classList.add("active");
    const tabName = btn.dataset.tab;
    document.querySelectorAll(".git-section").forEach(s => s.classList.add("hidden"));
    document.getElementById("git-" + tabName).classList.remove("hidden");
    // Diff tab shows the diff viewer (commit vs commit).
    if (tabName === "diff") {
      renderDiffViewer();
      // Auto-show diff for current file (sync with active editor tab).
      // renderDiffViewer already auto-shows if currentFileOnly is checked.
    }
  });
});

// ── Event handlers ──────────────────────────────────────────────────────────

btnNew.addEventListener("click", () => newDocument());

btnOpen.addEventListener("click", async () => {
  const result = await openFileDialog();
  if (!result) return;
  if (typeof result === "string") { await openDocument(result); }
  else if (result && result.path) {
    if (result.content) {
      await newDocument();
      const text = new TextDecoder().decode(result.content);
      await sendReplace(0, currentText.length, text);
      fileNameEl.textContent = result.path;
    } else { await openDocument(result.path); }
  }
});

btnSave.addEventListener("click", async () => {
  flushEdits();
  const tab = openTabs.find(t => t.id === activeTabId);
  if (tab && tab.file_name !== "untitled.md" && tab.file_name !== "untitled") { await saveDocument(); }
  else { const path = await saveFileDialog(); if (path) await saveDocument(path); }
});

btnUndo.addEventListener("click", () => doUndo());
btnRedo.addEventListener("click", () => doRedo());

// Prevent toolbar buttons from causing textarea blur (which would exit edit mode).
// mousedown fires before blur; we set a flag to suppress the blur handler.
const toolbarButtons = [btnBold, btnItalic, btnStrike, btnCode, btnLink, btnUl, btnOl, btnQuote, btnHr, btnCodeblock, btnTask, selHeading];
toolbarButtons.forEach(btn => {
  btn.addEventListener("mousedown", (e) => {
    if (getActiveTextarea()) {
      suppressBlur = true;
      e.preventDefault(); // prevent focus leaving the textarea
    }
  });
});

btnBold.addEventListener("click", () => applyInlineFormat("**", "**"));
btnItalic.addEventListener("click", () => applyInlineFormat("*", "*"));
btnStrike.addEventListener("click", () => applyInlineFormat("~~", "~~"));
btnCode.addEventListener("click", () => applyInlineFormat("`", "`"));
btnLink.addEventListener("click", () => applyLink());
btnUl.addEventListener("click", () => applyLineFormat("- "));
btnOl.addEventListener("click", () => applyLineFormat("1. "));
btnQuote.addEventListener("click", () => applyLineFormat("> "));
btnTask.addEventListener("click", () => applyLineFormat("- [ ] "));

btnHr.addEventListener("click", async () => {
  const ta = getActiveTextarea();
  if (ta) {
    const start = ta.selectionStart;
    const ls = ta.value.lastIndexOf("\n", start - 1) + 1;
    ta.value = ta.value.substring(0, ls) + "---\n" + ta.value.substring(ls);
    ta.selectionStart = ls + 4; ta.selectionEnd = ls + 4;
    autoSizeTextarea(ta);
  } else {
    // Insert HR as a new block after the current one.
    const sel = window.getSelection();
    let blockEl = null;
    if (sel.rangeCount > 0) {
      blockEl = sel.anchorNode;
      while (blockEl && blockEl !== blockEditor) {
        if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
        blockEl = blockEl.parentNode;
      }
    }
    if (!blockEl || blockEl === blockEditor) return;
    const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
    const block = syntaxBlocks[blockIdx];
    if (!block) return;
    // Insert after this block's source in currentText.
    const pos = currentText.indexOf(block.source);
    const insertPos = pos >= 0 ? pos + block.source.length : currentText.length;
    const newText = currentText.substring(0, insertPos) + "\n---\n" + currentText.substring(insertPos);
    await sendReplace(0, currentText.length, newText);
  }
});

btnCodeblock.addEventListener("click", async () => {
  const ta = getActiveTextarea();
  if (ta) {
    const lang = prompt("Language (optional):", "");
    const start = ta.selectionStart;
    const end = ta.selectionEnd;
    const sel = ta.value.substring(start, end) || "// code";
    ta.value = ta.value.substring(0, start) + "```" + lang + "\n" + sel + "\n```\n" + ta.value.substring(end);
    ta.selectionStart = start; ta.selectionEnd = start + 6 + lang.length + sel.length + 4;
    autoSizeTextarea(ta);
  } else {
    // Insert a code block after the current block.
    const sel = window.getSelection();
    let blockEl = null;
    if (sel.rangeCount > 0) {
      blockEl = sel.anchorNode;
      while (blockEl && blockEl !== blockEditor) {
        if (blockEl.dataset && blockEl.dataset.blockIndex !== undefined) break;
        blockEl = blockEl.parentNode;
      }
    }
    if (!blockEl || blockEl === blockEditor) return;
    const blockIdx = parseInt(blockEl.dataset.blockIndex, 10);
    const block = syntaxBlocks[blockIdx];
    if (!block) return;
    const lang = prompt("Language (optional):", "");
    const pos = currentText.indexOf(block.source);
    const insertPos = pos >= 0 ? pos + block.source.length : currentText.length;
    const newText = currentText.substring(0, insertPos) + "\n```" + lang + "\n// code here\n```\n" + currentText.substring(insertPos);
    await sendReplace(0, currentText.length, newText);
  }
});

btnGit.addEventListener("click", () => {
  gitPanelVisible = !gitPanelVisible;
  gitPanel.classList.toggle("hidden", !gitPanelVisible);
  gitDivider.classList.toggle("hidden", !gitPanelVisible);
  btnGit.classList.toggle("active", gitPanelVisible);
  if (gitPanelVisible) refreshGitAll();
});

// ── View toggle: rendered Markdown ↔ raw source ────────────────────────────
function toggleViewMode() {
  if (editingBlockIndex >= 0) exitEditMode();
  viewMode = viewMode === "rendered" ? "source" : "rendered";
  btnViewToggle.classList.toggle("active", viewMode === "source");
  renderBlocks();
}

btnViewToggle.addEventListener("click", toggleViewMode);

// ── Resizable divider between editor and git panel ─────────────────────────
(function setupGitDivider() {
  let dragging = false;
  let startX = 0, startWidth = 0;
  gitDivider.addEventListener("mousedown", (e) => {
    dragging = true;
    startX = e.clientX;
    startWidth = gitPanel.offsetWidth;
    gitDivider.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    // Git panel is on the right, so dragging left increases width.
    const delta = startX - e.clientX;
    let newWidth = startWidth + delta;
    const maxWidth = window.innerWidth * 0.8;
    const minWidth = 250;
    if (newWidth > maxWidth) newWidth = maxWidth;
    if (newWidth < minWidth) newWidth = minWidth;
    gitPanel.style.width = newWidth + "px";
  });
  document.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false;
    gitDivider.classList.remove("dragging");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  });
})();

// ── Context menu ───────────────────────────────────────────────────────────
function showContextMenu(x, y, items) {
  let html = "";
  for (const item of items) {
    if (item.separator) { html += '<div class="ctx-menu-separator"></div>'; continue; }
    const cls = item.danger ? "ctx-menu-item danger" : "ctx-menu-item";
    html += `<div class="${cls}" data-action="${escapeAttr(item.action)}">${escapeHtml(item.label)}</div>`;
  }
  contextMenu.innerHTML = html;
  contextMenu.classList.remove("hidden");
  // Position — keep within viewport.
  const rect = contextMenu.getBoundingClientRect();
  const maxX = window.innerWidth - rect.width - 4;
  const maxY = window.innerHeight - rect.height - 4;
  contextMenu.style.left = Math.min(x, maxX) + "px";
  contextMenu.style.top = Math.min(y, maxY) + "px";
  // Wire up clicks.
  contextMenu.querySelectorAll(".ctx-menu-item").forEach(el => {
    el.addEventListener("click", () => {
      const action = el.dataset.action;
      contextMenu.classList.add("hidden");
      const handler = contextMenuHandlers[action];
      if (handler) handler();
    });
  });
}
function hideContextMenu() { contextMenu.classList.add("hidden"); }
document.addEventListener("click", () => hideContextMenu());
document.addEventListener("keydown", (e) => { if (e.key === "Escape") hideContextMenu(); });
const contextMenuHandlers = {};

selHeading.addEventListener("change", () => {
  const level = parseInt(selHeading.value, 10);
  selHeading.value = "0";
  applyHeading(level);
});

tabNewBtn.addEventListener("click", () => newDocument());

// Handle link clicks in rendered Markdown.
// - http/https links: ask confirmation, open in default browser via opener plugin.
// - .md/.markdown links: open in a new tab in the editor.
// - Other links: prevent default navigation.
blockEditor.addEventListener("click", (e) => {
  const link = e.target.closest("a");
  if (!link) return;
  e.preventDefault();
  e.stopPropagation();
  const href = link.getAttribute("href") || "";
  if (!href) return;

  // External links (http, https, mailto, etc.)
  if (/^(https?:|mailto:|ftp:|tel:)/i.test(href)) {
    // Ask user for confirmation before opening external link.
    const confirmed = confirm(t("msg.external.link", { url: href }));
    if (!confirmed) return;
    // Use tauri-plugin-opener to open in default browser.
    const tauri = window.__TAURI__;
    if (tauri?.opener?.openUrl) {
      tauri.opener.openUrl(href).catch(err => console.error("opener failed:", err));
    } else {
      // Fallback: try direct invoke.
      tauriInvoke("plugin:opener|open_url", { url: href }).catch(err => {
        console.error("open_url invoke failed:", err);
        // Last resort: window.open (may be blocked by webview).
        window.open(href, "_blank");
      });
    }
    return;
  }

  // Markdown file links — open in new tab.
  if (/\.md$|\.markdown$/i.test(href)) {
    openRelativeFile(href);
    return;
  }

  // Anchor links (#section) — scroll to heading.
  if (href.startsWith("#")) {
    const target = blockEditor.querySelector(`[data-block-index]`);
    // For now, just ignore anchor links.
    return;
  }

  // Unknown link type — log and ignore.
  console.log("Unhandled link:", href);
});

// Open a relative .md file in a new tab.
async function openRelativeFile(relativePath) {
  try {
    const info = await tauriInvoke("open_relative_file", { relativePath });
    // If this file is already open in another tab, switch to it instead of creating a duplicate.
    const existing = openTabs.find(tab => tab.file_name === info.file_name);
    if (existing && existing.id !== info.tab_id) {
      // The backend already created a new tab — close it and switch to the existing one.
      tauriInvoke("close_tab", { tabId: info.tab_id });
      await switchTab(existing.id);
      return;
    }
    currentText = info.text || "";
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
  } catch (e) {
    alert(t("msg.open.fail", { path: relativePath, err: e }));
  }
}

// Click outside blocks exits edit mode.
blockEditor.addEventListener("click", (e) => {
  if (e.target === blockEditor || e.target.classList.contains("empty-block")) {
    if (editingBlockIndex >= 0) exitEditMode();
  }
});

// Keyboard shortcuts.
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey || e.metaKey) {
    // Ctrl+Shift+M — toggle Markdown source view.
    if (e.shiftKey && (e.key === "M" || e.key === "m")) {
      e.preventDefault();
      toggleViewMode();
      return;
    }
    // Ctrl+Shift+E — toggle file tree sidebar.
    if (e.shiftKey && (e.key === "E" || e.key === "e")) {
      e.preventDefault();
      toggleFileTree();
      return;
    }
    switch (e.key) {
      case "n": e.preventDefault(); newDocument(); break;
      case "o": e.preventDefault(); btnOpen.click(); break;
      case "s": e.preventDefault(); btnSave.click(); break;
      case "z": e.preventDefault(); doUndo(); break;
      case "y": e.preventDefault(); doRedo(); break;
      case "b": e.preventDefault(); applyInlineFormat("**", "**"); break;
      case "i": e.preventDefault(); applyInlineFormat("*", "*"); break;
      case "k": e.preventDefault(); applyLink(); break;
      case "Tab":
        if (openTabs.length < 2) return;
        e.preventDefault();
        const idx = openTabs.findIndex(t => t.id === activeTabId);
        const next = e.shiftKey ? (idx - 1 + openTabs.length) % openTabs.length : (idx + 1) % openTabs.length;
        switchTab(openTabs[next].id);
        break;
    }
  }
  // PgUp/PgDown — ensure chunk loading triggers after fast scroll.
  if (virtualizedMode && (e.key === "PageUp" || e.key === "PageDown")) {
    // Let the browser handle the scroll, then check for chunk loading.
    setTimeout(() => {
      if (scrollRenderPending) return;
      updateVisibleRange();
      const parsedHeight = cumulativeHeight(syntaxBlocks.length);
      const scrolledBeyondParsed = blockEditor.scrollTop > parsedHeight * 0.9;
      if (scrolledBeyondParsed || visibleRange.end >= syntaxBlocks.length - 20) {
        renderVirtualizedBlocks();
        maybeParseNextChunk();
      }
    }, 50);
  }
});

// ── File tree sidebar ──────────────────────────────────────────────────────

/// Toggle file tree sidebar visibility.
function toggleFileTree() {
  const visible = !fileTreePanel.classList.contains("hidden");
  if (visible) {
    fileTreePanel.classList.add("hidden");
    fileTreeDivider.classList.add("hidden");
    btnTreeToggle.classList.remove("active");
  } else {
    fileTreePanel.classList.remove("hidden");
    fileTreeDivider.classList.remove("hidden");
    btnTreeToggle.classList.add("active");
    // If no root set, try to use the active file's directory.
    if (!fileTreeRoot) {
      const activeTab = openTabs.find(t => t.id === activeTabId);
      if (activeTab && activeTab.file_path) {
        const path = activeTab.file_path;
        const dir = parentDir(path);
        if (dir) setFileTreeRoot(dir);
      }
    }
    if (fileTreeRoot) refreshFileTree();
  }
}

btnTreeToggle.addEventListener("click", toggleFileTree);
document.getElementById("btn-tree-close").addEventListener("click", toggleFileTree);

/// Set the file tree root directory.
function setFileTreeRoot(dirPath) {
  fileTreeRoot = dirPath;
  fileTreeExpanded.clear();
  fileTreeExpanded.add(dirPath);
  const name = dirPath.split(/[\\/]/).filter(Boolean).pop() || dirPath;
  fileTreeRootName.textContent = name;
  fileTreeRootName.title = dirPath;
  refreshFileTree();
}

/// Open folder dialog to set root.
document.getElementById("btn-tree-open-folder").addEventListener("click", async () => {
  try {
    const tauri = window.__TAURI__;
    if (tauri?.dialog?.open) {
      const selected = await tauri.dialog.open({ directory: true });
      if (selected) setFileTreeRoot(selected);
    }
  } catch (e) { alert(t("msg.open.fail", { path: "", err: e })); }
});

/// Go to parent directory.
document.getElementById("btn-tree-up").addEventListener("click", async () => {
  if (!fileTreeRoot) return;
  try {
    const parent = await tauriInvoke("get_parent_dir", { dirPath: fileTreeRoot });
    if (parent) setFileTreeRoot(parent);
  } catch (e) { /* at root, ignore */ }
});

/// Refresh the file tree content.
async function refreshFileTree() {
  if (!fileTreeRoot) {
    fileTreeContent.innerHTML = `<div class="file-tree-empty">${t("ft.empty")}</div>`;
    return;
  }
  const filter = fileTreeFilter.value.trim().toLowerCase();
  fileTreeContent.innerHTML = `<div class="file-tree-empty">${t("ft.loading")}</div>`;
  try {
    await renderFileTreeLevel(fileTreeRoot, fileTreeContent, 0, filter);
    if (fileTreeContent.children.length === 0) {
      fileTreeContent.innerHTML = `<div class="file-tree-empty">${t("ft.no.files")}</div>`;
    }
  } catch (e) {
    fileTreeContent.innerHTML = `<div class="file-tree-empty">Error: ${escapeHtml(String(e))}</div>`;
  }
}

/// Render a single level of the file tree.
async function renderFileTreeLevel(dirPath, container, depth, filter) {
  let entries;
  try {
    entries = await tauriInvoke("list_directory", { dirPath });
  } catch (e) {
    container.innerHTML = `<div class="file-tree-empty">Error: ${escapeHtml(String(e))}</div>`;
    return;
  }
  container.innerHTML = "";
  for (const entry of entries) {
    if (filter && !entry.name.toLowerCase().includes(filter) && !entry.is_dir) continue;
    const item = document.createElement("div");
    item.className = "file-tree-item " + (entry.is_dir ? "folder" : "file");
    item.dataset.path = entry.path;
    item.dataset.isDir = entry.is_dir;
    item.style.paddingLeft = (8 + depth * 16) + "px";

    const chevron = entry.is_dir ? '<span class="ft-chevron">▶</span>' : '<span class="ft-chevron"></span>';
    const icon = entry.is_dir ? "📁" : getFileIcon(entry.name);
    item.innerHTML = `${chevron}<span class="ft-icon">${icon}</span><span class="ft-name">${escapeHtml(entry.name)}</span>`;

    if (entry.is_dir) {
      const expanded = fileTreeExpanded.has(entry.path);
      if (expanded) item.classList.add("expanded");
      item.addEventListener("click", (e) => {
        e.stopPropagation();
        toggleFolder(entry.path, item, depth, filter);
      });
      item.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        e.stopPropagation();
        showFileContextMenu(e.clientX, e.clientY, entry);
      });
      // Auto-expand if in expanded set.
      if (expanded) {
        const childContainer = document.createElement("div");
        childContainer.className = "file-tree-children";
        container.appendChild(item);
        container.appendChild(childContainer);
        renderFileTreeLevel(entry.path, childContainer, depth + 1, filter);
        continue;
      }
    } else {
      item.addEventListener("click", (e) => {
        e.stopPropagation();
        openFileFromTree(entry.path);
      });
      item.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        e.stopPropagation();
        showFileContextMenu(e.clientX, e.clientY, entry);
      });
    }
    container.appendChild(item);
  }
}

/// Toggle folder expansion.
async function toggleFolder(dirPath, itemEl, depth, filter) {
  const expanded = fileTreeExpanded.has(dirPath);
  if (expanded) {
    fileTreeExpanded.delete(dirPath);
    itemEl.classList.remove("expanded");
    // Remove children container that follows this item.
    let next = itemEl.nextElementSibling;
    while (next && next.classList.contains("file-tree-children")) {
      const toRemove = next;
      next = next.nextElementSibling;
      toRemove.remove();
    }
  } else {
    fileTreeExpanded.add(dirPath);
    itemEl.classList.add("expanded");
    const childContainer = document.createElement("div");
    childContainer.className = "file-tree-children";
    itemEl.after(childContainer);
    await renderFileTreeLevel(dirPath, childContainer, depth + 1, filter);
  }
}

/// Get file icon based on extension.
function getFileIcon(name) {
  const ext = name.split(".").pop()?.toLowerCase();
  if (ext === "md" || ext === "markdown") return "📝";
  if (ext === "rs") return "🦀";
  if (ext === "js" || ext === "ts" || ext === "jsx" || ext === "tsx") return "📜";
  if (ext === "json" || ext === "toml" || ext === "yaml" || ext === "yml") return "⚙";
  if (ext === "html" || ext === "css") return "🎨";
  if (ext === "png" || ext === "jpg" || ext === "jpeg" || ext === "gif" || ext === "svg") return "🖼";
  return "📄";
}

/// Open a file from the tree.
async function openFileFromTree(filePath) {
  try {
    // If this file is already open in a tab, switch to it instead of creating a duplicate.
    const existing = openTabs.find(tab => tab.file_path === filePath);
    if (existing) {
      await switchTab(existing.id);
      return;
    }
    const info = await tauriInvoke("open_document", { path: filePath });
    currentText = info.text || "";
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
  } catch (e) {
    alert(t("msg.open.fail", { path: filePath, err: e }));
  }
}

/// Set selected folder as root.
window.setAsRoot = function(dirPath) {
  setFileTreeRoot(dirPath);
};

/// Show the file context menu.
function showFileContextMenu(x, y, entry) {
  fileContextMenu.innerHTML = "";
  const items = [
    { label: t("fc.open"), action: () => { if (entry.is_dir) setFileTreeRoot(entry.path); else openFileFromTree(entry.path); } },
    ...(entry.is_dir ? [{ label: t("fc.set.root"), action: () => setFileTreeRoot(entry.path) }] : []),
    { separator: true },
    { label: t("fc.copy"), action: () => { fileTreeClipboard = { path: entry.path, isDir: entry.isDir, operation: "copy" }; } },
    { label: t("fc.cut"), action: () => { fileTreeClipboard = { path: entry.path, isDir: entry.isDir, operation: "cut" }; } },
    { separator: true },
    { label: t("fc.rename"), action: () => renameFileEntry(entry) },
    { label: t("fc.delete"), danger: true, action: () => deleteFileEntry(entry) },
  ];
  for (const item of items) {
    if (item.separator) {
      const sep = document.createElement("div");
      sep.className = "fc-menu-separator";
      fileContextMenu.appendChild(sep);
      continue;
    }
    const el = document.createElement("div");
    el.className = "fc-menu-item" + (item.danger ? " danger" : "");
    el.textContent = item.label;
    el.addEventListener("click", () => {
      fileContextMenu.classList.add("hidden");
      item.action();
    });
    fileContextMenu.appendChild(el);
  }
  // Also add "Paste" if clipboard has something.
  if (fileTreeClipboard) {
    const sep = document.createElement("div");
    sep.className = "fc-menu-separator";
    fileContextMenu.appendChild(sep);
    const pasteEl = document.createElement("div");
    pasteEl.className = "fc-menu-item";
    pasteEl.textContent = `${t("fc.paste")} (${fileTreeClipboard.operation})`;
    pasteEl.addEventListener("click", () => {
      fileContextMenu.classList.add("hidden");
      pasteFileEntry(entry);
    });
    fileContextMenu.appendChild(pasteEl);
  }
  // Clamp to viewport so the menu doesn't go off-screen.
  fileContextMenu.classList.remove("hidden");
  const menuRect = fileContextMenu.getBoundingClientRect();
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const clampedX = Math.min(x, vw - menuRect.width - 4);
  const clampedY = Math.min(y, vh - menuRect.height - 4);
  fileContextMenu.style.left = Math.max(0, clampedX) + "px";
  fileContextMenu.style.top = Math.max(0, clampedY) + "px";
}

/// Close file context menu on click outside.
document.addEventListener("click", () => fileContextMenu.classList.add("hidden"));

/// Delete a file or directory with confirmation.
async function deleteFileEntry(entry) {
  const typeStr = entry.isDir ? "folder" : "file";
  const msg = t("msg.delete.confirm", { type: typeStr, name: entry.name });
  if (!confirm(msg)) return;
  try {
    await tauriInvoke("delete_file", { path: entry.path });
    refreshFileTree();
  } catch (e) {
    alert(t("msg.delete.fail", { err: e }));
  }
}

/// Rename a file or directory.
async function renameFileEntry(entry) {
  const newName = prompt(t("msg.rename.prompt", { name: entry.name }), entry.name);
  if (!newName || newName === entry.name) return;
  const parent = parentDir(entry.path);
  const dest = joinPath(parent, newName);
  try {
    await tauriInvoke("move_file", { srcPath: entry.path, destPath: dest });
    refreshFileTree();
  } catch (e) {
    alert(t("msg.rename.fail", { err: e }));
  }
}

/// Paste (copy or cut) from clipboard into a folder.
async function pasteFileEntry(targetEntry) {
  if (!fileTreeClipboard) return;
  const targetDir = targetEntry.isDir ? targetEntry.path : parentDir(targetEntry.path);
  const srcName = baseName(fileTreeClipboard.path);
  const destPath = joinPath(targetDir, srcName);
  if (fileTreeClipboard.path === destPath) {
    alert(t("msg.paste.same"));
    return;
  }
  try {
    if (fileTreeClipboard.operation === "copy") {
      await tauriInvoke("copy_file", { srcPath: fileTreeClipboard.path, destPath });
    } else {
      await tauriInvoke("move_file", { srcPath: fileTreeClipboard.path, destPath });
      fileTreeClipboard = null; // cut is one-time
    }
    refreshFileTree();
  } catch (e) {
    alert(t("msg.paste.fail", { err: e }));
  }
}

/// Filter input handler.
fileTreeFilter.addEventListener("input", () => refreshFileTree());

/// Resizable file tree divider.
(function setupFileTreeDivider() {
  let dragging = false;
  fileTreeDivider.addEventListener("mousedown", (e) => {
    dragging = true;
    fileTreeDivider.classList.add("dragging");
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const rect = fileTreePanel.getBoundingClientRect();
    const newWidth = Math.min(Math.max(e.clientX - rect.left, 180), 500);
    fileTreePanel.style.width = newWidth + "px";
  });
  document.addEventListener("mouseup", () => {
    if (dragging) { dragging = false; fileTreeDivider.classList.remove("dragging"); }
  });
})();

// ── Initialize ──────────────────────────────────────────────────────────────

function waitForTauri(maxRetries) {
  return new Promise((resolve, reject) => {
    let attempts = 0;
    function check() {
      if (window.__TAURI__) resolve(window.__TAURI__);
      else if (attempts < maxRetries) { attempts++; setTimeout(check, 100); }
      else reject(new Error("Tauri runtime not available"));
    }
    check();
  });
}

async function init() {
  try {
    // Load saved language preference.
    try {
      const savedLang = localStorage.getItem("womd_lang");
      if (savedLang && I18N_STRINGS[savedLang]) currentLang = savedLang;
    } catch {}
    applyI18n();

    await waitForTauri(50);
    // Check if this is a torn-off window — look for a file path in localStorage.
    // The parent window stores it with key "tear_off_<label>" before creating this window.
    // We scan all localStorage keys starting with "tear_off_" to find ours.
    let tearOffFile = null;
    // The window label is available via Tauri API.
    let myLabel = "main";
    try {
      const tauri = window.__TAURI__;
      if (tauri?.webviewWindow?.getCurrent) {
        myLabel = tauri.webviewWindow.getCurrent().label;
      } else if (tauri?.window?.getCurrent) {
        myLabel = tauri.window.getCurrent().label;
      }
    } catch (e) { /* ignore */ }

    // Try label-specific key first, then scan all tear_off keys.
    tearOffFile = localStorage.getItem("tear_off_" + myLabel);
    if (!tearOffFile) {
      // Scan all keys — the label might not match exactly.
      for (let i = 0; i < localStorage.length; i++) {
        const key = localStorage.key(i);
        if (key && key.startsWith("tear_off_")) {
          tearOffFile = localStorage.getItem(key);
          localStorage.removeItem(key); // consume it
          break;
        }
      }
    } else {
      localStorage.removeItem("tear_off_" + myLabel); // consume it
    }

    if (tearOffFile) {
      isTearOffWindow = true;
      await openDocument(tearOffFile);
      // Tear-off window: show only the current tab, not all tabs from shared state.
      openTabs = openTabs.filter(t => t.id === activeTabId);
      renderTabs();
    } else {
      await newDocument();
    }

    // Warn before closing the window if there are unsaved changes.
    window.addEventListener("beforeunload", (e) => {
      if (isDirty) {
        e.preventDefault();
        e.returnValue = "";
      }
    });
  } catch (e) {
    console.error("init failed:", e);
    blockEditor.innerHTML = `<div style="padding:24px;color:#f38ba8;font-family:monospace"><h2>${t("msg.init.fail")}</h2><p>${escapeHtml(e.message)}</p></div>`;
  }
}

// Open a document by file path (used by torn-off windows).
async function openDocument(path) {
  try {
    const info = await tauriInvoke("open_document", { path });
    currentText = info.text || "";
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
  } catch (e) {
    console.error("openDocument failed:", e);
    alert(t("msg.open.fail", { path: path, err: e }));
    await newDocument();
  }
}

if (document.readyState === "loading") { document.addEventListener("DOMContentLoaded", init); }
else { init(); }

// ── Settings modal ─────────────────────────────────────────────────────────

/// Open settings modal.
function openSettings() {
  settingsModal.classList.remove("hidden");
  renderThemeGrid();
  renderSyntaxGrid();
  renderThemePreview();
  refreshGithubStatus();
}

/// Close settings modal.
function closeSettings() {
  settingsModal.classList.add("hidden");
}

btnSettings.addEventListener("click", openSettings);
settingsClose.addEventListener("click", closeSettings);
settingsModal.addEventListener("click", (e) => {
  if (e.target === settingsModal) closeSettings();
});

// Settings tab switching.
document.querySelectorAll(".settings-tab").forEach(btn => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".settings-tab").forEach(b => b.classList.remove("active"));
    btn.classList.add("active");
    const tabName = btn.dataset.stab;
    document.querySelectorAll(".settings-panel").forEach(p => p.classList.add("hidden"));
    document.getElementById("settings-" + tabName).classList.remove("hidden");
    if (tabName === "github") refreshGithubStatus();
    if (tabName === "language") renderLanguageGrid();
  });
});

/// Render the language selection grid.
function renderLanguageGrid() {
  const grid = document.getElementById("language-grid");
  if (!grid) return;
  let html = "";
  for (const lang of I18N_LANGUAGES) {
    const selected = currentLang === lang.code ? " selected" : "";
    html += `<div class="lang-card${selected}" data-lang-code="${escapeAttr(lang.code)}" onclick="selectLanguage('${escapeAttr(lang.code)}')">
      <span class="lang-card-flag">${lang.flag}</span>
      <span class="lang-card-name">${escapeHtml(lang.name)}</span>
    </div>`;
  }
  grid.innerHTML = html;
}

/// Select interface language.
window.selectLanguage = function(code) {
  setLanguage(code);
  renderLanguageGrid();
  // Re-render dynamic UI that uses t().
  renderThemeGrid();
  renderSyntaxGrid();
  renderGitChanges();
  refreshFileTree();
  // Re-render git panel sections that contain translatable labels.
  refreshGitAll();
  // Re-render the diff viewer if visible.
  syncDiffTabIfVisible();
  // Update status bar (block count label is language-dependent).
  if (syntaxBlocks) blockCount.textContent = `${syntaxBlocks.length} ${t("status.blocks")}`;
};

/// Render the interface theme grid.
function renderThemeGrid() {
  const grid = document.getElementById("theme-grid");
  if (!grid) return;
  let html = "";
  for (const theme of THEMES) {
    const selected = settings.theme === theme.id ? " selected" : "";
    const swatches = theme.swatches.map(c => `<div class="theme-swatch" style="background:${c}"></div>`).join("");
    html += `<div class="theme-card${selected}" data-theme-id="${escapeAttr(theme.id)}" onclick="selectTheme('${escapeAttr(theme.id)}')">
      <div class="theme-card-name">${escapeHtml(theme.name)}</div>
      <div class="theme-card-swatches">${swatches}</div>
    </div>`;
  }
  grid.innerHTML = html;
}

/// Render the syntax palette grid.
function renderSyntaxGrid() {
  const grid = document.getElementById("syntax-grid");
  if (!grid) return;
  let html = "";
  for (const palette of SYNTAX_PALETTES) {
    const selected = settings.syntaxTheme === palette.id ? " selected" : "";
    const swatches = palette.swatches.map(c => `<div class="theme-swatch" style="background:${c}"></div>`).join("");
    html += `<div class="theme-card${selected}" data-syntax-id="${escapeAttr(palette.id)}" onclick="selectSyntaxTheme('${escapeAttr(palette.id)}')">
      <div class="theme-card-name">${escapeHtml(palette.name)}</div>
      <div class="theme-card-swatches">${swatches}</div>
    </div>`;
  }
  grid.innerHTML = html;
}

/// Select an interface theme.
window.selectTheme = function(themeId) {
  if (!THEMES.some(t => t.id === themeId)) return;
  settings.theme = themeId;
  // Also update syntax theme to match if they're in sync.
  settings.syntaxTheme = themeId;
  applyTheme();
  saveSettings();
  renderThemeGrid();
  renderSyntaxGrid();
  renderThemePreview();
};

/// Select a syntax palette independently.
window.selectSyntaxTheme = function(themeId) {
  if (!SYNTAX_PALETTES.some(p => p.id === themeId)) return;
  settings.syntaxTheme = themeId;
  // For now, syntax colors are tied to the interface theme's data-theme attribute.
  // To support independent syntax palettes, we'd need a separate attribute.
  // For now, just update the interface theme to match.
  settings.theme = themeId;
  applyTheme();
  saveSettings();
  renderThemeGrid();
  renderSyntaxGrid();
  renderThemePreview();
};

/// Render a preview of the current theme.
function renderThemePreview() {
  const preview = document.getElementById("theme-preview");
  if (!preview) return;
  preview.innerHTML = `
    <div class="pv-heading">Heading Example</div>
    <p class="syn-plain">This is a paragraph with <span class="pv-link">a link</span>,
      <span class="pv-emphasis">emphasized text</span>, and <span class="pv-code">inline code</span>.</p>
    <div class="pv-quote">This is a blockquote.</div>
    <div class="pv-syntax">
<span class="syn-cmt">// Example code</span>
<span class="syn-kw">fn</span> <span class="syn-fn">main</span>() {
  <span class="syn-kw">let</span> <span class="syn-var">x</span>: <span class="syn-type">i32</span> <span class="syn-op">=</span> <span class="syn-num">42</span>;
  <span class="syn-fn">println!</span>(<span class="syn-str">"Hello, world!"</span>);
}
    </div>
  `;
}

// Apply theme on load.
applyTheme();

// ── GitHub integration ─────────────────────────────────────────────────────

/// Refresh GitHub auth status and repo info.
async function refreshGithubStatus() {
  const statusDiv = document.getElementById("github-auth-status");
  const repoDiv = document.getElementById("github-repo-info");
  if (!statusDiv) return;
  statusDiv.innerHTML = '<div class="gh-status-line"><span class="gh-status-icon">⏳</span> Checking GitHub authentication...</div>';
  try {
    const result = await tauriInvoke("github_auth_status");
    if (result.authenticated) {
      statusDiv.innerHTML = `<div class="gh-status-line"><span class="gh-status-icon">✓</span> Authenticated as <span class="gh-user">${escapeHtml(result.user)}</span></div>
        <div class="gh-status-line" style="color:var(--fg-muted);font-size:11px">via gh CLI</div>`;
    } else {
      statusDiv.innerHTML = `<div class="gh-status-line"><span class="gh-status-icon">✗</span> <span class="gh-error">Not authenticated</span></div>
        <div class="gh-status-line" style="color:var(--fg-muted);font-size:11px">Click "Login with gh CLI" to authenticate</div>`;
    }
  } catch (e) {
    statusDiv.innerHTML = `<div class="gh-status-line"><span class="gh-status-icon">✗</span> <span class="gh-error">Error: ${escapeHtml(String(e))}</span></div>`;
  }
  // Try to load repo info.
  if (repoDiv) {
    try {
      const repo = await tauriInvoke("github_repo_metadata");
      if (repo && repo.full_name) {
        repoDiv.innerHTML = `<div class="gh-repo-name">${escapeHtml(repo.full_name)}</div>
          <div class="gh-repo-branch">Default branch: ${escapeHtml(repo.default_branch || "unknown")}</div>
          <div class="gh-repo-url">${escapeHtml(repo.html_url || "")}</div>`;
      } else {
        repoDiv.innerHTML = '<div style="color:var(--fg-muted)">No repository metadata available. Make sure you are in a GitHub repo.</div>';
      }
    } catch (e) {
      repoDiv.innerHTML = `<div style="color:var(--fg-muted)">No repository metadata available.</div>`;
    }
  }
}

document.getElementById("btn-gh-login").addEventListener("click", async () => {
  try {
    await tauriInvoke("github_login");
    refreshGithubStatus();
  } catch (e) { alert("GitHub login failed: " + e); }
});

document.getElementById("btn-gh-refresh").addEventListener("click", refreshGithubStatus);

document.getElementById("btn-gh-logout").addEventListener("click", async () => {
  if (!confirm("Logout from GitHub?")) return;
  try {
    await tauriInvoke("github_logout");
    refreshGithubStatus();
  } catch (e) { alert("Logout failed: " + e); }
});

document.getElementById("btn-gh-prs").addEventListener("click", async () => {
  const div = document.getElementById("github-prs");
  div.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">Loading PRs...</div>';
  try {
    const prs = await tauriInvoke("github_pull_requests");
    if (!prs || prs.length === 0) {
      div.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">No pull requests</div>';
    } else {
      let html = '<div class="settings-section-title">Pull Requests</div>';
      for (const pr of prs) {
        const stateClass = pr.state === "open" ? "open" : "closed";
        html += `<div class="github-pr-entry">
          <span class="pr-number">#${pr.number}</span>
          <span class="pr-title">${escapeHtml(pr.title)}</span>
          <span class="pr-state ${stateClass}">${escapeHtml(pr.state)}</span>
        </div>`;
      }
      div.innerHTML = html;
    }
  } catch (e) {
    div.innerHTML = `<div style="padding:8px;color:var(--diff-del)">${escapeHtml(String(e))}</div>`;
  }
});

document.getElementById("btn-gh-branches").addEventListener("click", async () => {
  const div = document.getElementById("github-branches");
  div.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">Loading branches...</div>';
  try {
    const branches = await tauriInvoke("github_remote_branches");
    if (!branches || branches.length === 0) {
      div.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">No remote branches</div>';
    } else {
      let html = '<div class="settings-section-title">Remote Branches</div>';
      for (const br of branches) {
        html += `<div class="github-branch-entry"><span class="br-name">${escapeHtml(br.name)}</span></div>`;
      }
      div.innerHTML = html;
    }
  } catch (e) {
    div.innerHTML = `<div style="padding:8px;color:var(--diff-del)">${escapeHtml(String(e))}</div>`;
  }
});
