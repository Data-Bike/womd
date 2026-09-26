//! WoMD Tauri 2 UI shell (ADR-001).
//!
//! Exposes `editor-core`'s `DocumentBuffer` to the frontend via Tauri commands.
//! Supports multiple documents in Chrome-style tabs. The frontend is a vanilla
//! HTML/CSS/JS editor (no JS framework) that calls these commands through
//! `window.__TAURI__.core.invoke`.

#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "custom-protocol"), allow(dead_code))]

use std::sync::Mutex;
use std::path::{Path, PathBuf};
use std::collections::HashMap;

use editor_core::{DocumentBuffer, EditTransaction, TextEdit};
use editor_domain::{ByteOffset, ByteRange, MarkdownProfile, ids::DocumentId};
use editor_git::{GitExtended, VersionControl};
use regex::Regex;
use serde::{Deserialize, Serialize};
use base64::{prelude::BASE64_STANDARD, Engine};

const WELCOME_MD: &str = include_str!("welcome.md");

// ---------------------------------------------------------------------------
// State: multiple document tabs
// ---------------------------------------------------------------------------

/// A single open document tab.
struct DocumentTab {
    id: u64,
    buffer: DocumentBuffer,
    file_path: Option<PathBuf>,
}

impl DocumentTab {
    fn file_name(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled.md".to_string())
    }
}

/// Application state: a list of open document tabs + the active tab index.
struct AppState {
    tabs: Vec<DocumentTab>,
    active: usize,
    next_id: u64,
    /// File paths for torn-off windows, keyed by window label.
    /// The new window calls `get_tear_off_file` to retrieve its file path.
    tear_off_files: HashMap<String, String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self { tabs: Vec::new(), active: 0, next_id: 1, tear_off_files: HashMap::new() }
    }
}

impl AppState {
    fn active_tab(&self) -> Result<&DocumentTab, String> {
        self.tabs.get(self.active).ok_or_else(|| "no document open".to_string())
    }
    fn active_tab_mut(&mut self) -> Result<&mut DocumentTab, String> {
        self.tabs.get_mut(self.active).ok_or_else(|| "no document open".to_string())
    }
    fn push_tab(&mut self, tab: DocumentTab) -> usize {
        let idx = self.tabs.len();
        self.tabs.push(tab);
        self.active = idx;
        idx
    }
}

// ---------------------------------------------------------------------------
// Command argument / return types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct DocumentInfo {
    /// Full document text — None for large documents (>1MB) to avoid
    /// sending 100+ MB strings over IPC. Frontend uses get_text_range
    /// for lazy loading in that case.
    text: Option<String>,
    file_name: String,
    is_dirty: bool,
    block_count: usize,
    byte_len: usize,
    tab_id: u64,
}

#[derive(Serialize, Deserialize)]
struct ReplaceTextArgs {
    start: u64,
    end: u64,
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Serialize, Deserialize)]
struct EditResult {
    /// Full text after edit — None for large documents (>1MB) to avoid
    /// serializing 100+ MB on every keystroke. Frontend uses block data
    /// for rendering instead.
    text: Option<String>,
    is_dirty: bool,
    block_count: usize,
    /// False when the command was a byte-identical no-op (no edit applied, no
    /// undo step recorded). Lets the frontend avoid emitting phantom dirty.
    changed: bool,
}

#[derive(Serialize, Deserialize)]
struct SyntaxBlock {
    kind: String,
    source: String,
    start: u64,
    end: u64,
    /// Parsed AST node for rich rendering (optional; null for trivial blocks).
    node: Option<AstNode>,
}

/// Lightweight block metadata for virtualized rendering — no source text or AST.
/// The frontend requests full block data (source + AST) only for visible blocks.
#[derive(Serialize, Deserialize)]
struct SyntaxBlockMeta {
    kind: String,
    start: u64,
    end: u64,
}

/// Threshold for "large document" — above this, full text is not serialized
/// on every operation. 1 MB ≈ 20K lines of typical Markdown.
const LARGE_DOC_THRESHOLD: usize = 1_048_576;

/// Build a DocumentInfo from a tab, conditionally serializing text only for small docs.
fn doc_info_from_tab(tab: &DocumentTab) -> DocumentInfo {
    let byte_len = tab.buffer.len() as usize;
    let block_count = tab.buffer.syntax().blocks.len();
    let text = if byte_len > LARGE_DOC_THRESHOLD {
        None
    } else {
        Some(String::from_utf8_lossy(&tab.buffer.serialize()).to_string())
    };
    DocumentInfo {
        text,
        file_name: tab.file_name(),
        is_dirty: tab.buffer.is_dirty(),
        block_count,
        byte_len,
        tab_id: tab.id,
    }
}

/// Build an EditResult from a tab, conditionally serializing text only for small docs.
fn edit_result_from_tab(tab: &DocumentTab) -> EditResult {
    edit_result(tab, true)
}

fn edit_result(tab: &DocumentTab, changed: bool) -> EditResult {
    let byte_len = tab.buffer.len() as usize;
    let text = if byte_len > LARGE_DOC_THRESHOLD {
        None
    } else {
        Some(String::from_utf8_lossy(&tab.buffer.serialize()).to_string())
    };
    EditResult {
        text,
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
        changed,
    }
}

/// A serializable AST node — block or inline — for the frontend renderer.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum AstNode {
    Heading { level: u8, children: Vec<AstNode> },
    Paragraph { children: Vec<AstNode> },
    ThematicBreak,
    BlockQuote { children: Vec<AstNode> },
    List { ordered: bool, start: u32, items: Vec<ListItemNode> },
    CodeBlock { fenced: bool, language: String, content: String },
    Table { alignments: Vec<String>, header: Vec<TableCellNode>, rows: Vec<Vec<TableCellNode>> },
    HtmlBlock { content: String },
    LinkRefDef { label: String, destination: String, title: Option<String> },
    BlankLine,
    // Inline nodes:
    Text { text: String },
    Emphasis { children: Vec<AstNode> },
    Strong { children: Vec<AstNode> },
    Strikethrough { children: Vec<AstNode> },
    CodeSpan { text: String },
    Link { children: Vec<AstNode>, destination: String, title: Option<String> },
    Image { alt: String, destination: String, title: Option<String> },
    Math { content: String, display: bool },
    Autolink { url: String },
    HardBreak,
    RawHtml { content: String },
}

#[derive(Serialize, Deserialize)]
struct ListItemNode {
    task: Option<String>, // "open" | "done" | null
    children: Vec<AstNode>,
}

#[derive(Serialize, Deserialize)]
struct TableCellNode {
    align: String,
    children: Vec<AstNode>,
}

#[derive(Serialize, Deserialize)]
struct BlockInfo {
    index: usize,
    kind: String,
    source: String,
    start: u64,
    end: u64,
}

#[derive(Serialize, Deserialize)]
struct GitStatusInfo {
    branch: String,
    staged: Vec<GitFileEntry>,
    changes: Vec<GitFileEntry>,
    untracked: Vec<GitFileEntry>,
    conflicted: Vec<GitFileEntry>,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct GitFileEntry {
    path: String,
    status: String,
    old_path: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct GitBranchInfo {
    name: String,
    is_remote: bool,
    is_current: bool,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
}

#[derive(Serialize, Deserialize)]
struct GitDiffLine {
    kind: String, // "equal", "insert", "delete"
    old_no: Option<u32>,
    new_no: Option<u32>,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct GitHunkInfo {
    old_start: u32,
    new_start: u32,
    lines: Vec<GitDiffLine>,
}

#[derive(Serialize, Deserialize)]
struct GitFileDiff {
    path: String,
    old_path: Option<String>,
    hunks: Vec<GitHunkInfo>,
}

#[derive(Serialize, Deserialize)]
struct GitCommitInfo {
    sha: String,
    author: String,
    date: String,
    message: String,
}

#[derive(Serialize, Deserialize)]
struct GitCommitEntry {
    sha: String,
    short_sha: String,
    author: String,
    author_email: String,
    date: String,
    message: String,
    body: String,
    parents: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct GitStashInfo {
    index: usize,
    message: String,
    branch: String,
}

#[derive(Serialize, Deserialize)]
struct GitTagInfo {
    name: String,
    target: String,
    message: Option<String>,
    is_lightweight: bool,
}

#[derive(Serialize, Deserialize)]
struct GitRemoteInfo {
    name: String,
    url: String,
    fetch_url: String,
    push_url: String,
}

#[derive(Serialize, Deserialize)]
struct TabInfo {
    id: u64,
    file_name: String,
    file_path: Option<String>,
    is_dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct TabList {
    tabs: Vec<TabInfo>,
    active: u64,
}

// ---------------------------------------------------------------------------
// Tauri commands — tab management
// ---------------------------------------------------------------------------

/// Get a list of all open tabs and the active tab id.
#[tauri::command]
fn get_tabs(state: tauri::State<'_, Mutex<AppState>>) -> Result<TabList, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tabs: Vec<TabInfo> = s
        .tabs
        .iter()
        .map(|t| TabInfo {
            id: t.id,
            file_name: t.file_name(),
            file_path: t.file_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            is_dirty: t.buffer.is_dirty(),
        })
        .collect();
    let active = s.active_tab().map(|t| t.id).unwrap_or(0);
    Ok(TabList { tabs, active })
}

/// Switch to a tab by id.
#[tauri::command]
fn switch_tab(state: tauri::State<'_, Mutex<AppState>>, tab_id: u64) -> Result<DocumentInfo, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let idx = s.tabs.iter().position(|t| t.id == tab_id).ok_or("tab not found")?;
    s.active = idx;
    let tab = &s.tabs[idx];
    Ok(doc_info_from_tab(tab))
}

/// Close a tab by id. Returns the new active tab's info (or None if no tabs remain).
#[tauri::command]
fn close_tab(state: tauri::State<'_, Mutex<AppState>>, tab_id: u64) -> Result<Option<DocumentInfo>, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let idx = s.tabs.iter().position(|t| t.id == tab_id).ok_or("tab not found")?;
    let was_active = idx == s.active;
    s.tabs.remove(idx);
    // Adjust active index.
    if s.tabs.is_empty() {
        s.active = 0;
        return Ok(None);
    }
    if was_active {
        // Closed the active tab — pick the one now at the same position (or last).
        if s.active >= s.tabs.len() {
            s.active = s.tabs.len() - 1;
        }
    } else if idx < s.active {
        // Closed a tab before the active one — shift active index down.
        s.active -= 1;
    }
    let tab = &s.tabs[s.active];
    Ok(Some(doc_info_from_tab(tab)))
}

// ---------------------------------------------------------------------------
// Tauri commands — document operations (operate on the active tab)
// ---------------------------------------------------------------------------

/// Canonicalize a path for identity comparison; falls back to the input
/// unchanged when canonicalization fails (nonexistent file, permissions).
fn canonical_or_self(p: &PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.clone())
}

/// Index of the open tab whose `file_path` resolves to `canonical`, if any.
fn find_tab_idx_for_path(tabs: &[DocumentTab], canonical: &PathBuf) -> Option<usize> {
    tabs.iter().position(|t| {
        t.file_path
            .as_ref()
            .is_some_and(|p| canonical_or_self(p) == *canonical)
    })
}

/// Open a file from disk in a new tab.
#[tauri::command]
fn open_document(state: tauri::State<'_, Mutex<AppState>>, path: String) -> Result<DocumentInfo, String> {
    // Dedup: the same file in two tabs would create divergent buffers —
    // whichever saved last would silently clobber the other's edits.
    let canonical = canonical_or_self(&PathBuf::from(&path));
    {
        let mut s = state.lock().map_err(|e| e.to_string())?;
        if let Some(idx) = find_tab_idx_for_path(&s.tabs, &canonical) {
            s.active = idx;
            let tab = s.active_tab()?;
            return Ok(doc_info_from_tab(tab));
        }
    }
    // Use mmap-backed storage for zero-copy reads (Invariant 6: >RAM file support).
    let doc_id = DocumentId::new(&path);
    let storage = editor_storage::MmapStorage::open(&path, doc_id)
        .map_err(|e| e.to_string())?;
    let byte_source = storage.byte_source();
    let file_name = PathBuf::from(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let bytes = byte_source.as_bytes();
    let trailing_newline = bytes.last() == Some(&b'\n');
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new(&file_name),
        has_bom: bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        line_ending: detect_line_ending(bytes),
        trailing_newline,
        encoding: editor_domain::Encoding::Utf8,
    };
    let byte_len = byte_source.len();
    // For large files (>20MB), use lazy/chunked parsing: only parse the first
    // 10MB chunk. Additional chunks are parsed on demand via parse_next_chunk.
    // This avoids parsing 100+ MB at once (Invariant 6: >RAM files).
    const LAZY_THRESHOLD: usize = 20 * 1024 * 1024; // 20 MB
    const CHUNK_SIZE: usize = 10 * 1024 * 1024;     // 10 MB
    let buffer = if byte_len > LAZY_THRESHOLD {
        DocumentBuffer::open_lazy(byte_source, meta, MarkdownProfile::Gfm, CHUNK_SIZE)
            .map_err(|e| e.to_string())?
    } else {
        DocumentBuffer::open_from_buffer(byte_source, meta, MarkdownProfile::Gfm)
            .map_err(|e| e.to_string())?
    };
    let block_count = buffer.syntax().blocks.len();
    // For large documents, don't serialize the full text — frontend will
    // use get_syntax_tree_meta + get_block_data for virtualized rendering.
    // Threshold: 1 MB (avoid sending 100+ MB strings over IPC).
    let text = if byte_len > 1_048_576 {
        None
    } else {
        Some(String::from_utf8_lossy(&buffer.serialize()).to_string())
    };
    let mut s = state.lock().map_err(|e| e.to_string())?;
    // Re-check under the final lock: another open_document for the same file
    // could have raced past the earlier check while we were parsing.
    if let Some(idx) = find_tab_idx_for_path(&s.tabs, &canonical) {
        s.active = idx;
        let tab = s.active_tab()?;
        return Ok(doc_info_from_tab(tab));
    }
    let id = s.next_id;
    s.next_id += 1;
    s.push_tab(DocumentTab { id, buffer, file_path: Some(PathBuf::from(path)) });
    let tab = s.active_tab()?;
    Ok(DocumentInfo {
        text,
        file_name,
        is_dirty: tab.buffer.is_dirty(),
        block_count,
        byte_len,
        tab_id: id,
    })
}

/// Create a new empty document in a new tab.
#[tauri::command]
fn new_document(state: tauri::State<'_, Mutex<AppState>>) -> Result<DocumentInfo, String> {
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new("untitled"),
        has_bom: false,
        line_ending: editor_domain::LineEnding::Lf,
        // An empty buffer has no trailing newline; marking it `true` would lie
        // about document metadata until the first save.
        trailing_newline: false,
        encoding: editor_domain::Encoding::Utf8,
    };
    let buffer = DocumentBuffer::open(Vec::new(), meta, MarkdownProfile::Gfm)
        .map_err(|e| e.to_string())?;
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let id = s.next_id;
    s.next_id += 1;
    s.push_tab(DocumentTab { id, buffer, file_path: None });
    Ok(DocumentInfo {
        text: Some(String::new()),
        file_name: "untitled.md".to_string(),
        is_dirty: false,
        block_count: 0,
        byte_len: 0,
        tab_id: id,
    })
}

/// Open the built-in welcome/demo document in a new tab.
#[tauri::command]
fn open_welcome(state: tauri::State<'_, Mutex<AppState>>) -> Result<DocumentInfo, String> {
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new("welcome"),
        has_bom: false,
        line_ending: editor_domain::LineEnding::Lf,
        trailing_newline: true,
        encoding: editor_domain::Encoding::Utf8,
    };
    let buffer = DocumentBuffer::open(WELCOME_MD.as_bytes().to_vec(), meta, MarkdownProfile::Gfm)
        .map_err(|e| e.to_string())?;
    let mut s = state.lock().map_err(|e| e.to_string())?;
    // Dedup: the welcome doc is a singleton — re-opening it should switch to
    // the existing tab, not spawn an identical second one (file_path is None,
    // so the path-based dedup can't catch it).
    if let Some(idx) = s.tabs.iter().position(|t| {
        t.file_path.is_none() && t.buffer.meta.id == DocumentId::new("welcome")
    }) {
        s.active = idx;
        let tab = s.active_tab()?;
        return Ok(doc_info_from_tab(tab));
    }
    let id = s.next_id;
    s.next_id += 1;
    let tab = DocumentTab { id, buffer, file_path: None };
    s.push_tab(tab);
    Ok(doc_info_from_tab(s.tabs.last().ok_or_else(|| "no tab".to_string())?))
}

/// Get the current document text (serialized from the active buffer).
#[tauri::command]
fn get_document_text(state: tauri::State<'_, Mutex<AppState>>) -> Result<String, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(String::from_utf8_lossy(&tab.buffer.serialize()).to_string())
}

/// Shared byte-range replacement against a locked tab. Used by the public
/// replace/insert commands and internally by replace_match_in_document so the
/// match-verification and the edit happen under one lock (no TOCTOU window
/// where a concurrent edit could shift the offsets between them).
fn replace_text_in_tab(tab: &mut DocumentTab, start: u64, end: u64, new_text: &str) -> Result<EditResult, String> {
    if start > end || end > tab.buffer.len() {
        return Err(format!(
            "invalid range [{},{}) for document of {} bytes",
            start,
            end,
            tab.buffer.len()
        ));
    }
    // No-op guard: a byte-identical replacement is not an edit — it must not
    // record an undo step or dirty the buffer.
    if tab.buffer.serialize_range(start, end) == new_text.as_bytes() {
        return Ok(edit_result(tab, false));
    }
    let edit = TextEdit::replace(
        ByteRange::new(ByteOffset(start), ByteOffset(end)),
        new_text.as_bytes(),
    );
    let tx = EditTransaction::single(
        edit,
        editor_domain::Selection::caret(ByteOffset(start)),
        editor_domain::Selection::caret(ByteOffset(start + new_text.len() as u64)),
    );
    tab.buffer.apply(tx).map_err(|e| e.to_string())?;
    Ok(edit_result_from_tab(tab))
}

/// Replace a text range in the active document.
#[tauri::command]
fn replace_text(
    state: tauri::State<'_, Mutex<AppState>>,
    args: ReplaceTextArgs,
) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    replace_text_in_tab(tab, args.start, args.end, &args.new_text)
}

/// Insert text at a position in the active document.
#[tauri::command]
fn insert_text(
    state: tauri::State<'_, Mutex<AppState>>,
    position: u64,
    text: String,
) -> Result<EditResult, String> {
    replace_text(state, ReplaceTextArgs { start: position, end: position, new_text: text })
}

// ---------------------------------------------------------------------------
// Find / Find & Replace
// ---------------------------------------------------------------------------
// Search runs entirely in Rust, directly against the document buffer, and
// only ever returns match *offsets* (a few bytes each) to the frontend —
// never the document text itself. This matters: the frontend used to switch
// into an unvirtualized "source view" textarea just to have something to
// search, which meant a Find keystroke on a 10+ MB file serialized the
// entire document to a string and shipped it across the Tauri IPC boundary
// on every search. Replacement is symmetric: the frontend sends back byte
// offsets (from a prior search_document call) plus a replacement template,
// and Rust applies the edit(s) via the existing byte-range TextEdit path —
// the full document text never needs to round-trip through JS at all.

// Generous — a few hundred KB of (start, end) pairs is still a trivial IPC
// payload — but not unbounded, so a genuinely degenerate pattern (e.g. a
// zero-width match repeated once per byte of a 100+ MB file) can't produce
// an enormous JSON array.
const MAX_SEARCH_MATCHES: usize = 100_000;

#[derive(Serialize, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(rename = "caseSensitive")]
    case_sensitive: bool,
    regex: bool,
}

#[derive(Serialize, Deserialize)]
struct SearchMatch {
    start: u64,
    end: u64,
}

#[derive(Serialize, Deserialize)]
struct SearchResult {
    /// False when `regex` was requested but `query` is not a valid pattern.
    valid: bool,
    matches: Vec<SearchMatch>,
    /// True if more than MAX_SEARCH_MATCHES matched and the list was capped.
    truncated: bool,
}

/// Compile `query` into a `Regex`. Non-regex searches are compiled too (as an
/// escaped literal) so both modes share one matching/replacement path and
/// unicode-aware case-insensitive comparison always operates on the original
/// bytes — no separate `to_lowercase()` string ever gets built, which would
/// risk shifting byte offsets out of sync with the real document for the
/// (rare but real) Unicode characters whose lowercase form has a different
/// UTF-8 length than their original form.
fn compile_search_regex(query: &str, case_sensitive: bool, is_regex: bool) -> Result<Regex, regex::Error> {
    let pattern = if is_regex { query.to_string() } else { regex::escape(query) };
    let pattern = if case_sensitive { pattern } else { format!("(?i){pattern}") };
    Regex::new(&pattern)
}

/// Pure match-finding logic, factored out of the Tauri command for unit
/// testing without a running app / `tauri::State`.
fn find_matches_in_text(text: &str, query: &str, case_sensitive: bool, is_regex: bool) -> SearchResult {
    if query.is_empty() {
        return SearchResult { valid: true, matches: Vec::new(), truncated: false };
    }
    let re = match compile_search_regex(query, case_sensitive, is_regex) {
        Ok(re) => re,
        Err(_) => return SearchResult { valid: false, matches: Vec::new(), truncated: false },
    };
    let mut matches = Vec::new();
    let mut truncated = false;
    for m in re.find_iter(text) {
        if matches.len() >= MAX_SEARCH_MATCHES {
            truncated = true;
            break;
        }
        matches.push(SearchMatch { start: m.start() as u64, end: m.end() as u64 });
    }
    SearchResult { valid: true, matches, truncated }
}

/// Find every occurrence of `query` in the active document.
#[tauri::command]
fn search_document(state: tauri::State<'_, Mutex<AppState>>, args: SearchArgs) -> Result<SearchResult, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let bytes = tab.buffer.serialize();
    // Strict UTF-8 — `from_utf8_lossy` would substitute U+FFFD for invalid
    // sequences, shifting every subsequent byte offset and corrupting the
    // match ranges the frontend feeds back into replace commands.
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "document is not valid UTF-8 — search unavailable".to_string())?;
    Ok(find_matches_in_text(text, &args.query, args.case_sensitive, args.regex))
}

#[derive(Serialize, Deserialize)]
struct ReplaceMatchArgs {
    start: u64,
    end: u64,
    query: String,
    #[serde(rename = "caseSensitive")]
    case_sensitive: bool,
    regex: bool,
    replacement: String,
}

/// Expand `$1`, `$name`, `$$`, etc. in `template` using the regex captures
/// found by re-matching `query` against `matched_text` (the exact substring
/// at `[start, end)`). For non-regex searches the template is used literally
/// (no expansion — a literal replacement string should never be reinterpreted).
/// True when `text[start..end]` is still a match of `query` — i.e. a regex
/// search starting at `start` finds exactly `[start, end)`. Uses `find_at`
/// (not `find` on the slice) so `\b`/`\B` word boundaries see the real
/// document context, not artificial slice edges. Pure helper for tests.
fn range_still_matches(
    text: &str,
    start: usize,
    end: usize,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<bool, regex::Error> {
    let re = compile_search_regex(query, case_sensitive, is_regex)?;
    Ok(re
        .find_at(text, start)
        .is_some_and(|m| m.start() == start && m.end() == end))
}

fn expand_replacement(matched_text: &str, query: &str, case_sensitive: bool, is_regex: bool, template: &str) -> String {
    if !is_regex {
        return template.to_string();
    }
    let Ok(re) = compile_search_regex(query, case_sensitive, is_regex) else {
        return template.to_string();
    };
    match re.captures(matched_text) {
        Some(caps) => {
            let mut expanded = String::new();
            caps.expand(template, &mut expanded);
            expanded
        }
        None => template.to_string(),
    }
}

/// Replace a single, previously-found match (identified by its byte range)
/// with `replacement`, expanding regex capture-group references if `regex`
/// is set.
#[tauri::command]
fn replace_match_in_document(
    state: tauri::State<'_, Mutex<AppState>>,
    args: ReplaceMatchArgs,
) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let bytes = tab.buffer.serialize();
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "document is not valid UTF-8 — replace unavailable".to_string())?;
    let start = args.start as usize;
    let end = args.end as usize;
    if start > end || end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err("match range out of bounds — document changed, please search again".to_string());
    }
    // Verify the range still matches the query: the document may have
    // changed since the search that produced these offsets, and replacing
    // a stale range would destroy whatever bytes now sit there. The lock is
    // held through the replace_text_in_tab call below, so no concurrent edit
    // can invalidate the offsets between check and write.
    let still = range_still_matches(text, start, end, &args.query, args.case_sensitive, args.regex)
        .map_err(|_| "invalid regular expression".to_string())?;
    if !still {
        return Err("match no longer matches — document changed, please search again".to_string());
    }
    let matched = &text[start..end];
    let expanded = expand_replacement(matched, &args.query, args.case_sensitive, args.regex, &args.replacement);
    replace_text_in_tab(tab, args.start, args.end, &expanded)
}

#[derive(Serialize, Deserialize)]
struct ReplaceAllArgs {
    query: String,
    #[serde(rename = "caseSensitive")]
    case_sensitive: bool,
    regex: bool,
    replacement: String,
}

#[derive(Serialize, Deserialize)]
struct ReplaceAllResult {
    count: usize,
    edit: EditResult,
}

/// Replace every match of `query` with `replacement` in a single undo step.
///
/// Edits are applied last-match-first: `DocumentBuffer::apply` mutates the
/// piece table sequentially in the order given, so processing matches from
/// the end of the document backward means every not-yet-applied match's
/// original byte offsets stay valid throughout (nothing *before* the match
/// currently being edited is ever touched by a later-in-the-vec edit).
/// Processing in the opposite order would invalidate all earlier offsets
/// after the first edit whose replacement has a different length than the
/// match it replaced.
#[tauri::command]
fn replace_all_in_document(
    state: tauri::State<'_, Mutex<AppState>>,
    args: ReplaceAllArgs,
) -> Result<ReplaceAllResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let bytes = tab.buffer.serialize();
    // Borrow, don't copy — `.to_string()` here would allocate a second
    // full-document buffer (100+ MB) for no reason.
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "document is not valid UTF-8 — replace unavailable".to_string())?;
    let found = find_matches_in_text(text, &args.query, args.case_sensitive, args.regex);
    if !found.valid {
        return Err("invalid regular expression".to_string());
    }
    if found.matches.is_empty() {
        return Ok(ReplaceAllResult { count: 0, edit: edit_result(tab, false) });
    }

    let mut edits: Vec<TextEdit> = found
        .matches
        .iter()
        .rev() // last-match-first, see doc comment above
        .map(|m| {
            let start = m.start as usize;
            let end = m.end as usize;
            let replacement_text = expand_replacement(
                &text[start..end],
                &args.query,
                args.case_sensitive,
                args.regex,
                &args.replacement,
            );
            TextEdit::replace(
                ByteRange::new(ByteOffset(m.start), ByteOffset(m.end)),
                replacement_text.as_bytes(),
            )
        })
        .collect();
    // `edits` is currently last-to-first; keep that order (see doc comment).
    edits.shrink_to_fit();

    let count = found.matches.len();
    let last_match = found.matches.last().expect("checked non-empty above");
    let tx = EditTransaction::new(
        edits,
        editor_domain::Selection::caret(ByteOffset(last_match.start)),
        editor_domain::Selection::caret(ByteOffset(last_match.start)),
    );
    tab.buffer.apply(tx).map_err(|e| e.to_string())?;
    Ok(ReplaceAllResult { count, edit: edit_result_from_tab(tab) })
}

/// Get all blocks of the active document with their source text.
/// Used by the WYSIWYG block editor to render and edit blocks individually.
#[tauri::command]
fn get_blocks(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<BlockInfo>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    // Blocks only cover [0, parsed_offset) — serializing the whole buffer
    // would materialize the unparsed tail of a lazy (>RAM) document.
    let serialized = tab.buffer.serialize_range(0, tab.buffer.parsed_offset());
    let blocks: Vec<BlockInfo> = tab
        .buffer
        .syntax()
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let m = b.meta();
            let start = m.span.start.0 as usize;
            let end = m.span.end.0 as usize;
            // Slice raw bytes (not a &str) — a span that lands mid-char must
            // not panic the command.
            let source = if start <= end && end <= serialized.len() {
                String::from_utf8_lossy(&serialized[start..end]).to_string()
            } else {
                String::new()
            };
            BlockInfo {
                index: i,
                kind: block_kind_name(b),
                source,
                start: m.span.start.0,
                end: m.span.end.0,
            }
        })
        .collect();
    Ok(blocks)
}

/// Replace a single block's source text by block index.
/// This produces a minimal diff — only the block's byte range is replaced.
///
/// `expected_start`/`expected_end` pin the block identity: lazy chunk parsing
/// can merge/split the trailing block between `get_blocks` and this call,
/// shifting indices — a stale index would then overwrite a DIFFERENT block's
/// span. When the caller supplies the span it saw, a mismatch rejects the
/// write instead of corrupting another block.
#[tauri::command]
fn replace_block(
    state: tauri::State<'_, Mutex<AppState>>,
    block_index: usize,
    new_source: String,
    expected_start: Option<u64>,
    expected_end: Option<u64>,
) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    // Get the block's span before replacing.
    let span = {
        let blocks = &tab.buffer.syntax().blocks;
        let block = blocks.get(block_index).ok_or_else(|| "invalid block index".to_string())?;
        block.meta().span
    };
    if expected_start.is_some_and(|es| span.start.0 != es)
        || expected_end.is_some_and(|ee| span.end.0 != ee)
    {
        return Err("block moved — document changed, please retry the edit".to_string());
    }
    // No-op guard: replacing a block with byte-identical content must not
    // record an undo step or mark the buffer dirty (the commit paths call
    // this unconditionally on exit).
    if tab.buffer.serialize_range(span.start.0, span.end.0) == new_source.as_bytes() {
        return Ok(edit_result(tab, false));
    }
    let edit = TextEdit::replace(
        ByteRange::new(span.start, span.end),
        new_source.as_bytes(),
    );
    let tx = EditTransaction::single(
        edit,
        editor_domain::Selection::caret(span.start),
        editor_domain::Selection::caret(ByteOffset(span.start.0 + new_source.len() as u64)),
    );
    tab.buffer.apply(tx).map_err(|e| e.to_string())?;
    Ok(edit_result_from_tab(tab))
}

/// Undo the last edit in the active document.
#[tauri::command]
fn undo(state: tauri::State<'_, Mutex<AppState>>) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let had = tab.buffer.undo_manager().can_undo();
    tab.buffer.undo().map_err(|e| e.to_string())?;
    // An undo on an empty stack changed nothing — report `changed` accurately
    // so the frontend doesn't emit phantom dirty.
    Ok(edit_result(tab, had))
}

/// Redo the last undone edit in the active document.
#[tauri::command]
fn redo(state: tauri::State<'_, Mutex<AppState>>) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let had = tab.buffer.undo_manager().can_redo();
    tab.buffer.redo().map_err(|e| e.to_string())?;
    Ok(edit_result(tab, had))
}

/// Resolve which path a save should target: an explicit path (e.g. from a
/// "Save As" dialog) takes precedence over the tab's existing file path.
/// Pulled out as a pure function so the decision logic is unit-testable
/// without a `tauri::State`/running app (see `tests::resolve_save_path_*`).
fn resolve_save_path(explicit: Option<PathBuf>, tab_path: Option<&PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| tab_path.cloned())
}

/// Save the active document to disk.
#[tauri::command]
fn save_document(
    state: tauri::State<'_, Mutex<AppState>>,
    path: Option<String>,
) -> Result<bool, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let bytes = tab.buffer.serialize();
    let save_path = resolve_save_path(path.map(PathBuf::from), tab.file_path.as_ref());
    let save_path = save_path.ok_or(
        "no file path to save to — the frontend must prompt for one (untitled document)",
    )?;
    editor_storage::atomic_save(&save_path, &bytes).map_err(|e| e.to_string())?;
    tab.buffer.mark_saved();
    tab.file_path = Some(save_path);
    Ok(true)
}

/// Get the syntax tree for the active document (full — includes source text and AST).
/// Used for small documents. For large documents, use get_syntax_tree_meta + get_block_data.
#[tauri::command]
fn get_syntax_tree(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<SyntaxBlock>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    // For lazy docs only [0, parsed_offset) is parsed — copy just that prefix
    // (all block spans end at or before parsed_offset) instead of
    // materializing the whole, possibly >RAM, buffer.
    let parsed_len = tab.buffer.parsed_offset();
    let parsed_text = tab.buffer.serialize_range(0, parsed_len);
    let text_str = String::from_utf8_lossy(&parsed_text).to_string();
    let blocks: Vec<SyntaxBlock> = tab
        .buffer
        .syntax()
        .blocks
        .iter()
        .map(|b| {
            let m = b.meta();
            let start = m.span.start.0 as usize;
            let end = m.span.end.0 as usize;
            let source = if start <= end && end <= parsed_text.len() {
                String::from_utf8_lossy(&parsed_text[start..end]).to_string()
            } else {
                String::new()
            };
            SyntaxBlock {
                kind: block_kind_name(b),
                source,
                start: m.span.start.0,
                end: m.span.end.0,
                node: block_to_ast(b, &text_str),
            }
        })
        .collect();
    Ok(blocks)
}

/// Get lightweight block metadata (kind + byte span) without source text or AST.
/// Used for virtualized rendering of large documents — the frontend requests
/// full block data only for visible blocks via get_block_data.
#[tauri::command]
fn get_syntax_tree_meta(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<SyntaxBlockMeta>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let blocks: Vec<SyntaxBlockMeta> = tab
        .buffer
        .syntax()
        .blocks
        .iter()
        .map(|b| {
            let m = b.meta();
            SyntaxBlockMeta {
                kind: block_kind_name(b),
                start: m.span.start.0,
                end: m.span.end.0,
            }
        })
        .collect();
    Ok(blocks)
}

/// How many bytes of the document have been parsed so far.
/// For fully-parsed documents, this equals the document length.
/// For lazy/chunked documents, bytes beyond this offset have not been parsed yet.
#[tauri::command]
fn get_parsed_offset(state: tauri::State<'_, Mutex<AppState>>) -> Result<(u64, u64), String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok((tab.buffer.parsed_offset(), tab.buffer.total_len()))
}

/// Parse the next chunk of a large document (10 MB).
/// Returns (parsed_offset, total_len, block_count) after parsing.
/// The frontend calls this when the user scrolls near the end of the currently
/// parsed region to trigger on-demand parsing of the next chunk.
#[tauri::command]
fn parse_next_chunk(state: tauri::State<'_, Mutex<AppState>>) -> Result<(u64, u64, usize), String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let offset = tab.buffer.parsed_offset();
    let total = tab.buffer.total_len();
    if offset >= total {
        return Ok((offset, total, tab.buffer.syntax().blocks.len()));
    }
    const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10 MB
    let new_offset = tab.buffer.parse_next_chunk(offset, CHUNK_SIZE)
        .map_err(|e| e.to_string())?;
    let block_count = tab.buffer.syntax().blocks.len();
    Ok((new_offset, total, block_count))
}

/// Get full block data (source + AST) for a specific block by index.
/// Used for on-demand loading of visible blocks in virtualized rendering.
#[tauri::command]
fn get_block_data(
    state: tauri::State<'_, Mutex<AppState>>,
    block_index: usize,
) -> Result<Option<SyntaxBlock>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let blocks = &tab.buffer.syntax().blocks;
    let b = match blocks.get(block_index) {
        Some(b) => b,
        None => return Ok(None),
    };
    let m = b.meta();
    let start = m.span.start.0;
    let end = m.span.end.0;
    // Extract only this block's byte range — avoids serializing the entire
    // 100+ MB document for each block request.
    let range_bytes = tab.buffer.serialize_range(start, end);
    let source = String::from_utf8_lossy(&range_bytes).to_string();
    // For AST rendering, block_to_ast uses span_text() with absolute byte offsets
    // from the block's meta.span. Since we only have the block's local bytes (not
    // the full document), we must rebase offsets to be relative to the block start.
    // block_to_ast_relative subtracts `start` from all span accesses.
    let node = block_to_ast_relative(b, &source, start);
    Ok(Some(SyntaxBlock {
        kind: block_kind_name(b),
        source,
        start: m.span.start.0,
        end: m.span.end.0,
        node,
    }))
}

/// Get the starting line number (1-indexed) for a single block.
/// Uses the pre-computed `PieceTable::line_starts` for O(log N) lookup.
#[tauri::command]
fn get_block_line_number(
    state: tauri::State<'_, Mutex<AppState>>,
    block_index: usize,
) -> Result<u32, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let blocks = &tab.buffer.syntax().blocks;
    let b = match blocks.get(block_index) {
        Some(b) => b,
        None => return Ok(1),
    };
    let start = b.meta().span.start.0;
    let line_starts = tab.buffer.text().line_starts();
    Ok(line_number_at_offset(line_starts, start))
}

/// Get line numbers (1-indexed) for a range of blocks in one call.
/// Uses the pre-computed `PieceTable::line_starts` for O(count * log N) lookup.
/// No byte copies — suitable for 100+ MB files.
#[tauri::command]
fn get_block_line_numbers(
    state: tauri::State<'_, Mutex<AppState>>,
    start_index: usize,
    count: usize,
) -> Result<Vec<u32>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let blocks = &tab.buffer.syntax().blocks;
    // `start_index + count` can overflow usize for a degenerate `count`.
    let end_index = start_index.saturating_add(count).min(blocks.len());
    if start_index >= end_index {
        return Ok(Vec::new());
    }
    let line_starts = tab.buffer.text().line_starts();
    let mut result = Vec::with_capacity(end_index - start_index);
    for i in start_index..end_index {
        let start = blocks[i].meta().span.start.0;
        result.push(line_number_at_offset(line_starts, start));
    }
    Ok(result)
}

/// 1-indexed line number for a byte offset, using a sorted `line_starts` table.
fn line_number_at_offset(line_starts: &[u64], offset: u64) -> u32 {
    match line_starts.binary_search(&offset) {
        Ok(i) => (i + 1) as u32,
        Err(i) => i as u32,
    }
    .max(1)
}

/// Get git status for the active document's repository.
#[tauri::command]
fn get_git_status(state: tauri::State<'_, Mutex<AppState>>) -> Result<GitStatusInfo, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let status = git.status().map_err(|e| e.to_string())?;
    Ok(GitStatusInfo {
        branch: status.head_branch.unwrap_or_default(),
        staged: status.staged.iter().map(|f| file_change_to_entry(f)).collect(),
        changes: status.changes.iter().map(|f| file_change_to_entry(f)).collect(),
        untracked: status.untracked.iter().map(|f| file_change_to_entry(f)).collect(),
        conflicted: status.conflicted.iter().map(|f| file_change_to_entry(f)).collect(),
        dirty: status.dirty,
    })
}

/// List all branches (local + remote) with ahead/behind info.
#[tauri::command]
fn git_branches(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitBranchInfo>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let status = git.status().map_err(|e| e.to_string())?;
    let current = status.head_branch.unwrap_or_default();
    let branches = git.branches().map_err(|e| e.to_string())?;
    Ok(branches.iter().map(|b| GitBranchInfo {
        name: b.name.clone(),
        is_remote: b.is_remote,
        is_current: b.name == current,
        upstream: b.upstream.clone(),
        ahead: b.ahead,
        behind: b.behind,
    }).collect())
}

/// Get diff between working tree and HEAD for all files.
#[tauri::command]
fn git_diff(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitFileDiff>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let diffs = git.diff(editor_git::DiffRequest::WorkingTreeVsHead).map_err(|e| e.to_string())?;
    Ok(diffs.iter().map(|fd| GitFileDiff {
        path: fd.path.clone(),
        old_path: fd.old_path.clone(),
        hunks: fd.hunks.iter().map(|h| GitHunkInfo {
            old_start: h.old_start,
            new_start: h.new_start,
            lines: h.lines.iter().map(|lc| line_change_to_info(lc)).collect(),
        }).collect(),
    }).collect())
}

/// Convert a repo-root-relative path to a git pathspec that works from any
/// subdirectory AND matches literally. `:/` alone anchors at the root but still
/// applies glob matching — `:(top,literal)` anchors AND treats `*?[...]` as
/// ordinary characters, so files whose names contain metacharacters work.
fn root_pathspec(path: &str) -> String {
    if path.starts_with(':') {
        path.to_string()
    } else {
        format!(":(top,literal){path}")
    }
}

/// Get unified diff for a single file (working tree vs HEAD) with parsed hunks and lines.
#[tauri::command]
fn git_diff_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    let text = git.diff_file_raw(&ps).map_err(|e| e.to_string())?;
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Get unified diff for a single file between two commits.
#[tauri::command]
fn git_diff_file_commits(state: tauri::State<'_, Mutex<AppState>>, file_path: String, commit_a: String, commit_b: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    let text = git.diff_file_commits_raw(&ps, &commit_a, &commit_b).map_err(|e| e.to_string())?;
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Get unified diff for a single file between working tree and a commit.
#[tauri::command]
fn git_diff_file_vs_commit(state: tauri::State<'_, Mutex<AppState>>, file_path: String, commit: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    let text = git.diff_file_vs_commit_raw(&ps, &commit).map_err(|e| e.to_string())?;
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Discard changes to a file (restore from HEAD). Equivalent to `git checkout -- <file>`.
#[tauri::command]
fn git_discard_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    git.discard_file(&ps).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Remove an untracked file (delete from disk).
#[tauri::command]
fn git_remove_untracked(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    git.clean_files(&ps).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Stage all changes.
#[tauri::command]
fn git_stage_all(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    git.stage(editor_git::ChangeSelection::All).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Stage a single file.
#[tauri::command]
fn git_stage_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    git.stage(editor_git::ChangeSelection::File { path: file_path }).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Unstage a single file.
#[tauri::command]
fn git_unstage_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    git.unstage(editor_git::ChangeSelection::File { path: file_path }).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Commit staged changes.
#[tauri::command]
fn git_commit(state: tauri::State<'_, Mutex<AppState>>, message: String) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("commit message cannot be empty".to_string());
    }
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let commit_id = git.commit(editor_git::CommitRequest { message, amend: false }).map_err(|e| e.to_string())?;
    Ok(commit_id.0)
}

/// Checkout a branch.
#[tauri::command]
fn git_checkout(state: tauri::State<'_, Mutex<AppState>>, branch: String) -> Result<bool, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    git.checkout(editor_git::Revision::Branch(branch)).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Get file history (commit log for the current file).
#[tauri::command]
fn git_file_history(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitCommitInfo>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    // History needs the repo-root-relative path — the bare file name only
    // matches files at the top level, silently returning empty history for
    // anything in a subdirectory.
    let root = git.repo_root().map_err(|e| e.to_string())?;
    let rel = path
        .strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let history = editor_git::file_history(&git, &rel).map_err(|e| e.to_string())?;
    Ok(history.iter().map(|e| GitCommitInfo {
        sha: e.revision.0.clone(),
        author: e.author.clone(),
        date: e.date.clone(),
        message: e.message.clone(),
    }).collect())
}

/// Get full commit log (repository history).
#[tauri::command]
fn git_log(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitCommitInfo>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let path = tab.file_path.as_ref().ok_or("no file open")?;
    let dir = path.parent().ok_or("no parent directory")?;
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let entries = git.log(50).map_err(|e| e.to_string())?;
    let commits = entries.into_iter().map(|c| GitCommitInfo {
        sha: c.sha,
        author: c.author,
        date: c.date,
        message: c.message,
    }).collect();
    Ok(commits)
}

// ---------------------------------------------------------------------------
// Tauri commands — extended Git operations
// ---------------------------------------------------------------------------

/// Helper to open git for the active tab's directory.
fn open_git_for_active(state: &tauri::State<'_, Mutex<AppState>>) -> Result<editor_git::GitCli, String> {
    let dir = {
        let s = state.lock().map_err(|e| e.to_string())?;
        let tab = s.active_tab()?;
        let path = tab.file_path.as_ref().ok_or("no file open")?;
        let dir = path.parent().ok_or("no parent directory")?;
        dir.to_path_buf()
    };
    editor_git::GitCli::open(dir).map_err(|e| e.to_string())
}

/// Diff between two commits.
#[tauri::command]
fn git_diff_commits(state: tauri::State<'_, Mutex<AppState>>, commit_a: String, commit_b: String) -> Result<Vec<GitFileDiff>, String> {
    let git = open_git_for_active(&state)?;
    let diffs = git.diff_commits(&commit_a, &commit_b).map_err(|e| e.to_string())?;
    Ok(diffs.iter().map(|fd| GitFileDiff {
        path: fd.path.clone(),
        old_path: fd.old_path.clone(),
        hunks: fd.hunks.iter().map(|h| GitHunkInfo {
            old_start: h.old_start,
            new_start: h.new_start,
            lines: h.lines.iter().map(|lc| line_change_to_info(lc)).collect(),
        }).collect(),
    }).collect())
}

/// Diff between working tree and a specific commit.
#[tauri::command]
fn git_diff_vs_commit(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<Vec<GitFileDiff>, String> {
    let git = open_git_for_active(&state)?;
    let diffs = git.diff_working_tree_vs_commit(&commit).map_err(|e| e.to_string())?;
    Ok(diffs.iter().map(|fd| GitFileDiff {
        path: fd.path.clone(),
        old_path: fd.old_path.clone(),
        hunks: fd.hunks.iter().map(|h| GitHunkInfo {
            old_start: h.old_start,
            new_start: h.new_start,
            lines: h.lines.iter().map(|lc| line_change_to_info(lc)).collect(),
        }).collect(),
    }).collect())
}

/// Get detailed commit log (with short SHA, body, parents).
#[tauri::command]
fn git_log_detailed(state: tauri::State<'_, Mutex<AppState>>, max_count: Option<usize>) -> Result<Vec<GitCommitEntry>, String> {
    let git = open_git_for_active(&state)?;
    let log = git.log(max_count.unwrap_or(100)).map_err(|e| e.to_string())?;
    Ok(log.iter().map(|c| GitCommitEntry {
        sha: c.sha.clone(),
        short_sha: c.short_sha.clone(),
        author: c.author.clone(),
        author_email: c.author_email.clone(),
        date: c.date.clone(),
        message: c.message.clone(),
        body: c.body.clone(),
        parents: c.parents.clone(),
    }).collect())
}

/// Stash operations.
#[tauri::command]
fn git_stash_list(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitStashInfo>, String> {
    let git = open_git_for_active(&state)?;
    let stashes = git.stash_list().map_err(|e| e.to_string())?;
    Ok(stashes.iter().map(|s| GitStashInfo {
        index: s.index,
        message: s.message.clone(),
        branch: s.branch.clone(),
    }).collect())
}

#[tauri::command]
fn git_stash_push(state: tauri::State<'_, Mutex<AppState>>, message: Option<String>) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.stash_push(message.as_deref()).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_stash_pop(state: tauri::State<'_, Mutex<AppState>>, index: usize) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.stash_pop(index).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_stash_apply(state: tauri::State<'_, Mutex<AppState>>, index: usize) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.stash_apply(index).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_stash_drop(state: tauri::State<'_, Mutex<AppState>>, index: usize) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.stash_drop(index).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Branch management.
#[tauri::command]
fn git_create_branch(state: tauri::State<'_, Mutex<AppState>>, name: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.create_branch(&name).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_delete_branch(state: tauri::State<'_, Mutex<AppState>>, name: String, force: bool) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.delete_branch(&name, force).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_rename_branch(state: tauri::State<'_, Mutex<AppState>>, old_name: String, new_name: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.rename_branch(&old_name, &new_name).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Merge.
#[tauri::command]
fn git_merge(state: tauri::State<'_, Mutex<AppState>>, branch: String, strategy: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    let strat = match strategy.as_str() {
        "ff-only" => editor_git::MergeStrategy::FastForwardOnly,
        "no-ff" => editor_git::MergeStrategy::NoFastForward,
        "squash" => editor_git::MergeStrategy::Squash,
        _ => editor_git::MergeStrategy::Merge,
    };
    git.merge(&branch, strat).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_merge_abort(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.merge_abort().map_err(|e| e.to_string())?;
    Ok(true)
}

/// Rebase.
#[tauri::command]
fn git_rebase(state: tauri::State<'_, Mutex<AppState>>, branch: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.rebase(&branch).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_rebase_abort(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.rebase_abort().map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_rebase_continue(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.rebase_continue().map_err(|e| e.to_string())?;
    Ok(true)
}

/// Cherry-pick / Revert.
#[tauri::command]
fn git_cherry_pick(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.cherry_pick(&commit).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_revert(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.revert(&commit).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Tags.
#[tauri::command]
fn git_tags(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitTagInfo>, String> {
    let git = open_git_for_active(&state)?;
    let tags = git.tags().map_err(|e| e.to_string())?;
    Ok(tags.iter().map(|t| GitTagInfo {
        name: t.name.clone(),
        target: t.target.clone(),
        message: t.message.clone(),
        is_lightweight: t.is_lightweight,
    }).collect())
}

#[tauri::command]
fn git_create_tag(state: tauri::State<'_, Mutex<AppState>>, name: String, message: Option<String>) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.create_tag(&name, message.as_deref()).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_delete_tag(state: tauri::State<'_, Mutex<AppState>>, name: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.delete_tag(&name).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Remotes.
#[tauri::command]
fn git_remotes(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GitRemoteInfo>, String> {
    let git = open_git_for_active(&state)?;
    let remotes = git.remotes().map_err(|e| e.to_string())?;
    Ok(remotes.iter().map(|r| GitRemoteInfo {
        name: r.name.clone(),
        url: r.url.clone(),
        fetch_url: r.fetch_url.clone(),
        push_url: r.push_url.clone(),
    }).collect())
}

#[tauri::command]
fn git_add_remote(state: tauri::State<'_, Mutex<AppState>>, name: String, url: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.add_remote(&name, &url).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_push_to_remote(state: tauri::State<'_, Mutex<AppState>>, remote: String, branch: String, force: bool) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.push_to_remote(&remote, &branch, force).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_pull_from_remote(state: tauri::State<'_, Mutex<AppState>>, remote: String, branch: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.pull_from_remote(&remote, &branch).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_fetch_remote(state: tauri::State<'_, Mutex<AppState>>, remote: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.fetch_remote(&remote).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Reset.
#[tauri::command]
fn git_reset_soft(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.reset_soft(&commit).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_reset_mixed(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.reset_mixed(&commit).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
fn git_reset_hard(state: tauri::State<'_, Mutex<AppState>>, commit: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.reset_hard(&commit).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Clean untracked files.
#[tauri::command]
fn git_clean(state: tauri::State<'_, Mutex<AppState>>, directories: bool, force: bool) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    git.clean(directories, force).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Get current branch name.
#[tauri::command]
fn git_current_branch(state: tauri::State<'_, Mutex<AppState>>) -> Result<String, String> {
    let git = open_git_for_active(&state)?;
    git.current_branch().map_err(|e| e.to_string())
}

/// Get HEAD commit id.
#[tauri::command]
fn git_head_commit(state: tauri::State<'_, Mutex<AppState>>) -> Result<String, String> {
    let git = open_git_for_active(&state)?;
    let id = git.head_commit().map_err(|e| e.to_string())?;
    Ok(id.0)
}

/// Read a file at a specific revision.
#[tauri::command]
fn git_read_file_at_revision(state: tauri::State<'_, Mutex<AppState>>, file_path: String, revision: String) -> Result<String, String> {
    let git = open_git_for_active(&state)?;
    if revision.is_empty() {
        // `file_path` is repo-relative; reject absolute paths and `..`
        // components so the read can't escape the repository root.
        let rel = std::path::Path::new(&file_path);
        if rel.is_absolute()
            || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("file_path must be a repo-relative path without '..'".to_string());
        }
        let root = git.repo_root().map_err(|e| e.to_string())?;
        let full = std::path::Path::new(&root).join(&file_path);
        let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    } else {
        let bytes = editor_git::read_file_at_revision(&git, &file_path, &revision).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }
}

/// Check if the active document has unsaved changes.
#[tauri::command]
fn is_dirty(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(tab.buffer.is_dirty())
}

/// Get the file name of the active document.
#[tauri::command]
fn get_file_name(state: tauri::State<'_, Mutex<AppState>>) -> Result<String, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(tab.file_name())
}

/// Get the absolute path of the active document (or None if unsaved).
#[tauri::command]
fn get_active_file_path(state: tauri::State<'_, Mutex<AppState>>) -> Result<Option<String>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(tab.file_path.as_ref().map(|p| p.to_string_lossy().to_string()))
}

/// Get the git repository root directory (absolute path) for the active document.
#[tauri::command]
fn git_repo_root(state: tauri::State<'_, Mutex<AppState>>) -> Result<Option<String>, String> {
    let git = open_git_for_active(&state);
    match git {
        Ok(g) => {
            let root = g.work_dir().to_string_lossy().to_string();
            Ok(Some(root))
        }
        Err(_) => Ok(None),
    }
}

// ── GitHub integration (via gh CLI) ────────────────────────────────────────
// We shell out to `gh` directly (like editor-git shells out to `git`).
// This avoids depending on the adapters/github crate (Invariant 3,4).

/// Run `gh` in the active document's directory and return stdout.
/// Run a subprocess with a deadline: spawn with piped output drained on reader
/// threads (a full pipe buffer would otherwise block a healthy child and look
/// like a hang), poll `try_wait`, kill on timeout. `Command::output()` has no
/// timeout — a wedged network op (`gh`, credential prompt) hung the command
/// forever.
fn run_subprocess(
    bin: &str,
    dir: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(bin)
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} not available: {}", bin, e))?;
    let mut out_pipe = child.stdout.take().expect("piped");
    let mut err_pipe = child.stderr.take().expect("piped");
    let out_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let err_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_pipe.read_to_end(&mut v);
        v
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(format!("{} timed out", bin));
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

fn gh_exec_text(state: &tauri::State<'_, Mutex<AppState>>, args: &[&str]) -> Result<String, String> {
    let dir = {
        let s = state.lock().map_err(|e| e.to_string())?;
        let tab = s.active_tab()?;
        let path = tab.file_path.as_ref().ok_or("no file open")?;
        let dir = path.parent().ok_or("no parent directory")?;
        dir.to_path_buf()
    };
    let gh_bin = std::env::var("GH_BIN").unwrap_or_else(|_| "gh".to_string());
    let out = run_subprocess(&gh_bin, &dir, args, std::time::Duration::from_secs(60))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(stderr)
    }
}

/// GitHub auth status response.
#[derive(Serialize, Deserialize)]
struct GithubAuthStatus {
    authenticated: bool,
    user: String,
}

/// GitHub repo metadata response.
#[derive(Serialize, Deserialize)]
struct GithubRepoMetadata {
    full_name: String,
    default_branch: String,
    html_url: String,
}

/// GitHub PR response.
#[derive(Serialize, Deserialize)]
struct GithubPullRequest {
    number: i64,
    title: String,
    state: String,
    html_url: String,
}

/// GitHub remote branch response.
#[derive(Serialize, Deserialize)]
struct GithubRemoteBranch {
    name: String,
}

/// Check GitHub auth status via `gh auth status`.
#[tauri::command]
fn github_auth_status(state: tauri::State<'_, Mutex<AppState>>) -> Result<GithubAuthStatus, String> {
    let text = gh_exec_text(&state, &["auth", "status"]);
    match text {
        Ok(t) => {
            // Parse "Logged in to github.com as <user>" or similar.
            let user = t
                .lines()
                .find_map(|l| {
                    if l.contains("Logged in") {
                        // Try "account <user>" or "as <user>"
                        if let Some(idx) = l.find("account ") {
                            Some(l[idx + 8..].split(' ').next().unwrap_or("").trim().to_string())
                        } else if let Some(idx) = l.find("as ") {
                            Some(l[idx + 3..].split(' ').next().unwrap_or("").trim().to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            Ok(GithubAuthStatus { authenticated: true, user })
        }
        Err(_) => Ok(GithubAuthStatus { authenticated: false, user: String::new() }),
    }
}

/// Login via `gh auth login` (interactive — opens browser).
#[tauri::command]
fn github_login(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    // gh auth login is interactive — we can't run it in a non-interactive context.
    // Instead, we'll try `gh auth login --web` which opens a browser.
    // If that fails, we tell the user to run it manually.
    let _dir = {
        let s = state.lock().map_err(|e| e.to_string())?;
        let tab = s.active_tab()?;
        let path = tab.file_path.as_ref().ok_or("no file open")?;
        let dir = path.parent().ok_or("no parent directory")?;
        dir.to_path_buf()
    };
    let gh_bin = std::env::var("GH_BIN").unwrap_or_else(|_| "gh".to_string());
    // Check if gh is installed.
    let check = std::process::Command::new(&gh_bin).arg("--version").output();
    if check.is_err() {
        return Err("gh CLI is not installed. Please install it from https://cli.github.com/".to_string());
    }
    // Open a terminal-like prompt — we can't do interactive login from Tauri.
    // Instead, instruct the user.
    Err("Please run 'gh auth login' in a terminal to authenticate with GitHub.".to_string())
}

/// Logout via `gh auth logout`.
#[tauri::command]
fn github_logout(state: tauri::State<'_, Mutex<AppState>>) -> Result<bool, String> {
    let _ = gh_exec_text(&state, &["auth", "logout", "--yes"])?;
    Ok(true)
}

/// Get repository metadata via `gh repo view`.
#[tauri::command]
fn github_repo_metadata(state: tauri::State<'_, Mutex<AppState>>) -> Result<GithubRepoMetadata, String> {
    let text = gh_exec_text(&state, &["repo", "view", "--json", "nameWithOwner,defaultBranchRef,url"])?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("invalid gh JSON: {e}"))?;
    Ok(GithubRepoMetadata {
        full_name: v["nameWithOwner"].as_str().unwrap_or("").to_string(),
        default_branch: v["defaultBranchRef"]["name"].as_str().unwrap_or("").to_string(),
        html_url: v["url"].as_str().unwrap_or("").to_string(),
    })
}

/// List pull requests via `gh pr list`.
#[tauri::command]
fn github_pull_requests(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GithubPullRequest>, String> {
    let text = gh_exec_text(&state, &["pr", "list", "--json", "number,title,state,url", "--limit", "30"])?;
    // gh emits a compact JSON array on ONE line — real JSON parsing is
    // required; the old per-line + substring approach only ever saw the
    // first PR and broke on escaped quotes in titles.
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("invalid gh JSON: {e}"))?;
    let mut prs = Vec::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            prs.push(GithubPullRequest {
                number: item["number"].as_i64().unwrap_or(0),
                title: item["title"].as_str().unwrap_or("").to_string(),
                state: item["state"].as_str().unwrap_or("").to_string(),
                html_url: item["url"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    Ok(prs)
}

/// List remote branches via `git branch -r`.
#[tauri::command]
fn github_remote_branches(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<GithubRemoteBranch>, String> {
    let git = open_git_for_active(&state)?;
    let names = git.remote_branches().map_err(|e| e.to_string())?;
    let branches = names.into_iter().map(|name| GithubRemoteBranch { name }).collect();
    Ok(branches)
}

/// Open a file by relative path (resolved against the active document's directory).
/// Used for `[link](other.md)` navigation — opens in a new tab.
#[tauri::command]
fn open_relative_file(
    state: tauri::State<'_, Mutex<AppState>>,
    relative_path: String,
) -> Result<DocumentInfo, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let active = s.active_tab()?;
    let base_dir = active.file_path.as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or("no base directory")?;
    let resolved = base_dir.join(&relative_path);
    drop(s);

    // Security: prevent path traversal — the resolved path must stay within
    // base_dir. `canonicalize` must succeed: a non-canonicalized path keeps
    // `..` segments which `starts_with` compares literally, letting
    // `base/../outside` pass as "inside".
    let path = resolved
        .canonicalize()
        .map_err(|_| format!("cannot resolve path: {}", relative_path))?;
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|_| "cannot resolve base directory".to_string())?;
    if !path.starts_with(&canonical_base) {
        return Err("path traversal denied: resolved path is outside the base directory".to_string());
    }
    let doc_id = DocumentId::new(path.to_string_lossy().as_ref());
    let storage = editor_storage::MmapStorage::open(&path, doc_id)
        .map_err(|e| e.to_string())?;
    let byte_source = storage.byte_source();
    let file_name = path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let bytes = byte_source.as_bytes();
    let trailing_newline = bytes.last() == Some(&b'\n');
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new(&file_name),
        has_bom: bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        line_ending: detect_line_ending(bytes),
        trailing_newline,
        encoding: editor_domain::Encoding::Utf8,
    };
    let byte_len = byte_source.len();
    const LAZY_THRESHOLD: usize = 20 * 1024 * 1024;
    const CHUNK_SIZE: usize = 10 * 1024 * 1024;
    let buffer = if byte_len > LAZY_THRESHOLD {
        DocumentBuffer::open_lazy(byte_source, meta, MarkdownProfile::Gfm, CHUNK_SIZE)
            .map_err(|e| e.to_string())?
    } else {
        DocumentBuffer::open_from_buffer(byte_source, meta, MarkdownProfile::Gfm)
            .map_err(|e| e.to_string())?
    };

    let mut s = state.lock().map_err(|e| e.to_string())?;
    // Same dedup as open_document: `path` is already canonicalized, so a
    // link to a file that's already open must activate its tab rather than
    // fork a second, divergent buffer.
    if let Some(idx) = find_tab_idx_for_path(&s.tabs, &path) {
        s.active = idx;
        let tab = s.active_tab()?;
        return Ok(doc_info_from_tab(tab));
    }
    let id = s.next_id;
    s.next_id += 1;
    let tab = DocumentTab {
        id,
        buffer,
        file_path: Some(path.clone()),
    };
    let info = doc_info_from_tab(&tab);
    s.push_tab(tab);
    Ok(info)
}

// ── File tree commands ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct FileTreeEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
}

/// List directory contents (non-recursive). Returns folders first, then files.
#[tauri::command]
fn list_directory(dir_path: String) -> Result<Vec<FileTreeEntry>, String> {
    let path = std::path::Path::new(&dir_path);
    if !path.exists() {
        return Err(format!("Path does not exist: {}", dir_path));
    }
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", dir_path));
    }
    let mut entries = Vec::new();
    let read = std::fs::read_dir(path).map_err(|e| e.to_string())?;
    for entry in read {
        let entry = entry.map_err(|e| e.to_string())?;
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        // Skip hidden files/dirs (starting with '.') on all platforms.
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        entries.push(FileTreeEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir: file_type.is_dir(),
            size,
        });
    }
    // Sort: directories first, then alphabetically.
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Get the parent directory of a path.
#[tauri::command]
fn get_parent_dir(dir_path: String) -> Result<Option<String>, String> {
    let path = std::path::Path::new(&dir_path);
    Ok(path.parent().map(|p| p.to_string_lossy().to_string()))
}

/// Copy a file or directory to a destination.
#[tauri::command]
fn copy_file(src_path: String, dest_path: String) -> Result<bool, String> {
    let src = std::path::Path::new(&src_path);
    let dest = std::path::Path::new(&dest_path);
    if !src.exists() {
        return Err(format!("Source does not exist: {}", src_path));
    }
    if dest.exists() {
        return Err(format!("Destination already exists: {}", dest_path));
    }
    if src.is_dir() {
        // Copying a directory into itself would recurse: `read_dir` can observe
        // the freshly-created destination during iteration. Canonicalize the
        // destination's parent (dest itself doesn't exist yet) and reject any
        // destination that resolves inside the source tree.
        let canonical_src = src.canonicalize().map_err(|e| e.to_string())?;
        let canonical_dest = dest
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(dest.file_name().unwrap_or_default()))
            .unwrap_or_else(|| dest.to_path_buf());
        if canonical_dest.starts_with(&canonical_src) {
            return Err("cannot copy a directory into itself".to_string());
        }
        copy_dir_recursive(&canonical_src, &canonical_dest).map_err(|e| e.to_string())?;
    } else {
        std::fs::copy(src, dest).map_err(|e| e.to_string())?;
    }
    Ok(true)
}

/// Move/rename a file or directory. Open tabs whose file lives at the moved
/// path (or under a moved directory) are repointed at the new location so a
/// later save does not silently recreate the old path.
#[tauri::command]
fn move_file(
    state: tauri::State<'_, Mutex<AppState>>,
    src_path: String,
    dest_path: String,
) -> Result<bool, String> {
    let src = std::path::Path::new(&src_path);
    let dest = std::path::Path::new(&dest_path);
    if !src.exists() {
        return Err(format!("Source does not exist: {}", src_path));
    }
    if dest.exists() {
        return Err(format!("Destination already exists: {}", dest_path));
    }
    std::fs::rename(src, dest).map_err(|e| e.to_string())?;
    let mut s = state.lock().map_err(|e| e.to_string())?;
    repoint_tabs_after_move(&mut s, src, dest);
    Ok(true)
}

/// Repoint every open tab whose file path equals `src` — or lives under `src`
/// when a directory was moved — to the corresponding location under `dest`.
fn repoint_tabs_after_move(state: &mut AppState, src: &std::path::Path, dest: &std::path::Path) {
    for tab in &mut state.tabs {
        let Some(fp) = &tab.file_path else { continue };
        if fp == src {
            tab.file_path = Some(dest.to_path_buf());
        } else if let Ok(rest) = fp.strip_prefix(src) {
            tab.file_path = Some(dest.join(rest));
        }
    }
}

/// Delete a file or directory.
#[tauri::command]
fn delete_file(
    state: tauri::State<'_, Mutex<AppState>>,
    path: String,
) -> Result<bool, String> {
    let p = delete_path(&path)?;
    let mut s = state.lock().map_err(|e| e.to_string())?;
    detach_tabs_for_deleted_path(&mut s, &p);
    Ok(true)
}

/// The filesystem part of delete_file — split out so tests can exercise it
/// without a `tauri::State`.
fn delete_path(path: &str) -> Result<PathBuf, String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(format!("Path does not exist: {}", path));
    }
    if p.is_dir() {
        std::fs::remove_dir_all(p).map_err(|e| e.to_string())?;
    } else {
        std::fs::remove_file(p).map_err(|e| e.to_string())?;
    }
    Ok(p.to_path_buf())
}

/// Clear `file_path` on every open tab pointing at `path` (or inside it, when
/// a directory was deleted). Without this the tab keeps a stale path and the
/// next save — including autosave — silently recreates a file the user just
/// deleted. The buffer itself is untouched; the tab behaves like an unsaved
/// document and will prompt for a new path on save.
fn detach_tabs_for_deleted_path(state: &mut AppState, path: &std::path::Path) {
    for tab in &mut state.tabs {
        let Some(fp) = &tab.file_path else { continue };
        // starts_with covers `fp == path` too (a path is its own prefix).
        if fp.starts_with(path) {
            tab.file_path = None;
        }
    }
}

/// Create a new file (with empty content).
#[tauri::command]
fn create_file(path: String) -> Result<bool, String> {
    let p = std::path::Path::new(&path);
    if p.exists() {
        return Err(format!("File already exists: {}", path));
    }
    std::fs::write(p, b"").map_err(|e| e.to_string())?;
    Ok(true)
}

/// Create a new directory.
#[tauri::command]
fn create_directory(path: String) -> Result<bool, String> {
    let p = std::path::Path::new(&path);
    if p.exists() {
        return Err(format!("Directory already exists: {}", path));
    }
    std::fs::create_dir(p).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Recursively copy a directory. Depth is capped so symlink cycles (Windows
/// falls back to copying the resolved target) can't recurse forever.
fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    copy_dir_depth(src, dest, 0)
}

fn copy_dir_depth(src: &std::path::Path, dest: &std::path::Path, depth: u32) -> std::io::Result<()> {
    if depth > 64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory depth limit exceeded (possible symlink cycle)",
        ));
    }
    std::fs::create_dir(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_symlink() {
            // Copy the symlink target path itself (don't follow — avoids infinite recursion on cycles).
            #[cfg(unix)]
            {
                if let Ok(target) = std::fs::read_link(&src_path) {
                    std::os::unix::fs::symlink(&target, &dest_path)?;
                }
            }
            #[cfg(windows)]
            {
                // On Windows, fall back to copying the resolved target.
                let resolved = src_path.canonicalize().unwrap_or_else(|_| src_path.clone());
                if resolved.is_dir() {
                    copy_dir_depth(&resolved, &dest_path, depth + 1)?;
                } else {
                    std::fs::copy(&resolved, &dest_path)?;
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                std::fs::copy(&src_path, &dest_path)?;
            }
        } else if file_type.is_dir() {
            copy_dir_depth(&src_path, &dest_path, depth + 1)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Retrieve the tear-off file path for the current window.
/// Called by the new window's init() to know which file to open.
/// Returns None if this is not a torn-off window.
#[tauri::command]
fn get_tear_off_file(
    state: tauri::State<'_, Mutex<AppState>>,
    webview: tauri::WebviewWindow,
) -> Result<Option<String>, String> {
    let label = webview.label().to_string();
    let mut s = state.lock().map_err(|e| e.to_string())?;
    Ok(s.tear_off_files.remove(&label))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn detect_line_ending(bytes: &[u8]) -> editor_domain::LineEnding {
    // Count each newline family once — a `\r\n` counts as one CRLF, not a lone
    // CR. The old check returned Crlf as soon as ANY `\r\n` existed, so files
    // that were mostly LF with one CRLF were mislabeled.
    let mut crlf = 0u64;
    let mut lf = 0u64;
    let mut cr = 0u64;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    crlf += 1;
                    i += 2;
                } else {
                    cr += 1;
                    i += 1;
                }
            }
            b'\n' => {
                lf += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    // Dominant family wins; genuinely mixed files are reported as Mixed
    // (per-line endings are preserved by the piece table regardless).
    let kinds = u8::from(crlf > 0) + u8::from(lf > 0) + u8::from(cr > 0);
    if kinds > 1 {
        return editor_domain::LineEnding::Mixed;
    }
    if crlf > 0 {
        editor_domain::LineEnding::Crlf
    } else if cr > 0 {
        editor_domain::LineEnding::Cr
    } else {
        editor_domain::LineEnding::Lf
    }
}

fn file_change_to_entry(f: &editor_git::FileChange) -> GitFileEntry {
    GitFileEntry {
        path: f.path.clone(),
        status: format!("{:?}", f.status),
        old_path: f.old_path.clone(),
    }
}

fn line_change_to_info(lc: &editor_diff::LineChange) -> GitDiffLine {
    match lc {
        editor_diff::LineChange::Equal { old_no, new_no, bytes } => GitDiffLine {
            kind: "equal".to_string(),
            old_no: Some(*old_no),
            new_no: Some(*new_no),
            text: String::from_utf8_lossy(bytes).to_string(),
        },
        editor_diff::LineChange::Delete { old_no, bytes } => GitDiffLine {
            kind: "delete".to_string(),
            old_no: Some(*old_no),
            new_no: None,
            text: String::from_utf8_lossy(bytes).to_string(),
        },
        editor_diff::LineChange::Insert { new_no, bytes } => GitDiffLine {
            kind: "insert".to_string(),
            old_no: None,
            new_no: Some(*new_no),
            text: String::from_utf8_lossy(bytes).to_string(),
        },
    }
}

/// Parse a unified diff text into hunks with line-level changes.
/// Each hunk header: `@@ -old_start,old_count +new_start,new_count @@`
/// Body lines: ` ` equal, `+` insert, `-` delete, `\` no-newline marker.
fn parse_unified_diff(text: &str) -> Vec<GitHunkInfo> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<GitHunkInfo> = None;
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;

    for line in text.lines() {
        if line.starts_with("@@ ") {
            // Push previous hunk.
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            // Parse: @@ -old_start,old_count +new_start,new_count @@
            let parts: Vec<&str> = line.split_whitespace().collect();
            // parts: ["@@", "-old_start,old_count", "+new_start,new_count", "@@"]
            let old_part = parts.get(1).unwrap_or(&"").trim_start_matches('-');
            let new_part = parts.get(2).unwrap_or(&"").trim_start_matches('+');
            let old_start: u32 = old_part.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let new_start: u32 = new_part.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(GitHunkInfo { old_start, new_start, lines: Vec::new() });
        } else if let Some(ref mut hunk) = current_hunk {
            if line.starts_with("diff --git") || line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("index ") {
                // File header — skip (we're parsing single file diff).
                continue;
            }
            let bytes = line.as_bytes();
            if bytes.is_empty() { continue; }
            match bytes[0] {
                b' ' => {
                    let text_content = &line[1..];
                    hunk.lines.push(GitDiffLine {
                        kind: "equal".to_string(),
                        old_no: Some(old_line),
                        new_no: Some(new_line),
                        text: text_content.to_string(),
                    });
                    old_line += 1;
                    new_line += 1;
                }
                b'+' => {
                    let text_content = &line[1..];
                    hunk.lines.push(GitDiffLine {
                        kind: "insert".to_string(),
                        old_no: None,
                        new_no: Some(new_line),
                        text: text_content.to_string(),
                    });
                    new_line += 1;
                }
                b'-' => {
                    let text_content = &line[1..];
                    hunk.lines.push(GitDiffLine {
                        kind: "delete".to_string(),
                        old_no: Some(old_line),
                        new_no: None,
                        text: text_content.to_string(),
                    });
                    old_line += 1;
                }
                b'\\' => {
                    // "\ No newline at end of file" — skip.
                }
                _ => {}
            }
        }
    }
    if let Some(h) = current_hunk {
        hunks.push(h);
    }
    hunks
}

// ── AST serialization for frontend rendering ────────────────────────────────

fn span_text(span: editor_markdown::SourceSpan, text: &str) -> String {
    let s = span.start.0 as usize;
    let e = span.end.0 as usize;
    // `str::get` returns None for mid-char boundaries — a stale span must not
    // panic the command.
    if s <= e { text.get(s..e).unwrap_or("").to_string() } else { String::new() }
}

/// Like span_text but rebases offsets by subtracting `base` (for virtualized
/// rendering where only the block's local bytes are available, not the full document).
fn span_text_relative(span: editor_markdown::SourceSpan, text: &str, base: u64) -> String {
    let s = span.start.0.saturating_sub(base) as usize;
    let e = span.end.0.saturating_sub(base) as usize;
    if s <= e { text.get(s..e).unwrap_or("").to_string() } else { String::new() }
}

/// Like block_to_ast but uses relative offsets (subtracts `base` from all spans).
/// Used by get_block_data for virtualized rendering where only the block's local
/// bytes are available, not the full 120MB document.
/// True when a line is a code-fence closing line: a run of 3+ identical
/// backticks or tildes and nothing else (closing fences cannot have an info
/// string; leading whitespace is allowed).
fn is_closing_fence_line(line: &str) -> bool {
    let t = line.trim();
    let Some(&c) = t.as_bytes().first() else { return false };
    if c != b'`' && c != b'~' {
        return false;
    }
    t.len() >= 3 && t.bytes().all(|b| b == c)
}

/// Split a line (as yielded by `split_inclusive('\n')`) into content and line
/// ending, mirroring the parser's `content_end` semantics — a trailing `\r`
/// belongs to the ending, never to the content.
fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(s) = line.strip_suffix("\r\n") {
        (s, "\r\n")
    } else if let Some(s) = line.strip_suffix('\n') {
        (s, "\n")
    } else if let Some(s) = line.strip_suffix('\r') {
        (s, "\r")
    } else {
        (line, "")
    }
}

/// Rebuild the marker-stripped buffer the parser produced for a block quote's
/// children: each line loses its leading spaces, the `>` marker, and one
/// optional space after it. Child block spans are relative to THIS buffer —
/// extracting them from the document text would read the wrong bytes.
fn de_mark_block_quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        let (content, ending) = split_line_ending(line);
        let ind = content.bytes().take_while(|&b| b == b' ').count();
        let s = &content[ind.min(content.len())..];
        let s = s.strip_prefix('>').unwrap_or(s);
        let s = s.strip_prefix(' ').unwrap_or(s);
        out.push_str(s);
        out.push_str(ending);
    }
    out
}

/// Rebuild the marker-stripped buffer the parser produced for a list item's
/// children: the first line loses indent + marker + whitespace after the
/// marker plus the task checkbox (rendered separately from `item.task`);
/// continuation lines lose `indent + content_indent` bytes.
fn de_mark_list_item(raw: &str, ordered: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut strip = 0usize;
    for (k, line) in raw.split_inclusive('\n').enumerate() {
        let (content, ending) = split_line_ending(line);
        if k == 0 {
            let ind = content.bytes().take_while(|&b| b == b' ').count();
            let s = &content[ind.min(content.len())..];
            let marker_len = if ordered {
                s.bytes().take_while(|b| b.is_ascii_digit()).count() + 1
            } else {
                1
            };
            let after = &s[marker_len.min(s.len())..];
            let ws = after.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
            strip = ind + marker_len + ws;
            let rest = &content[strip.min(content.len())..];
            if rest.starts_with("[ ] ") || rest.starts_with("[x] ") || rest.starts_with("[X] ") {
                strip += 4;
            }
        }
        out.push_str(&content[strip.min(content.len())..]);
        out.push_str(ending);
    }
    out
}

fn block_to_ast_relative(block: &editor_markdown::Block, text: &str, base: u64) -> Option<AstNode> {
    use editor_markdown::Block;
    match block {
        Block::BlankLine(_) => Some(AstNode::BlankLine),
        Block::ThematicBreak(_) => Some(AstNode::ThematicBreak),
        Block::Heading(h) => Some(AstNode::Heading {
            level: h.level,
            children: h.inlines.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
        }),
        Block::Paragraph(p) => Some(AstNode::Paragraph {
            children: p.inlines.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
        }),
        Block::BlockQuote(bq) => {
            let de = de_mark_block_quote(&span_text_relative(bq.meta.span, text, base));
            Some(AstNode::BlockQuote {
                children: bq.children.iter().filter_map(|b| block_to_ast_relative(b, &de, 0)).collect(),
            })
        }
        Block::List(l) => Some(AstNode::List {
            ordered: l.ordered,
            start: l.start,
            items: l.items.iter().map(|item| {
                let de = de_mark_list_item(&span_text_relative(item.meta.span, text, base), l.ordered);
                ListItemNode {
                    task: item.task.map(|t| match t {
                        editor_markdown::TaskState::Open => "open".to_string(),
                        editor_markdown::TaskState::Done => "done".to_string(),
                    }),
                    children: item.children.iter().filter_map(|b| block_to_ast_relative(b, &de, 0)).collect(),
                }
            }).collect(),
        }),
        Block::CodeBlock(cb) => {
            let raw = span_text_relative(cb.meta.span, text, base);
            let content = if cb.fenced {
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() >= 2 {
                    // Drop the closing line only when it is actually a closing
                    // fence — an *unclosed* fence would otherwise silently lose
                    // the last content line.
                    let body_end = if is_closing_fence_line(lines[lines.len() - 1]) {
                        lines.len() - 1
                    } else {
                        lines.len()
                    };
                    lines[1..body_end].join("\n")
                } else {
                    raw
                }
            } else {
                raw.lines().map(|l| {
                    if l.starts_with("    ") { l[4..].to_string() }
                    else if l.starts_with('\t') { l[1..].to_string() }
                    else { l.to_string() }
                }).collect::<Vec<_>>().join("\n")
            };
            Some(AstNode::CodeBlock {
                fenced: cb.fenced,
                language: cb.info_string.clone(),
                content,
            })
        }
        Block::Table(t) => {
            let alignments: Vec<String> = t.alignments.iter().map(|a| match a {
                editor_markdown::TableAlign::Left => "left".to_string(),
                editor_markdown::TableAlign::Center => "center".to_string(),
                editor_markdown::TableAlign::Right => "right".to_string(),
                editor_markdown::TableAlign::None => "none".to_string(),
            }).collect();
            let mut header = Vec::new();
            let mut rows: Vec<Vec<TableCellNode>> = Vec::new();
            for row in &t.rows {
                let cells: Vec<TableCellNode> = row.cells.iter().map(|c| TableCellNode {
                    align: "none".to_string(),
                    children: c.inlines.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
                }).collect();
                if row.header { header = cells; }
                else { rows.push(cells); }
            }
            Some(AstNode::Table { alignments, header, rows })
        }
        Block::HtmlBlock(hb) => Some(AstNode::HtmlBlock {
            content: span_text_relative(hb.meta.span, text, base),
        }),
        Block::LinkReferenceDefinition(d) => Some(AstNode::LinkRefDef {
            label: d.label.clone(),
            destination: d.destination.clone(),
            title: d.title.clone(),
        }),
        Block::UnknownBlock(ub) => Some(AstNode::HtmlBlock {
            content: span_text_relative(ub.meta.span, text, base),
        }),
    }
}

/// Like inline_to_ast but uses relative offsets for span-based content extraction.
/// Most inline variants store content directly (Text, CodeSpan, Autolink) so
/// they don't need rebasing. Only RawHtml and UnknownInline use span_text.
fn inline_to_ast_relative(inline: &editor_markdown::Inline, text: &str, base: u64) -> AstNode {
    use editor_markdown::Inline;
    match inline {
        Inline::Text(_m, s) => AstNode::Text { text: s.clone() },
        Inline::Emphasis(_, children, _) => AstNode::Emphasis {
            children: children.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
        },
        Inline::Strong(_, children, _) => AstNode::Strong {
            children: children.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
        },
        Inline::Strikethrough(_, children) => AstNode::Strikethrough {
            children: children.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
        },
        Inline::CodeSpan(_, s, _) => AstNode::CodeSpan { text: s.clone() },
        Inline::MathSpan(_, s, d) => AstNode::Math { content: s.clone(), display: *d },
        Inline::Link(l) => AstNode::Link {
            children: l.inlines.iter().map(|i| inline_to_ast_relative(i, text, base)).collect(),
            destination: l.destination.clone(),
            title: l.title.clone(),
        },
        Inline::Image(img) => AstNode::Image {
            alt: img.alt.clone(),
            destination: img.destination.clone(),
            title: img.title.clone(),
        },
        Inline::Autolink(_, url) => AstNode::Autolink { url: url.clone() },
        Inline::HardBreak(_) => AstNode::HardBreak,
        Inline::RawHtml(m) => AstNode::RawHtml { content: span_text_relative(m.span, text, base) },
        Inline::UnknownInline(m) => AstNode::Text { text: span_text_relative(m.span, text, base) },
    }
}

fn block_to_ast(block: &editor_markdown::Block, text: &str) -> Option<AstNode> {
    use editor_markdown::Block;
    match block {
        Block::BlankLine(_) => Some(AstNode::BlankLine),
        Block::ThematicBreak(_) => Some(AstNode::ThematicBreak),
        Block::Heading(h) => Some(AstNode::Heading {
            level: h.level,
            children: h.inlines.iter().map(|i| inline_to_ast(i, text)).collect(),
        }),
        Block::Paragraph(p) => Some(AstNode::Paragraph {
            children: p.inlines.iter().map(|i| inline_to_ast(i, text)).collect(),
        }),
        Block::BlockQuote(bq) => {
            let de = de_mark_block_quote(&span_text(bq.meta.span, text));
            Some(AstNode::BlockQuote {
                children: bq.children.iter().filter_map(|b| block_to_ast(b, &de)).collect(),
            })
        }
        Block::List(l) => Some(AstNode::List {
            ordered: l.ordered,
            start: l.start,
            items: l.items.iter().map(|item| {
                let de = de_mark_list_item(&span_text(item.meta.span, text), l.ordered);
                ListItemNode {
                    task: item.task.map(|t| match t {
                        editor_markdown::TaskState::Open => "open".to_string(),
                        editor_markdown::TaskState::Done => "done".to_string(),
                    }),
                    children: item.children.iter().filter_map(|b| block_to_ast(b, &de)).collect(),
                }
            }).collect(),
        }),
        Block::CodeBlock(cb) => {
            // Extract content between fence markers, or dedent indented code.
            let raw = span_text(cb.meta.span, text);
            let content = if cb.fenced {
                // Strip the opening fence line — and the closing one only when
                // it is actually a closing fence (unclosed fences keep all
                // remaining lines as content).
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() >= 2 {
                    let body_end = if is_closing_fence_line(lines[lines.len() - 1]) {
                        lines.len() - 1
                    } else {
                        lines.len()
                    };
                    lines[1..body_end].join("\n")
                } else {
                    raw
                }
            } else {
                // Dedent indented code (4 spaces or 1 tab).
                raw.lines().map(|l| {
                    if l.starts_with("    ") { l[4..].to_string() }
                    else if l.starts_with('\t') { l[1..].to_string() }
                    else { l.to_string() }
                }).collect::<Vec<_>>().join("\n")
            };
            Some(AstNode::CodeBlock {
                fenced: cb.fenced,
                language: cb.info_string.clone(),
                content,
            })
        }
        Block::Table(t) => {
            let alignments: Vec<String> = t.alignments.iter().map(|a| match a {
                editor_markdown::TableAlign::Left => "left".to_string(),
                editor_markdown::TableAlign::Center => "center".to_string(),
                editor_markdown::TableAlign::Right => "right".to_string(),
                editor_markdown::TableAlign::None => "none".to_string(),
            }).collect();
            let mut header = Vec::new();
            let mut rows: Vec<Vec<TableCellNode>> = Vec::new();
            for row in &t.rows {
                let cells: Vec<TableCellNode> = row.cells.iter().map(|c| TableCellNode {
                    align: "none".to_string(),
                    children: c.inlines.iter().map(|i| inline_to_ast(i, text)).collect(),
                }).collect();
                if row.header { header = cells; }
                else { rows.push(cells); }
            }
            Some(AstNode::Table { alignments, header, rows })
        }
        Block::HtmlBlock(hb) => Some(AstNode::HtmlBlock {
            content: span_text(hb.meta.span, text),
        }),
        Block::LinkReferenceDefinition(d) => Some(AstNode::LinkRefDef {
            label: d.label.clone(),
            destination: d.destination.clone(),
            title: d.title.clone(),
        }),
        Block::UnknownBlock(ub) => Some(AstNode::HtmlBlock {
            content: span_text(ub.meta.span, text),
        }),
    }
}

fn inline_to_ast(inline: &editor_markdown::Inline, text: &str) -> AstNode {
    use editor_markdown::Inline;
    match inline {
        Inline::Text(_m, s) => AstNode::Text { text: s.clone() },
        Inline::Emphasis(_, children, _) => AstNode::Emphasis {
            children: children.iter().map(|i| inline_to_ast(i, text)).collect(),
        },
        Inline::Strong(_, children, _) => AstNode::Strong {
            children: children.iter().map(|i| inline_to_ast(i, text)).collect(),
        },
        Inline::Strikethrough(_, children) => AstNode::Strikethrough {
            children: children.iter().map(|i| inline_to_ast(i, text)).collect(),
        },
        Inline::CodeSpan(_, s, _) => AstNode::CodeSpan { text: s.clone() },
        Inline::MathSpan(_, s, d) => AstNode::Math { content: s.clone(), display: *d },
        Inline::Link(l) => AstNode::Link {
            children: l.inlines.iter().map(|i| inline_to_ast(i, text)).collect(),
            destination: l.destination.clone(),
            title: l.title.clone(),
        },
        Inline::Image(img) => AstNode::Image {
            alt: img.alt.clone(),
            destination: img.destination.clone(),
            title: img.title.clone(),
        },
        Inline::Autolink(_, url) => AstNode::Autolink { url: url.clone() },
        Inline::HardBreak(_) => AstNode::HardBreak,
        Inline::RawHtml(m) => AstNode::RawHtml { content: span_text(m.span, text) },
        Inline::UnknownInline(m) => AstNode::Text { text: span_text(m.span, text) },
    }
}

fn block_kind_name(block: &editor_markdown::Block) -> String {
    match block {
        editor_markdown::Block::Heading(h) => format!("heading-{}", h.level),
        editor_markdown::Block::Paragraph(_) => "paragraph".to_string(),
        editor_markdown::Block::List(l) => {
            if l.ordered { "ordered-list".to_string() } else { "unordered-list".to_string() }
        }
        editor_markdown::Block::BlockQuote(_) => "block-quote".to_string(),
        editor_markdown::Block::CodeBlock(cb) => {
            if cb.fenced {
                format!("code-block-fenced-{}", cb.info_string)
            } else {
                "code-block-indented".to_string()
            }
        }
        editor_markdown::Block::Table(_) => "table".to_string(),
        editor_markdown::Block::HtmlBlock(_) => "html-block".to_string(),
        editor_markdown::Block::ThematicBreak(_) => "thematic-break".to_string(),
        editor_markdown::Block::BlankLine(_) => "blank-line".to_string(),
        editor_markdown::Block::LinkReferenceDefinition(_) => "link-ref-def".to_string(),
        editor_markdown::Block::UnknownBlock(_) => "unknown".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Image insertion
// ---------------------------------------------------------------------------

/// Read an image file from disk and return it as a `data:<mime>;base64,...` URL.
/// The frontend uses this to embed images directly into the Markdown source
/// instead of linking to an external file.
#[tauri::command]
fn read_image_file(path: String) -> Result<String, String> {
    const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
    let path = PathBuf::from(path);
    let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    if size > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large to embed ({size} bytes, max {MAX_IMAGE_BYTES})"
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };
    Ok(format!("data:{mime};base64,{}", BASE64_STANDARD.encode(&bytes)))
}

// ---------------------------------------------------------------------------
// App entry point
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::<AppState>::default())
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            switch_tab,
            close_tab,
            open_document,
            new_document,
            open_welcome,
            get_document_text,
            replace_text,
            insert_text,
            search_document,
            replace_match_in_document,
            replace_all_in_document,
            get_blocks,
            replace_block,
            undo,
            redo,
            save_document,
            get_syntax_tree,
            get_syntax_tree_meta,
            get_block_data,
            get_block_line_number,
            get_block_line_numbers,
            get_parsed_offset,
            parse_next_chunk,
            get_git_status,
            git_branches,
            git_diff,
            git_stage_all,
            git_stage_file,
            git_unstage_file,
            git_commit,
            git_checkout,
            git_file_history,
            git_log,
            git_diff_file,
            git_diff_file_commits,
            git_diff_file_vs_commit,
            git_discard_file,
            git_remove_untracked,
            git_diff_commits,
            git_diff_vs_commit,
            git_log_detailed,
            git_stash_list,
            git_stash_push,
            git_stash_pop,
            git_stash_apply,
            git_stash_drop,
            git_create_branch,
            git_delete_branch,
            git_rename_branch,
            git_merge,
            git_merge_abort,
            git_rebase,
            git_rebase_abort,
            git_rebase_continue,
            git_cherry_pick,
            git_revert,
            git_tags,
            git_create_tag,
            git_delete_tag,
            git_remotes,
            git_add_remote,
            git_push_to_remote,
            git_pull_from_remote,
            git_fetch_remote,
            git_reset_soft,
            git_reset_mixed,
            git_reset_hard,
            git_clean,
            git_current_branch,
            git_head_commit,
            git_read_file_at_revision,
            is_dirty,
            get_file_name,
            get_active_file_path,
            git_repo_root,
            github_auth_status,
            github_login,
            github_logout,
            github_repo_metadata,
            github_pull_requests,
            github_remote_branches,
            open_relative_file,
            list_directory,
            get_parent_dir,
            copy_file,
            move_file,
            delete_file,
            create_file,
            create_directory,
            get_tear_off_file,
            read_image_file,
        ])
        .setup(|_app| {
            #[cfg(desktop)]
            _app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;

            #[cfg(debug_assertions)]
            {
                use tauri::Manager;
                if let Some(window) = _app.get_webview_window("main") {
                    window.open_devtools();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running WoMD Tauri application");
}

fn main() {
    run();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// AppState should start with empty tear_off_files.
    #[test]
    fn test_tear_off_files_empty_by_default() {
        let state = AppState::default();
        assert!(state.tear_off_files.is_empty());
    }

    /// Storing and retrieving a tear-off file path should work.
    #[test]
    fn test_tear_off_file_store_and_retrieve() {
        let mut state = AppState::default();
        let label = "win-12345".to_string();
        let path = "C:\\docs\\test.md".to_string();

        // Store the file path.
        state.tear_off_files.insert(label.clone(), path.clone());

        // Retrieve it.
        let retrieved = state.tear_off_files.get(&label).cloned();
        assert_eq!(retrieved, Some(path.clone()));

        // Remove it (simulating get_tear_off_file consuming the entry).
        let removed = state.tear_off_files.remove(&label);
        assert_eq!(removed, Some(path));

        // Second retrieval should return None.
        assert!(state.tear_off_files.get(&label).is_none());
    }

    /// Multiple torn-off windows should each have their own file path.
    #[test]
    fn test_tear_off_multiple_windows() {
        let mut state = AppState::default();
        state.tear_off_files.insert("win-1".to_string(), "C:\\a.md".to_string());
        state.tear_off_files.insert("win-2".to_string(), "C:\\b.md".to_string());
        state.tear_off_files.insert("win-3".to_string(), "C:\\c.md".to_string());

        assert_eq!(state.tear_off_files.len(), 3);
        assert_eq!(state.tear_off_files.remove("win-2"), Some("C:\\b.md".to_string()));
        assert_eq!(state.tear_off_files.len(), 2);
        assert_eq!(state.tear_off_files.remove("win-1"), Some("C:\\a.md".to_string()));
        assert_eq!(state.tear_off_files.remove("win-3"), Some("C:\\c.md".to_string()));
        assert!(state.tear_off_files.is_empty());
    }

    /// A non-torn-off window (no entry in tear_off_files) should get None.
    #[test]
    fn test_tear_off_no_entry_returns_none() {
        let mut state = AppState::default();
        state.tear_off_files.insert("win-1".to_string(), "C:\\a.md".to_string());

        // Querying a label that doesn't exist returns None.
        let result = state.tear_off_files.remove("main");
        assert_eq!(result, None);
    }

    /// TabInfo should include file_path when the tab has a file.
    #[test]
    fn test_tab_info_includes_file_path() {
        let tab = DocumentTab {
            id: 1,
            buffer: DocumentBuffer::open(
                b"# Test\n".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("test.md"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            ).unwrap(),
            file_path: Some(PathBuf::from("C:\\docs\\test.md")),
        };

        let info = TabInfo {
            id: tab.id,
            file_name: tab.file_name(),
            file_path: tab.file_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            is_dirty: tab.buffer.is_dirty(),
        };

        assert_eq!(info.file_name, "test.md");
        assert_eq!(info.file_path, Some("C:\\docs\\test.md".to_string()));
        assert!(!info.is_dirty);
    }

    /// TabInfo should have file_path=None for untitled tabs.
    #[test]
    fn test_tab_info_no_file_path_for_untitled() {
        let tab = DocumentTab {
            id: 1,
            buffer: DocumentBuffer::open(
                b"".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("untitled.md"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            ).unwrap(),
            file_path: None,
        };

        let info = TabInfo {
            id: tab.id,
            file_name: tab.file_name(),
            file_path: tab.file_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            is_dirty: tab.buffer.is_dirty(),
        };

        assert_eq!(info.file_name, "untitled.md");
        assert_eq!(info.file_path, None);
    }

    /// open_relative_file should resolve a relative path against the active tab's directory.
    #[test]
    fn test_open_relative_file_path_resolution() {
        // Simulate: active tab is at C:\docs\main.md, relative path is "sub/other.md"
        let base = PathBuf::from("C:\\docs\\main.md");
        let base_dir = base.parent().unwrap();
        let resolved = base_dir.join("sub/other.md");
        assert_eq!(resolved, PathBuf::from("C:\\docs\\sub\\other.md"));
    }

    /// The tear-off flow: store file path → new window retrieves it → file is opened.
    #[test]
    fn test_tear_off_full_flow() {
        let mut state = AppState::default();

        // 1. User drags tab out → open_new_window stores file path.
        let label = "win-99999".to_string();
        let file_path = "C:\\docs\\my-file.md".to_string();
        state.tear_off_files.insert(label.clone(), file_path.clone());

        // 2. New window calls get_tear_off_file.
        let retrieved = state.tear_off_files.remove(&label);
        assert_eq!(retrieved, Some(file_path));

        // 3. Entry is consumed — subsequent calls return None.
        let second_call = state.tear_off_files.remove(&label);
        assert_eq!(second_call, None);
    }

    /// parse_unified_diff should parse a standard git diff output with hunks.
    #[test]
    fn test_parse_unified_diff_basic() {
        let diff = "diff --git a/file.txt b/file.txt\nindex abc..def 100644\n--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,4 @@\n line1\n line2\n+new line\n line3\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1, "should have 1 hunk");
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].lines.len(), 4, "should have 4 lines");
        assert_eq!(hunks[0].lines[0].kind, "equal");
        assert_eq!(hunks[0].lines[1].kind, "equal");
        assert_eq!(hunks[0].lines[2].kind, "insert");
        assert_eq!(hunks[0].lines[2].text, "new line");
        assert_eq!(hunks[0].lines[3].kind, "equal");
    }

    /// parse_unified_diff should handle multiple hunks.
    #[test]
    fn test_parse_unified_diff_multiple_hunks() {
        let diff = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n-old line\n+new line\n@@ -10,2 +10,2 @@\n-old2\n+new2\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 2, "should have 2 hunks");
        assert_eq!(hunks[0].lines.len(), 2);
        assert_eq!(hunks[0].lines[0].kind, "delete");
        assert_eq!(hunks[0].lines[1].kind, "insert");
        assert_eq!(hunks[1].lines.len(), 2);
        assert_eq!(hunks[1].old_start, 10);
    }

    /// parse_unified_diff should handle new file diffs.
    #[test]
    fn test_parse_unified_diff_new_file() {
        let diff = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..abc\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+line1\n+line2\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1, "should have 1 hunk for new file");
        assert_eq!(hunks[0].lines.len(), 2);
        assert_eq!(hunks[0].lines[0].kind, "insert");
        assert_eq!(hunks[0].lines[1].kind, "insert");
    }

    /// parse_unified_diff should handle deleted file diffs.
    #[test]
    fn test_parse_unified_diff_deleted_file() {
        let diff = "diff --git a/del.txt b/del.txt\ndeleted file mode 100644\nindex abc..0000000\n--- a/del.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-line1\n-line2\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1, "should have 1 hunk for deleted file");
        assert_eq!(hunks[0].lines.len(), 2);
        assert_eq!(hunks[0].lines[0].kind, "delete");
        assert_eq!(hunks[0].lines[1].kind, "delete");
    }

    /// parse_unified_diff should handle empty diff (no changes).
    #[test]
    fn test_parse_unified_diff_empty() {
        let hunks = parse_unified_diff("");
        assert!(hunks.is_empty());
    }

    /// parse_unified_diff should handle hunk header with function context.
    #[test]
    fn test_parse_unified_diff_with_function_context() {
        let diff = "--- a/file.rs\n+++ b/file.rs\n@@ -10,3 +10,4 @@ fn my_function(x: i32) -> i32 {\n     let y = x + 1;\n     y\n+    y * 2\n }\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 10);
        assert_eq!(hunks[0].new_start, 10);
        assert_eq!(hunks[0].lines.len(), 4);
    }

    /// parse_unified_diff should handle "No newline at end of file" marker.
    #[test]
    fn test_parse_unified_diff_no_newline_marker() {
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,2 @@\n line1\n+line2\n\\ No newline at end of file\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1);
        // The \ line should be skipped, so only 2 lines.
        assert_eq!(hunks[0].lines.len(), 2);
    }

    /// parse_unified_diff should handle hunk header without counts.
    #[test]
    fn test_parse_unified_diff_no_counts() {
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1,2 @@\n-old\n+new1\n+new2\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].lines.len(), 3);
    }

    /// root_pathspec should prepend the top-level literal pathspec magic.
    #[test]
    fn test_root_pathspec_basic() {
        assert_eq!(root_pathspec("src/main.rs"), ":(top,literal)src/main.rs");
        assert_eq!(root_pathspec("crates/editor-ui/src/main.rs"), ":(top,literal)crates/editor-ui/src/main.rs");
        // Glob metacharacters in real file names must be treated literally.
        assert_eq!(root_pathspec("docs/a[1].md"), ":(top,literal)docs/a[1].md");
    }

    /// root_pathspec should not double-prefix paths that already carry magic.
    #[test]
    fn test_root_pathspec_already_prefixed() {
        assert_eq!(root_pathspec(":/src/main.rs"), ":/src/main.rs");
        assert_eq!(root_pathspec(":(top)src/main.rs"), ":(top)src/main.rs");
    }

    /// root_pathspec should handle empty path.
    #[test]
    fn test_root_pathspec_empty() {
        assert_eq!(root_pathspec(""), ":(top,literal)");
    }

    /// list_directory should return entries sorted (dirs first).
    #[test]
    fn test_list_directory_sorts_dirs_first() {
        let dir = std::env::temp_dir().join("womd_test_list_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("z.txt"), b"z").unwrap();
        std::fs::create_dir(dir.join("subfolder")).unwrap();
        // Call the command logic directly (not via Tauri).
        let entries = list_directory(dir.to_string_lossy().to_string()).unwrap();
        assert!(entries.len() >= 3);
        // First entry should be the directory.
        assert!(entries[0].is_dir, "first entry should be a directory");
        assert_eq!(entries[0].name, "subfolder");
        // Files should be sorted alphabetically.
        assert_eq!(entries[1].name, "a.txt");
        assert_eq!(entries[2].name, "z.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// list_directory should skip hidden files.
    #[test]
    fn test_list_directory_skips_hidden() {
        let dir = std::env::temp_dir().join("womd_test_hidden");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        std::fs::write(dir.join("visible.txt"), b"y").unwrap();
        let entries = list_directory(dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "visible.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// get_parent_dir should return the parent path.
    #[test]
    fn test_get_parent_dir() {
        let parent = get_parent_dir("C:\\Users\\test\\docs".to_string()).unwrap();
        assert_eq!(parent, Some("C:\\Users\\test".to_string()));
    }

    /// create_file and delete_file should work.
    #[test]
    fn test_create_and_delete_file() {
        let dir = std::env::temp_dir().join("womd_test_create_delete");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let file_path = dir.join("test.txt");
        create_file(file_path.to_string_lossy().to_string()).unwrap();
        assert!(file_path.exists());
        // Creating again should fail.
        assert!(create_file(file_path.to_string_lossy().to_string()).is_err());
        delete_path(&file_path.to_string_lossy()).unwrap();
        assert!(!file_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// create_directory and delete_file (dir) should work.
    #[test]
    fn test_create_and_delete_directory() {
        let dir = std::env::temp_dir().join("womd_test_create_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let new_dir = dir.join("newfolder");
        create_directory(new_dir.to_string_lossy().to_string()).unwrap();
        assert!(new_dir.is_dir());
        delete_path(&new_dir.to_string_lossy()).unwrap();
        assert!(!new_dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// copy_file should copy a file to a new location.
    #[test]
    fn test_copy_file() {
        let dir = std::env::temp_dir().join("womd_test_copy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let src = dir.join("src.txt");
        std::fs::write(&src, b"hello").unwrap();
        let dest = dir.join("dest.txt");
        copy_file(src.to_string_lossy().to_string(), dest.to_string_lossy().to_string()).unwrap();
        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        // Copy to existing should fail.
        assert!(copy_file(src.to_string_lossy().to_string(), dest.to_string_lossy().to_string()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copying a directory into itself must be rejected — `read_dir` would
    /// observe the freshly-created destination mid-iteration and recurse
    /// (the depth cap alone can't stop exponential self-copying).
    #[test]
    fn test_copy_dir_into_itself_fails() {
        let dir = std::env::temp_dir().join("womd_test_copy_self");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        // Direct self-copy and copy into a subdirectory are both rejected.
        assert!(copy_file(
            dir.to_string_lossy().to_string(),
            dir.join("inside").to_string_lossy().to_string()
        )
        .is_err());
        assert!(copy_file(
            dir.to_string_lossy().to_string(),
            dir.join("a").join("b").to_string_lossy().to_string()
        )
        .is_err());
        // Copying to a sibling path still works.
        let sibling = dir.parent().unwrap().join("womd_test_copy_self_sibling");
        let _ = std::fs::remove_dir_all(&sibling);
        copy_file(dir.to_string_lossy().to_string(), sibling.to_string_lossy().to_string()).unwrap();
        assert_eq!(std::fs::read(sibling.join("f.txt")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    /// detect_line_ending counts each newline family once — a lone CRLF in
    /// an LF file (or vice versa) is Mixed, not a mislabeled dominant type.
    #[test]
    fn test_detect_line_ending() {
        use editor_domain::LineEnding;
        assert!(matches!(detect_line_ending(b"a\nb\n"), LineEnding::Lf));
        assert!(matches!(detect_line_ending(b"a\r\nb\r\n"), LineEnding::Crlf));
        assert!(matches!(detect_line_ending(b"a\rb\rc"), LineEnding::Cr));
        assert!(matches!(detect_line_ending(b"a\nb\r\n"), LineEnding::Mixed));
        assert!(matches!(detect_line_ending(b"a\r\nb\n"), LineEnding::Mixed));
        assert!(matches!(detect_line_ending(b""), LineEnding::Lf));
        assert!(matches!(detect_line_ending(b"no newlines"), LineEnding::Lf));
    }

    /// de_mark_block_quote strips the `> ` prefix per line, including the
    /// lazy-continuation form (lines without `>` stay verbatim inside a quote).
    #[test]
    fn test_de_mark_block_quote() {
        assert_eq!(de_mark_block_quote("> a\n> b\n"), "a\nb\n");
        assert_eq!(de_mark_block_quote(">a\n>b\n"), "a\nb\n");
        // Lazy continuation line (no `>`) is kept verbatim.
        assert_eq!(de_mark_block_quote("> a\ncontinued\n"), "a\ncontinued\n");
        // Nested quote markers are stripped one level only.
        assert_eq!(de_mark_block_quote("> > deep\n"), "> deep\n");
    }

    /// de_mark_list_item strips marker + indentation AND the task checkbox —
    /// the checkbox is re-emitted from `item.task`, so leaving it in the
    /// buffer would duplicate it on render and dirty regen.
    #[test]
    fn test_de_mark_list_item() {
        assert_eq!(de_mark_list_item("- hello\n- world\n", false), "hello\nworld\n");
        assert_eq!(de_mark_list_item("- [ ] todo\n- [x] done\n", false), "todo\ndone\n");
        assert_eq!(de_mark_list_item("- [x] done\n", false), "done\n");
        // Ordered markers.
        assert_eq!(de_mark_list_item("1. one\n2. two\n", true), "one\ntwo\n");
        assert_eq!(de_mark_list_item("3) three\n4) four\n", true), "three\nfour\n");
        // Continuation lines keep their content (indentation reduced by marker width).
        assert_eq!(de_mark_list_item("- a\n  cont\n", false), "a\ncont\n");
    }

    /// move_file should rename/move a file.
    #[test]
    fn test_move_file() {
        let dir = std::env::temp_dir().join("womd_test_move");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let src = dir.join("old.txt");
        std::fs::write(&src, b"data").unwrap();
        let dest = dir.join("new.txt");
        std::fs::rename(&src, &dest).unwrap();
        assert!(!src.exists());
        assert!(dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After a move, open tabs must follow the file to its new path —
    /// otherwise a save silently recreates the old path.
    #[test]
    fn test_repoint_tabs_after_move() {
        let mut s = AppState::default();
        let blank = || DocumentTab {
            id: 0,
            buffer: DocumentBuffer::open(
                b"x".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("t"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: None,
        };
        let mut t1 = blank();
        t1.file_path = Some(PathBuf::from("/repo/dir/a.md"));
        let mut t2 = blank();
        t2.file_path = Some(PathBuf::from("/repo/dir/sub/b.md"));
        let mut t3 = blank();
        t3.file_path = Some(PathBuf::from("/repo/other/c.md"));
        s.push_tab(t1);
        s.push_tab(t2);
        s.push_tab(t3);
        let src = std::path::Path::new("/repo/dir");
        let dest = std::path::Path::new("/repo/moved");
        repoint_tabs_after_move(&mut s, src, dest);
        assert_eq!(s.tabs[0].file_path.as_deref(), Some(std::path::Path::new("/repo/moved/a.md")));
        assert_eq!(s.tabs[1].file_path.as_deref(), Some(std::path::Path::new("/repo/moved/sub/b.md")));
        assert_eq!(s.tabs[2].file_path.as_deref(), Some(std::path::Path::new("/repo/other/c.md")));
    }

    /// Deleting a file (or its directory) must detach open tabs from the dead
    /// path — otherwise the next save silently recreates it.
    #[test]
    fn test_detach_tabs_for_deleted_path() {
        let mut s = AppState::default();
        let blank = || DocumentTab {
            id: 0,
            buffer: DocumentBuffer::open(
                b"x".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("t"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: None,
        };
        let mut t1 = blank();
        t1.file_path = Some(PathBuf::from("/repo/dir/a.md"));
        let mut t2 = blank();
        t2.file_path = Some(PathBuf::from("/repo/dir/sub/b.md"));
        let mut t3 = blank();
        t3.file_path = Some(PathBuf::from("/repo/other/c.md"));
        let mut t4 = blank(); // untitled — stays untouched
        t4.file_path = None;
        s.push_tab(t1);
        s.push_tab(t2);
        s.push_tab(t3);
        s.push_tab(t4);

        // File delete: only the exact file detaches.
        detach_tabs_for_deleted_path(&mut s, std::path::Path::new("/repo/dir/a.md"));
        assert_eq!(s.tabs[0].file_path, None);
        assert_eq!(s.tabs[1].file_path.as_deref(), Some(std::path::Path::new("/repo/dir/sub/b.md")));
        assert_eq!(s.tabs[2].file_path.as_deref(), Some(std::path::Path::new("/repo/other/c.md")));

        // Directory delete: every tab under it detaches; siblings keep paths.
        detach_tabs_for_deleted_path(&mut s, std::path::Path::new("/repo/dir"));
        assert_eq!(s.tabs[1].file_path, None);
        assert_eq!(s.tabs[2].file_path.as_deref(), Some(std::path::Path::new("/repo/other/c.md")));
        // A prefix-similar path (/repo/dirX) must NOT detach.
        let mut t5 = blank();
        t5.file_path = Some(PathBuf::from("/repo/dirX/d.md"));
        s.push_tab(t5);
        detach_tabs_for_deleted_path(&mut s, std::path::Path::new("/repo/dir"));
        assert_eq!(s.tabs[4].file_path.as_deref(), Some(std::path::Path::new("/repo/dirX/d.md")));
    }

    /// find_tab_idx_for_path must match a tab opened via a non-normalized
    /// spelling of the same file (dot segments, different separators) — this
    /// is what prevents the same file opening twice with diverging buffers.
    #[test]
    fn find_tab_idx_for_path_dedups() {
        let dir = std::env::temp_dir().join("womd_test_dedup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.md");
        std::fs::write(&f, b"hi").unwrap();

        let mk = |id: u64, path: Option<PathBuf>| DocumentTab {
            id,
            buffer: DocumentBuffer::open(
                b"x".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("t"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: path,
        };
        // Tab opened with a `..`-normalized-away spelling.
        let weird = dir.join("sub").join("..").join("a.md");
        let tabs = vec![
            mk(1, None),                                  // untitled — no match
            mk(2, Some(weird)),                           // same file, odd spelling
            mk(3, Some(dir.join("other.md"))),            // different file
        ];
        let canon = canonical_or_self(&f);
        assert_eq!(find_tab_idx_for_path(&tabs, &canon), Some(1));
        // A different file must not match.
        let other = dir.join("b.md");
        std::fs::write(&other, b"y").unwrap();
        assert_eq!(find_tab_idx_for_path(&tabs, &canonical_or_self(&other)), None);
        // Untitled tabs never match.
        let untitled = vec![mk(9, None)];
        assert_eq!(find_tab_idx_for_path(&untitled, &canon), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit path (e.g. from a Save As dialog) always wins over the
    /// tab's existing path.
    #[test]
    fn resolve_save_path_prefers_explicit_over_tab_path() {
        let explicit = Some(PathBuf::from("C:\\new\\explicit.md"));
        let tab_path = PathBuf::from("C:\\old\\tab.md");
        assert_eq!(
            resolve_save_path(explicit.clone(), Some(&tab_path)),
            explicit
        );
    }

    /// With no explicit path, fall back to the tab's existing file path.
    #[test]
    fn resolve_save_path_falls_back_to_tab_path() {
        let tab_path = PathBuf::from("C:\\docs\\existing.md");
        assert_eq!(
            resolve_save_path(None, Some(&tab_path)),
            Some(tab_path)
        );
    }

    /// An untitled tab (no explicit path, no tab path) has nowhere to save —
    /// `save_document` must surface this as an error rather than panicking
    /// or silently writing nowhere. This is the exact scenario that used to
    /// fail silently before the frontend was fixed to prompt a Save As
    /// dialog for untitled documents (see App.vue::saveFile).
    #[test]
    fn resolve_save_path_none_when_untitled_and_no_explicit_path() {
        assert_eq!(resolve_save_path(None, None), None);
    }

    /// End-to-end: saving a tab with a resolved path should write the exact
    /// serialized buffer bytes to disk (source-preservation, Invariant 1).
    #[test]
    fn save_document_writes_resolved_path_to_disk() {
        let dir = std::env::temp_dir().join("womd_test_save_document");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join("out.md");

        let tab = DocumentTab {
            id: 1,
            buffer: DocumentBuffer::open(
                b"# Saved\n".to_vec(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("untitled.md"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: None,
        };

        let save_path = resolve_save_path(Some(dest.clone()), tab.file_path.as_ref())
            .expect("explicit path must resolve");
        let bytes = tab.buffer.serialize();
        editor_storage::atomic_save(&save_path, &bytes).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"# Saved\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Find / Find & Replace
    // -----------------------------------------------------------------

    #[test]
    fn find_matches_plain_text_case_insensitive_by_default() {
        let r = find_matches_in_text("Foo bar foo BAR foo", "foo", false, false);
        assert!(r.valid);
        assert_eq!(
            r.matches.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            vec![(0, 3), (8, 11), (16, 19)]
        );
    }

    #[test]
    fn find_matches_plain_text_respects_case_sensitivity() {
        let r = find_matches_in_text("Foo foo FOO", "foo", true, false);
        assert_eq!(r.matches.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(), vec![(4, 7)]);
    }

    #[test]
    fn find_matches_empty_query_returns_no_matches() {
        let r = find_matches_in_text("anything", "", false, false);
        assert!(r.valid);
        assert!(r.matches.is_empty());
    }

    #[test]
    fn find_matches_regex_mode_finds_pattern_matches() {
        let r = find_matches_in_text("cat, bat, hat", "[cb]at", false, true);
        assert!(r.valid);
        assert_eq!(r.matches.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(), vec![(0, 3), (5, 8)]);
    }

    #[test]
    fn find_matches_invalid_regex_is_reported_as_invalid_not_an_error() {
        let r = find_matches_in_text("anything", "(unclosed", false, true);
        assert!(!r.valid);
        assert!(r.matches.is_empty());
    }

    #[test]
    fn find_matches_does_not_hang_on_zero_length_matches() {
        let r = find_matches_in_text("abc", "x*", false, true);
        assert!(r.valid);
        assert_eq!(r.matches.len(), 4); // positions 0..=3
        assert!(r.matches.iter().all(|m| m.start == m.end));
    }

    #[test]
    fn find_matches_caps_pathological_match_counts() {
        let text = "a".repeat(MAX_SEARCH_MATCHES + 500);
        let r = find_matches_in_text(&text, "a", false, false);
        assert!(r.valid);
        assert_eq!(r.matches.len(), MAX_SEARCH_MATCHES);
        assert!(r.truncated);
    }

    /// Case-insensitive matching must never rely on `to_lowercase()`-ing the
    /// haystack: for characters like German 'ß' that expand under case
    /// folding (ß -> "ss"), a naive lowercase-then-search would shift every
    /// subsequent byte offset out of sync with the real (original) document,
    /// corrupting the very save/replace operations Find is meant to drive.
    #[test]
    fn find_matches_case_insensitive_does_not_shift_byte_offsets_on_expanding_casefold() {
        let text = "Straße ist lang"; // 'ß' is 2 bytes in UTF-8
        let r = find_matches_in_text(text, "ist", false, false);
        assert!(r.valid);
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        assert_eq!(&text[m.start as usize..m.end as usize], "ist");
    }

    /// A previously-found match range must be re-verified before replacing —
    /// bytes that moved under the cursor must not be destroyed.
    #[test]
    fn range_still_matches_detects_stale_ranges() {
        let text = "foo bar foo";
        // Original match at [0,3) still matches.
        assert!(range_still_matches(text, 0, 3, "foo", false, false).unwrap());
        // Simulate a doc edit shifting the second match's bytes.
        let edited = "foo bar Xoo";
        assert!(!range_still_matches(edited, 8, 11, "foo", false, false).unwrap());
        // A range that only partially covers a match is rejected.
        assert!(!range_still_matches(text, 0, 2, "foo", false, false).unwrap());
        // Case-insensitive literal still matches a differently-cased range.
        assert!(range_still_matches("FOO x", 0, 3, "foo", false, false).unwrap());
    }

    /// `\b`-style context must be evaluated against the real document, not
    /// the sliced match text — `find_at` preserves it.
    #[test]
    fn range_still_matches_respects_word_boundaries() {
        // `\bfoo` matches at 4 in "the foo" but not inside "thefoo".
        assert!(range_still_matches("the foo", 4, 7, r"\bfoo", false, true).unwrap());
        assert!(!range_still_matches("thefoo", 3, 6, r"\bfoo", false, true).unwrap());
        // `\B` non-boundary: "oo" inside "xoo" — requires the preceding 'x'.
        assert!(range_still_matches("xoo", 1, 3, r"\Boo", false, true).unwrap());
        assert!(!range_still_matches(" oo", 1, 3, r"\Boo", false, true).unwrap());
    }

    #[test]
    fn expand_replacement_substitutes_capture_groups() {
        let expanded = expand_replacement("2024-01-15", r"(\d{4})-(\d{2})-(\d{2})", false, true, "$3/$2/$1");
        assert_eq!(expanded, "15/01/2024");
    }

    #[test]
    fn expand_replacement_is_literal_for_non_regex_search() {
        let expanded = expand_replacement("hello", "ell", false, false, "$1 stays literal");
        assert_eq!(expanded, "$1 stays literal");
    }

    /// Replacing every match must be a single atomic edit that, once undone,
    /// restores the document byte-for-byte (Invariant: undo is exact).
    #[test]
    fn replace_all_in_document_end_to_end() {
        let mut state = AppState::default();
        let original = b"foo bar foo baz foo".to_vec();
        state.push_tab(DocumentTab {
            id: 1,
            buffer: DocumentBuffer::open(
                original.clone(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("untitled.md"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: None,
        });
        let tab = state.active_tab_mut().expect("pushed tab");

        let text = String::from_utf8_lossy(&tab.buffer.serialize()).to_string();
        let found = find_matches_in_text(&text, "foo", false, false);
        assert_eq!(found.matches.len(), 3);

        let mut edits: Vec<TextEdit> = found
            .matches
            .iter()
            .rev()
            .map(|m| {
                TextEdit::replace(ByteRange::new(ByteOffset(m.start), ByteOffset(m.end)), b"X")
            })
            .collect();
        edits.shrink_to_fit();
        let tx = EditTransaction::new(
            edits,
            editor_domain::Selection::caret(ByteOffset(0)),
            editor_domain::Selection::caret(ByteOffset(0)),
        );
        tab.buffer.apply(tx).unwrap();
        assert_eq!(tab.buffer.serialize(), b"X bar X baz X");

        // A single undo must fully restore the original text.
        tab.buffer.undo().unwrap();
        assert_eq!(tab.buffer.serialize(), original);
    }

    #[test]
    fn replace_all_with_regex_capture_groups_end_to_end() {
        let mut state = AppState::default();
        let original = b"a=1;b=2;c=3".to_vec();
        state.push_tab(DocumentTab {
            id: 1,
            buffer: DocumentBuffer::open(
                original.clone(),
                editor_domain::DocumentMeta {
                    id: DocumentId::new("untitled.md"),
                    has_bom: false,
                    line_ending: editor_domain::LineEnding::Lf,
                    trailing_newline: false,
                    encoding: editor_domain::Encoding::Utf8,
                },
                MarkdownProfile::Gfm,
            )
            .unwrap(),
            file_path: None,
        });
        let tab = state.active_tab_mut().expect("pushed tab");

        let text = String::from_utf8_lossy(&tab.buffer.serialize()).to_string();
        let found = find_matches_in_text(&text, r"(\w)=(\d)", false, true);
        assert_eq!(found.matches.len(), 3);

        let mut edits: Vec<TextEdit> = found
            .matches
            .iter()
            .rev()
            .map(|m| {
                let start = m.start as usize;
                let end = m.end as usize;
                let replacement = expand_replacement(&text[start..end], r"(\w)=(\d)", false, true, "$1:$2");
                TextEdit::replace(ByteRange::new(ByteOffset(m.start), ByteOffset(m.end)), replacement.as_bytes())
            })
            .collect();
        edits.shrink_to_fit();
        let tx = EditTransaction::new(
            edits,
            editor_domain::Selection::caret(ByteOffset(0)),
            editor_domain::Selection::caret(ByteOffset(0)),
        );
        tab.buffer.apply(tx).unwrap();
        assert_eq!(tab.buffer.serialize(), b"a:1;b:2;c:3");
    }

    // -----------------------------------------------------------------
    // De-marked child coordinates (block quote / list item children)
    // -----------------------------------------------------------------

    /// A fenced code block inside a block quote must render its own bytes.
    /// Children of a quote live in marker-stripped coordinates — extracting
    /// them against the document text used to read coincident document bytes.
    #[test]
    fn block_to_ast_quote_child_code_block_uses_demarked_spans() {
        let src = "para text that makes the doc long\n\n> ```rust\n> code()\n> ```\n";
        let doc = editor_markdown::parse_with(src.as_bytes(), MarkdownProfile::Gfm).unwrap();
        let quote = doc.blocks.iter().find(|b| matches!(b, editor_markdown::Block::BlockQuote(_)))
            .expect("quote block");
        let node = block_to_ast(quote, src).unwrap();
        let AstNode::BlockQuote { children } = node else { panic!("expected BlockQuote") };
        let AstNode::CodeBlock { content, .. } = &children[0] else {
            panic!("expected CodeBlock child")
        };
        assert_eq!(content, "code()");
    }

    /// Same for list items: a fenced code block inside an item must show the
    /// item's code, not document bytes at coincident offsets.
    #[test]
    fn block_to_ast_list_item_code_block_uses_demarked_spans() {
        let src = "para text that makes the doc long\n\n- item\n\n  ```\n  inside()\n  ```\n";
        let doc = editor_markdown::parse_with(src.as_bytes(), MarkdownProfile::Gfm).unwrap();
        let list = doc.blocks.iter().find(|b| matches!(b, editor_markdown::Block::List(_)))
            .expect("list block");
        let node = block_to_ast(list, src).unwrap();
        let AstNode::List { items, .. } = node else { panic!("expected List") };
        let code = items[0].children.iter().find_map(|c| match c {
            AstNode::CodeBlock { content, .. } => Some(content.clone()),
            _ => None,
        }).expect("code block inside item");
        assert_eq!(code, "inside()");
    }

    /// HtmlBlock content inside a block quote resolves against the de-marked
    /// buffer as well.
    #[test]
    fn block_to_ast_quote_child_html_block_uses_demarked_spans() {
        let src = "para text that makes the doc long\n\n> <div>hi</div>\n";
        let doc = editor_markdown::parse_with(src.as_bytes(), MarkdownProfile::Gfm).unwrap();
        let quote = doc.blocks.iter().find(|b| matches!(b, editor_markdown::Block::BlockQuote(_)))
            .expect("quote block");
        let node = block_to_ast(quote, src).unwrap();
        let AstNode::BlockQuote { children } = node else { panic!("expected BlockQuote") };
        let AstNode::HtmlBlock { content } = &children[0] else {
            panic!("expected HtmlBlock child")
        };
        assert!(content.contains("<div>hi</div>"), "content was {:?}", content);
    }
}
