// Mock Tauri backend for browser-based E2E clicking.
// Emulates the editor-ui Rust command surface over an in-memory document
// model (byte-accurate offsets via TextEncoder) + a virtual filesystem.
// Injected via page.addInitScript before the app bundle runs.

const enc = new TextEncoder();
const dec = new TextDecoder();
const toB = (s) => enc.encode(s);
const toS = (b) => dec.decode(b);
const bLen = (s) => enc.encode(s).length;

// ── Virtual FS ──────────────────────────────────────────────────────────
const CRLF_DOC = '# CRLF\r\n\r\n- a\r\n- b\r\n\r\n> quote\r\n\r\ntail\r\n';
const NOTES_MD = `# Notes

First **bold** paragraph with *em*, \`code\`, [link](https://example.com) and ~~strike~~.

## Second heading

- item one
- item two
- [ ] open task
- [x] done task

> quoted line
> continued

\`\`\`js
const x = 1;
console.log(x);
\`\`\`

| Col A | Col B |
| ----- | ----- |
| 1     | 2     |

$$E = mc^2$$

Some cyrillic: привет мир, и ещё текст.

***

Trailing paragraph.

See the [deep page](sub/deep.md) for details.
`;

const BIG_MD = (() => {
  let s = '# Big\n\n';
  for (let i = 0; i < 400; i++) s += `Paragraph number ${i} with some filler text to make it longer.\n\n`;
  return s;
})();

const WELCOME_MD = `# Welcome to WoMD

A **source-preserving** Markdown editor.

- type here
- click blocks

\`\`\`
code
\`\`\`
`;

const gitState = {
  branch: 'main',
  branches: [{ name: 'main', is_remote: false, is_current: true, upstream: 'origin/main', ahead: 1, behind: 0 }],
  staged: [],
  changes: [{ path: 'notes.md', status: 'Modified', old_path: null }],
  untracked: [{ path: 'new.md', status: 'Untracked', old_path: null }],
  stash: [],
  tags: [],
  remotes: [],
  log: [{ sha: 'abc1234def5678', author: 'T', date: '2024-01-01', message: 'init' }],
};

const vfs = new Map();
vfs.set('/repo/notes.md', NOTES_MD);
vfs.set('/repo/crlf.md', CRLF_DOC);
vfs.set('/repo/big.md', BIG_MD);
vfs.set('/repo/new.md', 'untracked content\n');
vfs.set('/repo/sub/deep.md', '# Deep\n\nnested file\n');

// ── Document model ──────────────────────────────────────────────────────
let nextTabId = 1;
const state = { tabs: [], active: -1 };

function newTab(name, path, text) {
  const t = {
    id: nextTabId++,
    name,
    path,
    bytes: toB(text),
    dirty: false,
    undo: [],
    redo: [],
    metaId: path ? `doc-${path}` : `doc-${name}-${nextTabId}`,
  };
  state.tabs.push(t);
  state.active = state.tabs.length - 1;
  return t;
}
const activeTab = () => state.tabs[state.active] || null;

function docInfo(t) {
  const blocks = parseBlocks(toS(t.bytes));
  return {
    text: t.bytes.length > 1_048_576 ? null : toS(t.bytes),
    file_name: t.name,
    is_dirty: t.dirty,
    block_count: blocks.length,
    byte_len: t.bytes.length,
    tab_id: t.id,
  };
}

function splice(tab, start, end, insertB) {
  const before = tab.bytes.slice(0, start);
  const after = tab.bytes.slice(end);
  const out = new Uint8Array(before.length + insertB.length + after.length);
  out.set(before); out.set(insertB, before.length); out.set(after, before.length + insertB.length);
  tab.undo.push(tab.bytes); tab.redo = [];
  tab.bytes = out; tab.dirty = true;
}

function editResult(t, changed = true) {
  const blocks = parseBlocks(toS(t.bytes));
  return {
    text: t.bytes.length > 1_048_576 ? null : toS(t.bytes),
    is_dirty: t.dirty,
    block_count: blocks.length,
    changed,
  };
}

// ── Block splitter (line-based approximation of the Rust parser) ────────
function parseBlocks(text) {
  const lines = text.match(/[^\n]*(?:\n|$)/g) || [];
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  const off = [];
  let acc = 0;
  for (const l of lines) { off.push(acc); acc += bLen(l); }
  const blocks = [];
  const isBlank = (s) => /^\s*$/.test(s.replace(/\r?\n$/, ''));
  const isListItem = (s) => /^ {0,3}([-+*]|\d{1,9}[.)])(\s|$)/.test(s);
  const isQuote = (s) => /^ {0,3}>/.test(s);
  const isFence = (s) => /^ {0,3}(`{3,}|~{3,})/.test(s);
  const isHeading = (s) => /^ {0,3}#{1,6}(\s|$)/.test(s);
  const isHR = (s) => /^ {0,3}([-*_])[ \t]*(\1[ \t]*){2,}$/.test(s.trim());
  const isIndented = (s) => /^(    |\t)/.test(s);
  const isRefDef = (s) => /^ {0,3}\[[^\]]+\]:/.test(s);
  const isHtml = (s) => /^ {0,3}<[A-Za-z!/]/.test(s);
  const isTableLine = (s) => s.includes('|');
  const isTableSep = (s) => /^ {0,3}\|?[\s:|-]*-+[\s:|-]*\|?\s*$/.test(s.trimEnd());

  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const bare = line.replace(/\r?\n$/, '');
    const start = off[i];
    let kind = 'paragraph', j = i + 1;

    if (isBlank(line)) {
      kind = 'blank-line';
    } else if (isFence(bare)) {
      const m = bare.match(/^ {0,3}(`{3,}|~{3,})/);
      const fc = m[1][0], fl = m[1].length;
      const info = bare.slice(bare.indexOf(m[1]) + fl).trim().split(/\s/)[0] || '';
      kind = `code-block-fenced-${info}`;
      while (j < lines.length) {
        const bl = lines[j].replace(/\r?\n$/, '');
        const close = bl.match(/^ {0,3}(`{3,}|~{3,})\s*$/);
        j++;
        if (close && close[1][0] === fc && close[1].length >= fl) break;
      }
    } else if (isHeading(bare)) {
      const lvl = bare.match(/#/g).length;
      kind = `heading-${Math.min(lvl, 6)}`;
    } else if (isHR(bare)) {
      kind = 'thematic-break';
    } else if (isListItem(bare)) {
      kind = /^\s*\d/.test(bare) ? 'ordered-list' : 'unordered-list';
      while (j < lines.length) {
        const bl = lines[j].replace(/\r?\n$/, '');
        if (isListItem(bl) || /^\s{2,}\S/.test(bl)) { j++; continue; }
        if (isBlank(lines[j])) break;
        if (isFence(bl) || isHeading(bl) || isQuote(bl)) break;
        break;
      }
    } else if (isQuote(bare)) {
      kind = 'block-quote';
      while (j < lines.length && isQuote(lines[j].replace(/\r?\n$/, ''))) j++;
    } else if (isTableLine(bare) && i + 1 < lines.length && isTableSep(lines[i + 1].replace(/\r?\n$/, ''))) {
      kind = 'table';
      j = i + 2;
      while (j < lines.length && isTableLine(lines[j]) && !isBlank(lines[j])) j++;
    } else if (isIndented(bare)) {
      kind = 'code-block-indented';
      while (j < lines.length && isIndented(lines[j].replace(/\r?\n$/, ''))) j++;
    } else if (isRefDef(bare)) {
      kind = 'link-ref-def';
    } else if (isHtml(bare)) {
      kind = 'html-block';
      while (j < lines.length && !isBlank(lines[j])) j++;
    } else {
      kind = 'paragraph';
      while (j < lines.length) {
        const bl = lines[j].replace(/\r?\n$/, '');
        if (isBlank(lines[j])) break;
        if (isFence(bl) || isHeading(bl) || isQuote(bl) || isListItem(bl) || isIndented(bl) || isRefDef(bl) || isHtml(bl)) break;
        if (/^ {0,3}=+\s*$/.test(bl)) { kind = 'heading-1'; j++; break; }
        if (/^ {0,3}-+\s*$/.test(bl)) { kind = 'heading-2'; j++; break; }
        j++;
      }
    }
    const end = j < lines.length ? off[j] : acc;
    blocks.push({ kind, start, end });
    i = j;
  }
  return blocks;
}

// ── Minimal inline parser → AstNode ─────────────────────────────────────
function parseInline(text) {
  const nodes = [];
  const re = /(!\[([^\]]*)\]\(([^)\s]+)(?:\s+"([^"]*)")?\))|(\[([^\]]+)\]\(([^)\s]+)(?:\s+"([^"]*)")?\))|(\*\*([^*]+)\*\*)|(__([^_]+)__)|(\*([^*]+)\*)|(_([^_]+)_)|(~~([^~]+)~~)|(`([^`]+)`)|(\$\$([^$]+)\$\$)|(\$([^$\n]+)\$)|(<(https?:\/\/[^>]+)>)/g;
  let last = 0, m;
  const pushText = (s) => { if (s) nodes.push({ type: 'Text', text: s }); };
  while ((m = re.exec(text))) {
    pushText(text.slice(last, m.index));
    if (m[1]) nodes.push({ type: 'Image', alt: m[2], destination: m[3], title: m[4] || null });
    else if (m[5]) nodes.push({ type: 'Link', children: [{ type: 'Text', text: m[6] }], destination: m[7], title: m[8] || null });
    else if (m[9]) nodes.push({ type: 'Strong', children: parseInline(m[10]) });
    else if (m[11]) nodes.push({ type: 'Strong', children: parseInline(m[12]) });
    else if (m[13]) nodes.push({ type: 'Emphasis', children: parseInline(m[14]) });
    else if (m[15]) nodes.push({ type: 'Emphasis', children: parseInline(m[16]) });
    else if (m[17]) nodes.push({ type: 'Strikethrough', children: parseInline(m[18]) });
    else if (m[19]) nodes.push({ type: 'CodeSpan', text: m[20] });
    else if (m[21]) nodes.push({ type: 'Math', content: m[22], display: true });
    else if (m[23]) nodes.push({ type: 'Math', content: m[24], display: false });
    else if (m[25]) nodes.push({ type: 'Autolink', url: m[26] });
    last = re.lastIndex;
  }
  pushText(text.slice(last));
  return nodes;
}

function blockNode(b, text) {
  const src = text.slice(b.start, b.end);
  const bare = src.replace(/\r?\n$/, '');
  const k = b.kind;
  if (k.startsWith('heading-')) {
    const lvl = parseInt(k.slice(8));
    const t = bare.replace(/^ {0,3}#{1,6}\s*/, '').replace(/\s*#+\s*$/, '').replace(/\r?\n {0,3}[=-]+\s*$/, '');
    return { type: 'Heading', level: lvl, children: parseInline(t) };
  }
  if (k === 'paragraph') return { type: 'Paragraph', children: parseInline(bare) };
  if (k === 'thematic-break') return { type: 'ThematicBreak' };
  if (k === 'blank-line') return { type: 'BlankLine' };
  if (k === 'block-quote') {
    const inner = bare.split('\n').map((l) => l.replace(/^ {0,3}> ?/, '')).join('\n');
    return { type: 'BlockQuote', children: [{ type: 'Paragraph', children: parseInline(inner) }] };
  }
  if (k === 'unordered-list' || k === 'ordered-list') {
    const items = bare.split('\n').filter((l) => /^ {0,3}([-+*]|\d{1,9}[.)])(\s|$)/.test(l)).map((l) => {
      const body = l.replace(/^ {0,3}([-+*]|\d{1,9}[.)]) +/, '');
      let task = null, t = body;
      const tm = body.match(/^\[([ xX])\] +(.*)/);
      if (tm) { task = tm[1].toLowerCase() === 'x' ? 'done' : 'open'; t = tm[2]; }
      return { task, children: [{ type: 'Paragraph', children: parseInline(t) }] };
    });
    return { type: 'List', ordered: k === 'ordered-list', start: 1, items };
  }
  if (k.startsWith('code-block-fenced')) {
    const lines = bare.split('\n');
    const lang = lines[0].replace(/^ {0,3}`{3,}\s*/, '').replace(/^ {0,3}~{3,}\s*/, '').trim();
    const body = lines.slice(1, lines.length > 1 ? lines.length - 1 : 1).join('\n');
    return { type: 'CodeBlock', fenced: true, language: lang, content: body };
  }
  if (k === 'code-block-indented') {
    return { type: 'CodeBlock', fenced: false, language: '', content: bare.split('\n').map((l) => l.replace(/^(    |\t)/, '')).join('\n') };
  }
  if (k === 'table') {
    const rows = bare.split('\n');
    const cellRow = (r) => r.replace(/^\s*\|/, '').replace(/\|\s*$/, '').split('|').map((c) => ({ align: 'none', children: parseInline(c.trim()) }));
    return { type: 'Table', alignments: [], header: cellRow(rows[0]), rows: rows.slice(2).map(cellRow) };
  }
  if (k === 'html-block') return { type: 'HtmlBlock', content: bare };
  if (k === 'link-ref-def') {
    const m = bare.match(/^\s*\[([^\]]+)\]:\s*(\S+)\s*(?:"([^"]*)")?/);
    return { type: 'LinkRefDef', label: m ? m[1] : '', destination: m ? m[2] : '', title: m && m[3] ? m[3] : null };
  }
  return { type: 'Paragraph', children: parseInline(bare) };
}

// ── Command dispatcher ──────────────────────────────────────────────────
function expandTemplate(re, matchedText, template) {
  const caps = re.exec(matchedText);
  if (!caps) return template;
  return template.replace(/\$(\$|\d+|[a-zA-Z_]\w*|&)/g, (_, g) => {
    if (g === '$') return '$';
    if (g === '&' || g === '0') return caps[0];
    const named = caps.groups && caps.groups[g];
    if (named !== undefined) return named;
    const idx = parseInt(g, 10);
    return caps[idx] ?? '';
  });
}

function buildRe(query, cs, isRe) {
  const flags = (cs ? '' : 'i') + 'g';
  return isRe ? new RegExp(query, flags) : new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), flags);
}

const commands = {
  get_tabs: () => ({
    tabs: state.tabs.map((t) => ({
      id: t.id, file_name: t.name, file_path: t.path, is_dirty: t.dirty,
    })),
    active: activeTab()?.id ?? 0,
  }),
  new_document: () => docInfo(newTab('untitled.md', null, '')),
  open_welcome: () => {
    const ex = state.tabs.findIndex((t) => !t.path && t.metaId.startsWith('doc-welcome'));
    if (ex >= 0) { state.active = ex; return docInfo(state.tabs[ex]); }
    const t = newTab('welcome.md', null, WELCOME_MD); t.metaId = 'doc-welcome';
    return docInfo(t);
  },
  open_document: ({ path }) => {
    const ex = state.tabs.findIndex((t) => t.path === path);
    if (ex >= 0) { state.active = ex; return docInfo(state.tabs[ex]); }
    const content = vfs.get(path);
    if (content === undefined) throw new Error(`file not found: ${path}`);
    return docInfo(newTab(path.split('/').pop(), path, content));
  },
  switch_tab: ({ tabId }) => {
    const i = state.tabs.findIndex((t) => t.id === tabId);
    if (i < 0) throw new Error('tab not found');
    state.active = i; return docInfo(state.tabs[i]);
  },
  close_tab: ({ tabId }) => {
    const i = state.tabs.findIndex((t) => t.id === tabId);
    if (i < 0) throw new Error('tab not found');
    const was = i === state.active;
    state.tabs.splice(i, 1);
    if (!state.tabs.length) { state.active = -1; return null; }
    if (was) state.active = Math.min(i, state.tabs.length - 1);
    else if (i < state.active) state.active--;
    return docInfo(activeTab());
  },
  save_document: ({ path }) => {
    const t = activeTab(); if (!t) throw new Error('no active tab');
    const p = path || t.path;
    if (!p) throw new Error('no file path to save to');
    vfs.set(p, toS(t.bytes));
    t.path = p; t.name = p.split(/[\\/]/).pop(); t.dirty = false;
    return true;
  },
  get_document_text: () => toS(activeTab().bytes),
  get_parsed_offset: () => { const l = activeTab().bytes.length; return [l, l]; },
  parse_next_chunk: () => { const l = activeTab().bytes.length; return [l, l, parseBlocks(toS(activeTab().bytes)).length]; },
  get_syntax_tree_meta: () => parseBlocks(toS(activeTab().bytes)),
  get_blocks: () => parseBlocks(toS(activeTab().bytes)).map((b, i) => ({ index: i, kind: b.kind, start: b.start, end: b.end })),
  get_block_data: ({ blockIndex }) => {
    const t = activeTab();
    const blocks = parseBlocks(toS(t.bytes));
    const b = blocks[blockIndex];
    if (!b) return null;
    const text = toS(t.bytes);
    (window.__gbd = window.__gbd || []).push([blockIndex, JSON.stringify(toS(t.bytes.slice(b.start, b.end))).slice(0, 90)]);
    return { kind: b.kind, source: text.slice(
      // byte offsets → JS string indexes: decode slice directly
      0, 0) || toS(t.bytes.slice(b.start, b.end)),
      start: b.start, end: b.end, node: blockNode({ kind: b.kind, start: 0, end: b.end - b.start }, toS(t.bytes.slice(b.start, b.end))) };
  },
  get_block_line_number: ({ blockIndex }) => {
    const blocks = parseBlocks(toS(activeTab().bytes));
    return lineOf(activeTab(), blocks[blockIndex]?.start ?? 0);
  },
  get_block_line_numbers: ({ startIndex, count }) => {
    const blocks = parseBlocks(toS(activeTab().bytes));
    return blocks.slice(startIndex, startIndex + count).map((b) => lineOf(activeTab(), b.start));
  },
  is_dirty: () => !!activeTab()?.dirty,
  get_file_name: () => activeTab()?.name ?? 'untitled.md',
  get_active_file_path: () => activeTab()?.path ?? null,
  get_tear_off_file: () => null,

  replace_text: ({ args }) => {
    const t = activeTab();
    splice(t, args.start, args.end, toB(args.newText ?? args.new_text ?? ''));
    return editResult(t);
  },
  insert_text: ({ position, text }) => {
    const t = activeTab();
    splice(t, position, position, toB(text));
    return editResult(t);
  },
  replace_block: ({ blockIndex, newSource, expectedStart, expectedEnd }) => {
    const t = activeTab();
    const blocks = parseBlocks(toS(t.bytes));
    const b = blocks[blockIndex];
    if (!b) throw new Error('invalid block index');
    if (expectedStart != null && expectedStart !== b.start) throw new Error(`block moved — document changed, please retry the edit (block ${blockIndex}: expected start ${expectedStart}, actual ${b.start})`);
    if (expectedEnd != null && expectedEnd !== b.end) throw new Error(`block moved — document changed, please retry the edit (block ${blockIndex}: expected end ${expectedEnd}, actual ${b.end})`);
    if (toS(t.bytes.slice(b.start, b.end)) === newSource) return editResult(t, false);
    splice(t, b.start, b.end, toB(newSource));
    return editResult(t);
  },
  search_document: ({ args }) => {
    const text = toS(activeTab().bytes);
    if (!args.query) return { valid: true, matches: [], truncated: false };
    let re;
    try { re = buildRe(args.query, args.caseSensitive, args.regex); }
    catch { return { valid: false, matches: [], truncated: false }; }
    const matches = [];
    let m;
    while ((m = re.exec(text)) && matches.length < 100000) {
      matches.push({ start: bLen(text.slice(0, m.index)), end: bLen(text.slice(0, m.index)) + bLen(m[0]) });
      if (m[0] === '') re.lastIndex++;
    }
    return { valid: true, matches, truncated: m != null };
  },
  replace_match_in_document: ({ args }) => {
    const t = activeTab();
    const text = toS(t.bytes);
    const start = args.start, end = args.end;
    // byte → char index
    const cStart = toS(t.bytes.slice(0, start)).length;
    const cEnd = cStart + toS(t.bytes.slice(start, end)).length;
    let re;
    try { re = buildRe(args.query, args.caseSensitive, args.regex); } catch { throw new Error('invalid regular expression'); }
    const matched = text.slice(cStart, cEnd);
    re.lastIndex = 0;
    const m = re.exec(matched);
    if (!m || m.index !== 0 || m[0].length !== matched.length) {
      throw new Error('match no longer matches — document changed, please search again');
    }
    const expanded = args.regex ? expandTemplate(re, matched, args.replacement) : args.replacement;
    splice(t, start, end, toB(expanded));
    return editResult(t);
  },
  replace_all_in_document: ({ args }) => {
    const t = activeTab();
    const text = toS(t.bytes);
    let re;
    try { re = buildRe(args.query, args.caseSensitive, args.regex); } catch { throw new Error('invalid regular expression'); }
    const matches = [];
    let m;
    const reAll = new RegExp(re.source, re.flags.includes('g') ? re.flags : re.flags + 'g');
    while ((m = reAll.exec(text))) {
      matches.push({ s: m.index, e: m.index + m[0].length, t: m[0] });
      if (m[0] === '') reAll.lastIndex++;
    }
    if (!matches.length) return { count: 0, edit: editResult(t, false) };
    const one = new RegExp(re.source, re.flags.replace('g', ''));
    for (let i = matches.length - 1; i >= 0; i--) {
      const mm = matches[i];
      const bs = bLen(text.slice(0, mm.s)), be = bs + bLen(mm.t);
      const expanded = args.regex ? expandTemplate(one, mm.t, args.replacement) : args.replacement;
      splice(t, bs, be, toB(expanded));
    }
    return { count: matches.length, edit: editResult(t) };
  },
  undo: () => {
    const t = activeTab();
    if (!t.undo.length) return editResult(t, false);
    t.redo.push(t.bytes); t.bytes = t.undo.pop();
    t.dirty = true;
    return editResult(t);
  },
  redo: () => {
    const t = activeTab();
    if (!t.redo.length) return editResult(t, false);
    t.undo.push(t.bytes); t.bytes = t.redo.pop();
    t.dirty = true;
    return editResult(t);
  },

  // ── File tree ──
  list_directory: ({ dirPath }) => {
    const prefix = dirPath.endsWith('/') ? dirPath : dirPath + '/';
    const seen = new Map();
    for (const p of vfs.keys()) {
      if (!p.startsWith(prefix)) continue;
      const rest = p.slice(prefix.length);
      const seg = rest.split('/')[0];
      const isDir = rest.includes('/');
      if (!seg || seg.startsWith('.')) continue;
      seen.set(seg, { name: seg, path: prefix + seg, is_dir: isDir, size: isDir ? 0 : vfs.get(p).length });
    }
    if (!seen.size && dirPath !== '/repo' && dirPath !== '/') throw new Error(`Path does not exist: ${dirPath}`);
    const entries = [...seen.values()].sort((a, b) => (b.is_dir - a.is_dir) || a.name.localeCompare(b.name));
    if (dirPath === '/' || dirPath === '') return [{ name: 'repo', path: '/repo', is_dir: true, size: 0 }];
    return entries;
  },
  get_parent_dir: ({ dirPath }) => {
    const p = dirPath.replace(/[\\/]+$/, '');
    const i = p.lastIndexOf('/');
    return i > 0 ? p.slice(0, i) : (i === 0 ? '/' : null);
  },
  create_file: ({ path }) => { if (vfs.has(path)) throw new Error('exists'); vfs.set(path, ''); return true; },
  create_directory: () => true,
  delete_file: ({ path }) => { vfs.delete(path); for (const k of [...vfs.keys()]) if (k.startsWith(path + '/')) vfs.delete(k); return true; },
  move_file: ({ srcPath, destPath }) => {
    if (!vfs.has(srcPath)) throw new Error('Source does not exist');
    vfs.set(destPath, vfs.get(srcPath)); vfs.delete(srcPath); return true;
  },
  copy_file: ({ srcPath, destPath }) => {
    if (!vfs.has(srcPath)) throw new Error('Source does not exist');
    vfs.set(destPath, vfs.get(srcPath)); return true;
  },
  read_image_file: () => 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  open_url: () => true,
  open_relative_file: ({ relativePath }) => {
    // Resolve against the active document's directory; reject escapes above
    // the repo root like the real backend's canonicalize guard does.
    const dir = (activeTab()?.path || '/repo/x').replace(/[^/]*$/, '');
    const parts = [];
    for (const seg of (dir + relativePath).split('/')) {
      if (!seg || seg === '.') continue;
      if (seg === '..') { if (!parts.length) throw new Error('path escapes root'); parts.pop(); }
      else parts.push(seg);
    }
    const full = '/' + parts.join('/');
    if (!vfs.has(full)) throw new Error(`file not found: ${full}`);
    const ex = state.tabs.findIndex((t) => t.path === full);
    if (ex >= 0) { state.active = ex; return docInfo(state.tabs[ex]); }
    return docInfo(newTab(full.split('/').pop(), full, vfs.get(full)));
  },

  // ── Git (stateful mini-repo: stage/commit actually move files) ──
  git_repo_root: () => '/repo',
  get_git_status: () => ({
    branch: gitState.branch, staged: [...gitState.staged],
    changes: [...gitState.changes], untracked: [...gitState.untracked],
    conflicted: [], dirty: gitState.changes.length + gitState.staged.length > 0,
  }),
  git_branches: () => gitState.branches.map((b) => ({ ...b, is_current: b.name === gitState.branch })),
  git_log: () => [...gitState.log],
  git_file_history: () => [],
  git_stash_list: () => [...gitState.stash],
  git_tags: () => [...gitState.tags],
  git_remotes: () => [...gitState.remotes],
  git_diff: () => gitState.changes.map((f) => ({ path: f.path, old_path: null, hunks: [] })),
  git_diff_file: ({ filePath: path }) => ({ path, old_path: null, hunks: [] }),
  git_read_file_at_revision: ({ filePath, revision }) => {
    if (!revision) { const c = vfs.get('/repo/' + filePath) ?? vfs.get(filePath); if (c == null) throw new Error('not found'); return c; }
    return vfs.get('/repo/' + filePath) ?? '';
  },
  git_diff_vs_commit: () => [],
  git_diff_commits: () => [],
  git_diff_file_vs_commit: ({ filePath: path }) => ({ path, old_path: null, hunks: [] }),
  git_diff_file_commits: ({ filePath: path }) => ({ path, old_path: null, hunks: [] }),
  git_stage_file: ({ filePath: path }) => {
    const i = gitState.changes.findIndex((f) => f.path === path);
    const j = gitState.untracked.findIndex((f) => f.path === path);
    if (i >= 0) gitState.staged.push(gitState.changes.splice(i, 1)[0]);
    else if (j >= 0) gitState.staged.push({ ...gitState.untracked.splice(j, 1)[0], status: 'Added' });
    else throw new Error(`no such change: ${path}`);
    return true;
  },
  git_unstage_file: ({ filePath: path }) => {
    const i = gitState.staged.findIndex((f) => f.path === path);
    if (i < 0) throw new Error(`not staged: ${path}`);
    gitState.changes.push({ ...gitState.staged.splice(i, 1)[0], status: 'Modified' });
    return true;
  },
  git_stage_all: () => {
    gitState.staged.push(...gitState.changes.splice(0));
    gitState.staged.push(...gitState.untracked.splice(0).map((f) => ({ ...f, status: 'Added' })));
    return true;
  },
  git_discard_file: ({ filePath: path }) => {
    const i = gitState.changes.findIndex((f) => f.path === path);
    if (i < 0) throw new Error(`no such change: ${path}`);
    gitState.changes.splice(i, 1);
    return true;
  },
  git_remove_untracked: ({ filePath: path }) => {
    const i = gitState.untracked.findIndex((f) => f.path === path);
    if (i < 0) throw new Error(`no such file: ${path}`);
    gitState.untracked.splice(i, 1);
    vfs.delete('/repo/' + path);
    return true;
  },
  git_commit: ({ message }) => {
    if (!message?.trim()) throw new Error('empty commit message');
    if (!gitState.staged.length) throw new Error('nothing staged');
    const sha = Math.random().toString(16).slice(2, 18);
    gitState.log.unshift({ sha, author: 'Dev', date: '2024-06-01', message });
    gitState.staged.length = 0;
    return sha;
  },
  git_checkout: ({ branch }) => {
    if (!gitState.branches.some((b) => b.name === branch)) throw new Error(`no branch: ${branch}`);
    if (gitState.staged.length || gitState.changes.length) throw new Error('worktree dirty');
    gitState.branch = branch;
    return true;
  },
  git_create_branch: ({ name }) => {
    if (gitState.branches.some((b) => b.name === name)) throw new Error('exists');
    gitState.branches.push({ name, is_remote: false, is_current: false, upstream: null, ahead: 0, behind: 0 });
    return true;
  },
  git_delete_branch: ({ name }) => {
    const i = gitState.branches.findIndex((b) => b.name === name);
    if (i < 0 || name === gitState.branch) throw new Error('cannot delete');
    gitState.branches.splice(i, 1);
    return true;
  },
  git_stash_push: () => {
    if (!gitState.changes.length && !gitState.staged.length) throw new Error('nothing to stash');
    gitState.stash.unshift({ index: 0, message: 'WIP', date: '2024-06-01' });
    gitState.changes.length = 0; gitState.staged.length = 0;
    return true;
  },
  git_stash_pop: () => { const s = gitState.stash.shift(); if (!s) throw new Error('no stash'); gitState.changes.push({ path: 'notes.md', status: 'Modified', old_path: null }); return true; },
  git_stash_apply: () => { if (!gitState.stash.length) throw new Error('no stash'); gitState.changes.push({ path: 'notes.md', status: 'Modified', old_path: null }); return true; },
  git_stash_drop: ({ index }) => { gitState.stash.splice(index ?? 0, 1); return true; },
  git_create_tag: ({ name }) => { gitState.tags.push({ name, sha: 'abc1234', message: '' }); return true; },
  git_delete_tag: ({ name }) => { gitState.tags.splice(gitState.tags.findIndex((t) => t.name === name), 1); return true; },
  git_add_remote: ({ name, url }) => { gitState.remotes.push({ name, url }); return true; },
  git_fetch_remote: () => true, git_pull_from_remote: () => true, git_push_to_remote: () => true,
  git_merge: () => true, git_rebase: () => true, git_cherry_pick: () => true, git_revert: () => true,

  // ── GitHub ──
  github_auth_status: () => { throw new Error('gh not installed'); },
  github_repo_metadata: () => { throw new Error('not a github repo'); },
  github_pull_requests: () => { throw new Error('gh not installed'); },
  github_remote_branches: () => { throw new Error('gh not installed'); },
  github_login: () => { throw new Error('gh not installed'); },
  github_logout: () => { throw new Error('gh not installed'); },
};

function lineOf(t, byteStart) {
  const text = toS(t.bytes.slice(0, byteStart));
  return text.split('\n').length;
}

// ── __TAURI_INTERNALS__ plumbing ────────────────────────────────────────
let cbSeq = 1;
const callbacks = new Map();

window.__mockInvokeCalls = [];
window.__tauriMock = { state, vfs, callbacks, docInfo };

async function mockInvoke(cmd, args) {
  window.__mockInvokeCalls.push([cmd, args, window.__step || '?']);
  // plugin commands
  if (cmd === 'plugin:dialog|open') {
    const r = window.__mockDialogOpen;
    window.__mockDialogOpen = undefined;
    return r === undefined ? null : r;
  }
  if (cmd === 'plugin:dialog|save') {
    const r = window.__mockDialogSave;
    window.__mockDialogSave = undefined;
    return r === undefined ? null : r;
  }
  if (cmd === 'plugin:event|listen') return Math.floor(Math.random() * 1e9);
  if (cmd.startsWith('plugin:event|')) return null;
  if (cmd === 'plugin:updater|check') return null;
  if (cmd.startsWith('plugin:updater|')) return null;
  if (cmd.startsWith('plugin:window|')) return null;
  if (cmd.startsWith('plugin:webview|')) return null;
  if (cmd.startsWith('plugin:os|')) return null;
  const fn = commands[cmd];
  if (!fn) {
    console.warn('[mock] unhandled command', cmd, args);
    return null;
  }
  const out = fn(args || {});
  // Simulate async IPC.
  await new Promise((r) => setTimeout(r, 0));
  return out;
}

window.__TAURI_INTERNALS__ = {
  invoke: (cmd, args) => mockInvoke(cmd, args),
  transformCallback: (cb, once) => {
    const id = cbSeq++;
    callbacks.set(id, (data) => { if (once) callbacks.delete(id); return cb && cb(data); });
    return id;
  },
  unregisterCallback: (id) => callbacks.delete(id),
  runCallback: (id, data) => { const cb = callbacks.get(id); if (cb) cb(data); },
  callbacks,
  metadata: {
    currentWindow: { label: 'main' },
    currentWebview: { label: 'main' },
    currentWebviewWindow: { label: 'main' },
  },
  convertFileSrc: (p) => `asset://localhost/${p}`,
  os: { platform: 'windows' },
};
