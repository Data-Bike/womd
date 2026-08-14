//! WoMD Tauri 2 UI shell (ADR-001).
//!
//! Exposes `editor-core`'s `DocumentBuffer` to the frontend via Tauri commands.
//! Supports multiple documents in Chrome-style tabs. The frontend is a vanilla
//! HTML/CSS/JS editor (no JS framework) that calls these commands through
//! `window.__TAURI__.core.invoke`.

#![cfg_attr(not(feature = "custom-protocol"), allow(dead_code))]

use std::sync::Mutex;
use std::path::PathBuf;
use std::collections::HashMap;

use editor_core::{DocumentBuffer, EditTransaction, TextEdit};
use editor_domain::{ByteOffset, ByteRange, MarkdownProfile, ids::DocumentId};
use editor_git::{GitExtended, VersionControl};
use serde::{Deserialize, Serialize};

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
    text: String,
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
    new_text: String,
}

#[derive(Serialize, Deserialize)]
struct EditResult {
    text: String,
    is_dirty: bool,
    block_count: usize,
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
    Ok(DocumentInfo {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        file_name: tab.file_name(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
        byte_len: tab.buffer.serialize().len(),
        tab_id: tab.id,
    })
}

/// Close a tab by id. Returns the new active tab's info (or None if no tabs remain).
#[tauri::command]
fn close_tab(state: tauri::State<'_, Mutex<AppState>>, tab_id: u64) -> Result<Option<DocumentInfo>, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let idx = s.tabs.iter().position(|t| t.id == tab_id).ok_or("tab not found")?;
    s.tabs.remove(idx);
    // Adjust active index.
    if s.tabs.is_empty() {
        s.active = 0;
        return Ok(None);
    }
    if s.active >= s.tabs.len() {
        s.active = s.tabs.len() - 1;
    }
    let tab = &s.tabs[s.active];
    Ok(Some(DocumentInfo {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        file_name: tab.file_name(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
        byte_len: tab.buffer.serialize().len(),
        tab_id: tab.id,
    }))
}

// ---------------------------------------------------------------------------
// Tauri commands — document operations (operate on the active tab)
// ---------------------------------------------------------------------------

/// Open a file from disk in a new tab.
#[tauri::command]
fn open_document(state: tauri::State<'_, Mutex<AppState>>, path: String) -> Result<DocumentInfo, String> {
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let file_name = PathBuf::from(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let trailing_newline = bytes.last() == Some(&b'\n');
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new(&file_name),
        has_bom: bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        line_ending: detect_line_ending(&bytes),
        trailing_newline,
        encoding: editor_domain::Encoding::Utf8,
    };
    let buffer = DocumentBuffer::open(bytes, meta, MarkdownProfile::Gfm)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buffer.serialize()).to_string();
    let id = {
        let mut s = state.lock().map_err(|e| e.to_string())?;
        let id = s.next_id;
        s.next_id += 1;
        s.push_tab(DocumentTab { id, buffer, file_path: Some(PathBuf::from(path)) });
        id
    };
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(DocumentInfo {
        text,
        file_name,
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
        byte_len: tab.buffer.serialize().len(),
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
        trailing_newline: true,
        encoding: editor_domain::Encoding::Utf8,
    };
    let buffer = DocumentBuffer::open(b"\n".to_vec(), meta, MarkdownProfile::Gfm)
        .map_err(|e| e.to_string())?;
    let id = {
        let mut s = state.lock().map_err(|e| e.to_string())?;
        let id = s.next_id;
        s.next_id += 1;
        s.push_tab(DocumentTab { id, buffer, file_path: None });
        id
    };
    Ok(DocumentInfo {
        text: String::new(),
        file_name: "untitled.md".to_string(),
        is_dirty: false,
        block_count: 0,
        byte_len: 0,
        tab_id: id,
    })
}

/// Get the current document text (serialized from the active buffer).
#[tauri::command]
fn get_document_text(state: tauri::State<'_, Mutex<AppState>>) -> Result<String, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    Ok(String::from_utf8_lossy(&tab.buffer.serialize()).to_string())
}

/// Replace a text range in the active document.
#[tauri::command]
fn replace_text(
    state: tauri::State<'_, Mutex<AppState>>,
    args: ReplaceTextArgs,
) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    let edit = TextEdit::replace(
        ByteRange::new(ByteOffset(args.start), ByteOffset(args.end)),
        args.new_text.as_bytes(),
    );
    let tx = EditTransaction::single(
        edit,
        editor_domain::Selection::caret(ByteOffset(args.start)),
        editor_domain::Selection::caret(ByteOffset(args.start + args.new_text.len() as u64)),
    );
    tab.buffer.apply(tx).map_err(|e| e.to_string())?;
    Ok(EditResult {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
    })
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

/// Get all blocks of the active document with their source text.
/// Used by the WYSIWYG block editor to render and edit blocks individually.
#[tauri::command]
fn get_blocks(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<BlockInfo>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let serialized = tab.buffer.serialize();
    let full_text = String::from_utf8_lossy(&serialized);
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
            BlockInfo {
                index: i,
                kind: block_kind_name(b),
                source: full_text[start..end].to_string(),
                start: m.span.start.0,
                end: m.span.end.0,
            }
        })
        .collect();
    Ok(blocks)
}

/// Replace a single block's source text by block index.
/// This produces a minimal diff — only the block's byte range is replaced.
#[tauri::command]
fn replace_block(
    state: tauri::State<'_, Mutex<AppState>>,
    block_index: usize,
    new_source: String,
) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    // Get the block's span before replacing.
    let span = {
        let blocks = &tab.buffer.syntax().blocks;
        let block = blocks.get(block_index).ok_or_else(|| "invalid block index".to_string())?;
        block.meta().span
    };
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
    Ok(EditResult {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
    })
}

/// Undo the last edit in the active document.
#[tauri::command]
fn undo(state: tauri::State<'_, Mutex<AppState>>) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    tab.buffer.undo().map_err(|e| e.to_string())?;
    Ok(EditResult {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
    })
}

/// Redo the last undone edit in the active document.
#[tauri::command]
fn redo(state: tauri::State<'_, Mutex<AppState>>) -> Result<EditResult, String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab_mut()?;
    tab.buffer.redo().map_err(|e| e.to_string())?;
    Ok(EditResult {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
    })
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
    let save_path = path.map(PathBuf::from).or_else(|| tab.file_path.clone());
    let save_path = save_path.ok_or("no file path to save to")?;
    std::fs::write(&save_path, &bytes).map_err(|e| e.to_string())?;
    tab.buffer.mark_saved();
    tab.file_path = Some(save_path);
    Ok(true)
}

/// Get the syntax tree for the active document.
#[tauri::command]
fn get_syntax_tree(state: tauri::State<'_, Mutex<AppState>>) -> Result<Vec<SyntaxBlock>, String> {
    let s = state.lock().map_err(|e| e.to_string())?;
    let tab = s.active_tab()?;
    let full_text = tab.buffer.serialize();
    let text_str = String::from_utf8_lossy(&full_text).to_string();
    let blocks: Vec<SyntaxBlock> = tab
        .buffer
        .syntax()
        .blocks
        .iter()
        .map(|b| {
            let m = b.meta();
            let start = m.span.start.0 as usize;
            let end = m.span.end.0 as usize;
            let source = if start <= end && end <= text_str.len() {
                text_str[start..end].to_string()
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

/// Convert a repo-root-relative path to a git pathspec that works from any subdirectory.
/// Uses the `:/` magic prefix (relative to root of working tree).
fn root_pathspec(path: &str) -> String {
    if path.starts_with(":/") || path.starts_with("**/") {
        path.to_string()
    } else {
        format!(":/{}", path)
    }
}

/// Get unified diff for a single file (working tree vs HEAD) with parsed hunks and lines.
#[tauri::command]
fn git_diff_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    // Get the unified diff text for this file.
    let text = git.exec_text(&["diff", "HEAD", "--", &ps]).map_err(|e| e.to_string())?;
    // If empty (no unstaged changes), try staged (index vs HEAD).
    let text = if text.trim().is_empty() {
        git.exec_text(&["diff", "--cached", "HEAD", "--", &ps]).map_err(|e| e.to_string())?
    } else { text };
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Get unified diff for a single file between two commits.
#[tauri::command]
fn git_diff_file_commits(state: tauri::State<'_, Mutex<AppState>>, file_path: String, commit_a: String, commit_b: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    let text = git.exec_text(&["diff", &commit_a, &commit_b, "--", &ps]).map_err(|e| e.to_string())?;
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Get unified diff for a single file between working tree and a commit.
#[tauri::command]
fn git_diff_file_vs_commit(state: tauri::State<'_, Mutex<AppState>>, file_path: String, commit: String) -> Result<GitFileDiff, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    let text = git.exec_text(&["diff", &commit, "--", &ps]).map_err(|e| e.to_string())?;
    let hunks = parse_unified_diff(&text);
    Ok(GitFileDiff { path: file_path, old_path: None, hunks })
}

/// Discard changes to a file (restore from HEAD). Equivalent to `git checkout -- <file>`.
#[tauri::command]
fn git_discard_file(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    git.exec_text(&["checkout", "--", &ps]).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Remove an untracked file (delete from disk).
#[tauri::command]
fn git_remove_untracked(state: tauri::State<'_, Mutex<AppState>>, file_path: String) -> Result<bool, String> {
    let git = open_git_for_active(&state)?;
    let ps = root_pathspec(&file_path);
    git.exec_text(&["clean", "-f", "--", &ps]).map_err(|e| e.to_string())?;
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
    let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let git = editor_git::GitCli::open(dir).map_err(|e| e.to_string())?;
    let history = editor_git::file_history(&git, &file_name).map_err(|e| e.to_string())?;
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
    // Use git log directly via the CLI.
    let text = git.exec_text(&["log", "--pretty=format:%H%x09%an%x09%ad%x09%s", "--date=short", "-50"]).map_err(|e| e.to_string())?;
    let mut commits = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let sha = f.next().unwrap_or("").to_string();
        let author = f.next().unwrap_or("").to_string();
        let date = f.next().unwrap_or("").to_string();
        let message = f.next().unwrap_or("").to_string();
        if !sha.is_empty() {
            commits.push(GitCommitInfo { sha, author, date, message });
        }
    }
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
        // Read from working tree (for untracked files).
        // file_path is relative to repo root, but work_dir might be a subdirectory.
        // Resolve via git rev-parse --show-toplevel to get the repo root.
        let root = git.exec_text(&["rev-parse", "--show-toplevel"]).map_err(|e| e.to_string())?;
        let root = root.trim();
        let full = std::path::Path::new(root).join(&file_path);
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
        .ok_or("no base directory")?;
    let resolved = base_dir.join(&relative_path);
    drop(s);

    let path = resolved.canonicalize().unwrap_or(resolved);
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let file_name = path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let trailing_newline = bytes.last() == Some(&b'\n');
    let meta = editor_domain::DocumentMeta {
        id: DocumentId::new(&file_name),
        has_bom: bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        line_ending: detect_line_ending(&bytes),
        trailing_newline,
        encoding: editor_domain::Encoding::Utf8,
    };
    let buffer = DocumentBuffer::open(bytes, meta, MarkdownProfile::Gfm)
        .map_err(|e| e.to_string())?;

    let mut s = state.lock().map_err(|e| e.to_string())?;
    let id = s.next_id;
    s.next_id += 1;
    let tab = DocumentTab {
        id,
        buffer,
        file_path: Some(path.clone()),
    };
    let info = DocumentInfo {
        text: String::from_utf8_lossy(&tab.buffer.serialize()).to_string(),
        file_name: tab.file_name(),
        is_dirty: tab.buffer.is_dirty(),
        block_count: tab.buffer.syntax().blocks.len(),
        byte_len: tab.buffer.serialize().len(),
        tab_id: tab.id,
    };
    s.push_tab(tab);
    Ok(info)
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
    if bytes.windows(2).any(|w| w == b"\r\n") {
        editor_domain::LineEnding::Crlf
    } else if bytes.contains(&b'\r') {
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
    if s <= e && e <= text.len() { text[s..e].to_string() } else { String::new() }
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
        Block::BlockQuote(bq) => Some(AstNode::BlockQuote {
            children: bq.children.iter().filter_map(|b| block_to_ast(b, text)).collect(),
        }),
        Block::List(l) => Some(AstNode::List {
            ordered: l.ordered,
            start: l.start,
            items: l.items.iter().map(|item| ListItemNode {
                task: item.task.map(|t| match t {
                    editor_markdown::TaskState::Open => "open".to_string(),
                    editor_markdown::TaskState::Done => "done".to_string(),
                }),
                children: item.children.iter().filter_map(|b| block_to_ast(b, text)).collect(),
            }).collect(),
        }),
        Block::CodeBlock(cb) => {
            // Extract content between fence markers, or dedent indented code.
            let raw = span_text(cb.meta.span, text);
            let content = if cb.fenced {
                // Strip fence lines.
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() >= 2 {
                    lines[1..lines.len()-1].join("\n")
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
            get_document_text,
            replace_text,
            insert_text,
            get_blocks,
            replace_block,
            undo,
            redo,
            save_document,
            get_syntax_tree,
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
            open_relative_file,
            get_tear_off_file,
        ])
        .setup(|_app| {
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

    /// root_pathspec should prepend :/ to make paths relative to repo root.
    #[test]
    fn test_root_pathspec_basic() {
        assert_eq!(root_pathspec("src/main.rs"), ":/src/main.rs");
        assert_eq!(root_pathspec("crates/editor-ui/src/main.rs"), ":/crates/editor-ui/src/main.rs");
    }

    /// root_pathspec should not double-prefix paths that already start with :/.
    #[test]
    fn test_root_pathspec_already_prefixed() {
        assert_eq!(root_pathspec(":/src/main.rs"), ":/src/main.rs");
        assert_eq!(root_pathspec("**/*.rs"), "**/*.rs");
    }

    /// root_pathspec should handle empty path.
    #[test]
    fn test_root_pathspec_empty() {
        assert_eq!(root_pathspec(""), ":/");
    }
}
