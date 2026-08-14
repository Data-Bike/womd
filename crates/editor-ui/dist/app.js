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
let activeTabId = 0;
let openTabs = [];
let gitPanelVisible = false;
let gitData = { status: null, branches: [], diff: [], log: [] };
let editingBlockIndex = -1;
let suppressRender = false;
let suppressBlur = false;  // prevent exitEditMode when clicking toolbar buttons
let isTearOffWindow = false; // true for torn-off windows — show only their own tab

// ── DOM ─────────────────────────────────────────────────────────────────────
const blockEditor = document.getElementById("block-editor");
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
  currentText = info.text;
  activeTabId = info.tab_id;
  updateUI(info);
  await refreshTabs();
  await refreshSyntax();
  editorFocusFirst();
}

async function openDocument(path) {
  const info = await tauriInvoke("open_document", { path });
  currentText = info.text;
  activeTabId = info.tab_id;
  updateUI(info);
  await refreshTabs();
  await refreshSyntax();
  refreshGitAll();
  editorFocusFirst();
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
  currentText = r.text;
  updateUI(r);
  await refreshSyntax();
  scheduleAutosave();
}

async function doRedo() {
  flushEdits();
  const r = await tauriInvoke("redo");
  currentText = r.text;
  updateUI(r);
  await refreshSyntax();
  scheduleAutosave();
}

async function refreshSyntax() {
  try { syntaxBlocks = await tauriInvoke("get_syntax_tree"); }
  catch (e) { syntaxBlocks = []; }
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

async function switchTab(tabId) {
  if (tabId === activeTabId) return;
  flushEdits();
  const info = await tauriInvoke("switch_tab", { tabId });
  currentText = info.text;
  activeTabId = info.tab_id;
  updateUI(info);
  await refreshSyntax();
  await refreshTabs();
  refreshGitAll();
  editorFocusFirst();
}

async function closeTab(tabId) {
  const tab = openTabs.find(t => t.id === tabId);
  if (tab && tab.is_dirty) {
    if (!confirm(`"${tab.file_name}" has unsaved changes.\n\nClose anyway? Unsaved changes will be lost.`)) return;
  }
  flushEdits();
  const result = await tauriInvoke("close_tab", { tabId });
  if (result) {
    currentText = result.text;
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
    alert("This tab has no file path — save it first before tearing off.");
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
        alert("Could not create new window: " + e);
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
    alert("Could not open new window: " + e);
  }
}

// ── Send full-text edit to backend ──────────────────────────────────────────

async function sendReplace(start, end, newText) {
  const r = await tauriInvoke("replace_text", { args: { start, end, new_text: newText } });
  currentText = r.text;
  isDirty = r.is_dirty;
  updateDirtyState();
  blockCount.textContent = `${r.block_count} blocks`;
  await refreshSyntax();
  refreshTabs();
  scheduleAutosave();
  return r;
}

function flushEdits() {
  clearTimeout(debounceTimer);
  if (editingBlockIndex >= 0) {
    // Commit current edit synchronously into currentText.
    const ta = blockEditor.querySelector(".md-block.editing .md-block-textarea");
    if (ta) commitEdit(ta.value);
  }
}

// ── Autosave ────────────────────────────────────────────────────────────────

function scheduleAutosave() {
  clearTimeout(autosaveTimer);
  if (!isDirty) return;
  autosaveTimer = setTimeout(async () => {
    const tab = openTabs.find(t => t.id === activeTabId);
    if (tab && tab.file_name !== "untitled.md" && tab.file_name !== "untitled" && isDirty) {
      try {
        flushEdits();
        setTimeout(async () => {
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
  blockEditor.innerHTML = "";

  if (syntaxBlocks.length === 0) {
    const empty = document.createElement("div");
    empty.className = "md-block empty-block";
    empty.textContent = "Empty document — start typing or open a file.";
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

  for (let i = 0; i < syntaxBlocks.length; i++) {
    const block = syntaxBlocks[i];
    // Skip blank-line blocks — they're spacing, not content.
    if (block.kind === "blank-line") continue;
    // Skip link-ref-def blocks — hidden in WYSIWYG.
    if (block.kind === "link-ref-def") continue;
    const el = createBlockElement(i, block);
    blockEditor.appendChild(el);
  }

  // Add a trailing empty block for appending content.
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
  });
  el.addEventListener("click", (e) => {
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
      return node.content; // Render raw HTML
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
      return node.content; // Render raw HTML inline
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
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
function escapeAttr(s) { return s.replace(/"/g, "&quot;").replace(/'/g, "&#39;"); }

// ── Click-to-edit ───────────────────────────────────────────────────────────

function enterEditMode(blockIndex) {
  if (editingBlockIndex >= 0) exitEditMode();
  if (blockIndex < 0 || blockIndex >= syntaxBlocks.length) return;

  editingBlockIndex = blockIndex;
  suppressRender = true;

  const blockEl = blockEditor.querySelector(`[data-block-index="${blockIndex}"]`);
  if (!blockEl) { suppressRender = false; return; }

  const source = syntaxBlocks[blockIndex].source;
  blockEl.classList.add("editing");
  blockEl.innerHTML = "";

  const ta = document.createElement("textarea");
  ta.className = "md-block-textarea";
  ta.value = source;
  ta.spellcheck = false;
  blockEl.appendChild(ta);

  // Auto-size.
  autoSizeTextarea(ta);

  ta.focus();
  // Place cursor at end.
  ta.selectionStart = ta.value.length;
  ta.selectionEnd = ta.value.length;

  ta.addEventListener("input", () => autoSizeTextarea(ta));
  ta.addEventListener("blur", () => {
    if (suppressBlur) { suppressBlur = false; return; }
    exitEditMode();
  });
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); exitEditMode(); return; }
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); exitEditMode(); return; }
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
  if (editingBlockIndex < 0) return;
  const idx = editingBlockIndex;
  const blockEl = blockEditor.querySelector(`[data-block-index="${idx}"]`);
  if (blockEl) {
    const ta = blockEl.querySelector(".md-block-textarea");
    if (ta) {
      const newSource = ta.value;
      commitEdit(newSource);
    }
  }
}

async function commitEdit(newSource) {
  const idx = editingBlockIndex;
  editingBlockIndex = -1;
  suppressRender = false;

  if (idx < 0 || idx >= syntaxBlocks.length) return;
  const oldSource = syntaxBlocks[idx].source;
  if (newSource === oldSource) {
    renderBlocks();
    return;
  }

  // Find oldSource in currentText by string matching, not byte offsets.
  // Byte offsets from Rust don't match JS string indices for non-ASCII text.
  const pos = currentText.indexOf(oldSource);
  let newText;
  if (pos >= 0) {
    newText = currentText.substring(0, pos) + newSource + currentText.substring(pos + oldSource.length);
  } else {
    // Fallback: use byte offsets (works for pure ASCII).
    const start = syntaxBlocks[idx].start;
    const end = syntaxBlocks[idx].end;
    newText = currentText.substring(0, start) + newSource + currentText.substring(end);
  }
  await sendReplace(0, currentText.length, newText);
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
function applyInlineFormat(prefix, suffix) {
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

  // Replace block source in full text using string matching (not byte offsets).
  const pos = currentText.indexOf(block.source);
  let newText;
  if (pos >= 0) {
    newText = currentText.substring(0, pos) + newSource + currentText.substring(pos + block.source.length);
  } else {
    newText = newSource; // fallback: replace everything
  }
  sendReplace(0, currentText.length, newText);
}

// Helper: replace a block's source in currentText using string matching.
function replaceBlockSource(block, newSource) {
  const pos = currentText.indexOf(block.source);
  if (pos >= 0) {
    return currentText.substring(0, pos) + newSource + currentText.substring(pos + block.source.length);
  }
  return newSource; // fallback
}

// Apply line-prefix formatting (list, quote, heading) to current selection.
function applyLineFormat(prefix) {
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
  const newText = replaceBlockSource(block, newSource);
  sendReplace(0, currentText.length, newText);
}

// Apply heading level to current block.
function applyHeading(level) {
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
  const newText = replaceBlockSource(block, newSource);
  sendReplace(0, currentText.length, newText);
}

// Insert a link around the current selection.
function applyLink() {
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
  const newText = replaceBlockSource(block, newSource);
  sendReplace(0, currentText.length, newText);
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
      if (source[i] === "]" && source[i+1] === "(") {
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
  blockCount.textContent = `${info.block_count} blocks`;
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
  try { gitData.status = await tauriInvoke("get_git_status"); gitBranchInfo.textContent = gitData.status.branch ? `branch: ${gitData.status.branch}` : ""; renderGitChanges(); }
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
  if (!s) { gitChanges.innerHTML = '<div class="git-section-title">No repository</div>'; return; }
  let html = "";
  if (s.staged.length > 0) {
    html += '<div class="git-section-title">Staged Changes</div>';
    for (const f of s.staged) {
      html += renderFileDiffEntry(f, "staged");
    }
  }
  if (s.changes.length > 0) {
    html += '<div class="git-section-title">Changes</div>';
    for (const f of s.changes) {
      html += renderFileDiffEntry(f, "changes");
    }
  }
  if (s.untracked.length > 0) {
    html += '<div class="git-section-title">Untracked</div>';
    for (const f of s.untracked) {
      html += renderFileDiffEntry(f, "untracked");
    }
  }
  if (s.staged.length > 0) {
    html += '<div class="git-section-title">Commit</div>';
    html += '<input type="text" class="git-commit-input" id="commit-msg" placeholder="Commit message..." />';
    html += '<button class="git-action-btn" onclick="gitCommit()">Commit staged changes</button>';
  }
  if (s.changes.length === 0 && s.staged.length === 0 && s.untracked.length === 0) {
    html += '<div style="padding:16px;color:var(--fg-muted);text-align:center">No changes</div>';
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
  inlineDiv.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">Loading diff...</div>';
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
  if (!gitData.branches || gitData.branches.length === 0) { gitBranches.innerHTML = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No branches</div>'; return; }
  let html = '<div class="git-section-title">Branches</div>';
  html += '<div class="git-inline-form"><input type="text" id="new-branch-name" placeholder="New branch name..." /><button class="git-action-btn" onclick="gitCreateBranch()">Create</button></div>';
  for (const b of gitData.branches) {
    const actions = b.is_current ? '' : `<div class="git-branch-actions"><button class="git-mini-btn" onclick="event.stopPropagation();gitCheckout('${escapeAttr(b.name)}')">Checkout</button><button class="git-mini-btn" onclick="event.stopPropagation();gitMergeBranch('${escapeAttr(b.name)}')">Merge</button><button class="git-mini-btn" onclick="event.stopPropagation();gitRebaseBranch('${escapeAttr(b.name)}')">Rebase</button><button class="git-mini-btn git-mini-danger" onclick="event.stopPropagation();gitDeleteBranch('${escapeAttr(b.name)}')">Delete</button></div>`;
    html += `<div class="git-branch-entry ${b.is_current ? 'current' : ''}"><div class="git-branch-row" onclick="gitCheckout('${escapeAttr(b.name)}')"><span class="git-branch-icon">${b.is_current ? '●' : '○'}</span><span class="git-branch-name">${escapeHtml(b.name)}</span>${b.ahead > 0 ? `<span class="git-branch-ahead">↓${b.ahead}</span>` : ''}${b.behind > 0 ? `<span class="git-branch-behind">↑${b.behind}</span>` : ''}</div>${actions}</div>`;
  }
  gitBranches.innerHTML = html;
}

function renderGitHistory() {
  if (!gitData.log || gitData.log.length === 0) { gitHistory.innerHTML = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No commits</div>'; return; }
  let html = '<div class="git-section-title">Commit History</div>';
  for (const c of gitData.log) {
    html += `<div class="git-commit-entry"><div class="git-commit-row"><div class="git-commit-sha">${escapeHtml(c.sha.substring(0, 8))}</div><div class="git-commit-msg">${escapeHtml(c.message)}</div><div class="git-commit-meta">${escapeHtml(c.author)} · ${escapeHtml(c.date)}</div></div><div class="git-commit-actions"><button class="git-mini-btn" onclick="event.stopPropagation();gitCherryPick('${escapeAttr(c.sha)}')">Cherry-pick</button><button class="git-mini-btn" onclick="event.stopPropagation();gitRevertCommit('${escapeAttr(c.sha)}')">Revert</button><button class="git-mini-btn" onclick="event.stopPropagation();gitResetSoft('${escapeAttr(c.sha)}')">Reset soft</button><button class="git-mini-btn git-mini-danger" onclick="event.stopPropagation();gitResetHard('${escapeAttr(c.sha)}')">Reset hard</button></div></div>`;
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
      gitDiff.innerHTML = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No changes</div>';
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
  let html = '<div class="git-section-title">Stash</div>';
  html += '<button class="git-action-btn" onclick="gitStashPush()">Stash current changes</button>';
  const stash = gitData.stash || [];
  if (stash.length === 0) {
    html += '<div style="padding:16px;color:var(--fg-muted);text-align:center">No stashed changes</div>';
  } else {
    for (const s of stash) {
      html += `<div class="git-stash-entry"><div class="git-stash-info"><span class="git-stash-idx">stash@{${s.index}}</span><span class="git-stash-msg">${escapeHtml(s.message)}</span></div><div class="git-stash-actions"><button class="git-mini-btn" onclick="gitStashApply(${s.index})">Apply</button><button class="git-mini-btn" onclick="gitStashPop(${s.index})">Pop</button><button class="git-mini-btn git-mini-danger" onclick="gitStashDrop(${s.index})">Drop</button></div></div>`;
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
    for (const t of tags) {
      html += `<div class="git-tag-entry"><span class="git-tag-icon">🏷</span><span class="git-tag-name">${escapeHtml(t.name)}</span><span class="git-tag-target">${escapeHtml(t.target)}</span>${t.message ? `<span class="git-tag-msg">${escapeHtml(t.message)}</span>` : ''}<button class="git-mini-btn git-mini-danger" onclick="gitDeleteTag('${escapeAttr(t.name)}')">Delete</button></div>`;
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
      html += `<div class="git-remote-entry"><div class="git-remote-info"><span class="git-remote-name">${escapeHtml(r.name)}</span><span class="git-remote-url">${escapeHtml(r.fetch_url || r.url)}</span></div><div class="git-remote-actions"><button class="git-mini-btn" onclick="gitFetchRemote('${escapeAttr(r.name)}')">Fetch</button></div></div>`;
    }
  }
  html += '<div class="git-section-title" style="margin-top:12px">Push / Pull</div>';
  html += '<div class="git-inline-form"><input type="text" id="push-remote-input" placeholder="remote" /><input type="text" id="push-branch-input" placeholder="branch" /><button class="git-action-btn" onclick="gitPushToRemote(false)">Push</button><button class="git-action-btn git-mini-danger" onclick="gitPushToRemote(true)">Force</button><button class="git-action-btn" onclick="gitPullFromRemote()">Pull</button></div>';
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
  html += '<div id="diff-viewer-result"></div>';
  gitDiff.innerHTML = html;
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
  const mode = document.getElementById("diff-mode-select").value;
  const resultDiv = document.getElementById("diff-viewer-result");
  resultDiv.innerHTML = '<div style="padding:8px;color:var(--fg-muted)">Loading diff...</div>';
  try {
    let diffs;
    if (mode === "wt-vs-commit") {
      const commit = document.getElementById("diff-commit-a").value;
      diffs = await tauriInvoke("git_diff_vs_commit", { commit });
    } else {
      const a = document.getElementById("diff-commit-a").value;
      const b = document.getElementById("diff-commit-b").value;
      diffs = await tauriInvoke("git_diff_commits", { commitA: a, commitB: b });
    }
    let html = "";
    if (!diffs || diffs.length === 0) {
      html = '<div style="padding:16px;color:var(--fg-muted);text-align:center">No differences</div>';
    } else {
      for (const file of diffs) {
        html += `<div class="diff-file-header">${escapeHtml(file.path)}</div>`;
        // Fetch line-level diff for this specific file.
        let fileDiff;
        if (mode === "wt-vs-commit") {
          const commit = document.getElementById("diff-commit-a").value;
          fileDiff = await tauriInvoke("git_diff_file_vs_commit", { filePath: file.path, commit });
        } else {
          const a = document.getElementById("diff-commit-a").value;
          const b = document.getElementById("diff-commit-b").value;
          fileDiff = await tauriInvoke("git_diff_file_commits", { filePath: file.path, commitA: a, commitB: b });
        }
        html += renderDiffHtml(fileDiff);
      }
    }
    resultDiv.innerHTML = html;
  } catch (e) { resultDiv.innerHTML = `<div style="padding:8px;color:#f44">Diff error: ${escapeHtml(String(e))}</div>`; }
};

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
  items.push({ label: "View diff", action: "view-diff" });
  items.push({ separator: true });
  if (section === "staged") {
    items.push({ label: "Unstage file", action: "unstage" });
  } else if (section === "changes") {
    items.push({ label: "Stage file", action: "stage" });
    items.push({ separator: true });
    items.push({ label: "Discard changes", action: "discard", danger: true });
  } else if (section === "untracked") {
    items.push({ label: "Stage file", action: "stage" });
    items.push({ separator: true });
    items.push({ label: "Delete file", action: "delete", danger: true });
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
    if (tabName === "diff") renderDiffViewer();
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

btnHr.addEventListener("click", () => {
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
    sendReplace(0, currentText.length, newText);
  }
});

btnCodeblock.addEventListener("click", () => {
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
    sendReplace(0, currentText.length, newText);
  }
});

btnGit.addEventListener("click", () => {
  gitPanelVisible = !gitPanelVisible;
  gitPanel.classList.toggle("hidden", !gitPanelVisible);
  gitDivider.classList.toggle("hidden", !gitPanelVisible);
  btnGit.classList.toggle("active", gitPanelVisible);
  if (gitPanelVisible) refreshGitAll();
});

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
    const confirmed = confirm(`Open external link?\n\n${href}\n\nThis will open in your default browser.`);
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
    currentText = info.text;
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
  } catch (e) {
    alert("Could not open file: " + relativePath + "\n\n" + e);
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
});

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
  } catch (e) {
    console.error("init failed:", e);
    blockEditor.innerHTML = '<div style="padding:24px;color:#f38ba8;font-family:monospace"><h2>Failed to initialize</h2><p>' + escapeHtml(e.message) + '</p></div>';
  }
}

// Open a document by file path (used by torn-off windows).
async function openDocument(path) {
  try {
    const info = await tauriInvoke("open_document", { path });
    currentText = info.text;
    activeTabId = info.tab_id;
    updateUI(info);
    await refreshSyntax();
    await refreshTabs();
    refreshGitAll();
  } catch (e) {
    console.error("openDocument failed:", e);
    alert("Could not open file: " + path + "\n\n" + e);
    await newDocument();
  }
}

if (document.readyState === "loading") { document.addEventListener("DOMContentLoaded", init); }
else { init(); }
