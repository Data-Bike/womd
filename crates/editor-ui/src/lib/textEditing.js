// Pure text-editing helpers shared by the block-level textarea editor and the
// full-document source view. Each function takes (value, selectionStart,
// selectionEnd, ...) and returns { value, start, end } describing the new
// text and the selection that should be restored — no DOM access, so these
// are unit-testable in isolation from Vue/textarea internals.

/** Wrap (or unwrap, if already wrapped) the selection with prefix/suffix. */
export function wrapSelection(value, start, end, prefix, suffix = prefix) {
  const selected = value.slice(start, end);
  const before = value.slice(0, start);
  const after = value.slice(end);

  if (selected) {
    const hasPrefix = before.endsWith(prefix);
    const hasSuffix = after.startsWith(suffix);
    if (hasPrefix && hasSuffix) {
      return {
        value: before.slice(0, -prefix.length) + selected + after.slice(suffix.length),
        start: start - prefix.length,
        end: end - prefix.length,
      };
    }
    return {
      value: before + prefix + selected + suffix + after,
      start: start + prefix.length,
      end: end + prefix.length,
    };
  }

  return {
    value: before + prefix + suffix + after,
    start: start + prefix.length,
    end: start + prefix.length,
  };
}

// Expand a (possibly mid-line or collapsed) range to whole lines. A caret at
// the very start of a document that begins with '\n' must still resolve to
// line start 0 — `lastIndexOf` with fromIndex -1 is clamped to 0 and would
// find the newline itself, pushing the range past the first line.
function expandToLineRange(value, start, end) {
  const lineStart = start === 0 ? 0 : value.lastIndexOf('\n', start - 1) + 1;
  let lineEnd;
  if (end > start && value[end - 1] === '\n') {
    // The selection already ends exactly on a line boundary — extending to
    // the next '\n' would pull in a line the selection never touched.
    lineEnd = end;
  } else {
    lineEnd = value.indexOf('\n', end);
    if (lineEnd < 0) lineEnd = value.length;
  }
  return [lineStart, lineEnd];
}

/** Toggle a per-line prefix (e.g. "- ", "> ") across every line touching the selection. */
// A list marker at the start of a line: `- `, `* `, `+ `, `1. `, `12) `,
// optionally followed by a task checkbox (`- [x] `). Up to 3 leading
// spaces of indent (CommonMark's list tolerance); the captured indent is
// preserved when the marker is swapped.
export const LIST_MARKER_RE = /^(\s{0,3})(?:[-+*]|\d{1,9}[.)])(?:\s+\[[ xX]\])?\s+/;

/** `prefix` names a list-style marker ("- ", "1. ", "- [ ] ") rather than a wrapper like "> ". */
export function isListPrefix(prefix) {
  return /^(?:[-+*]|\d{1,9}[.)])(?: \[[ xX]\])? $/.test(prefix);
}

// Swap one list marker for another, keeping the indent and the item text:
// "- item" + "1. " -> "1. item" (NOT "1. - item"), "- [x] done" + "- [ ] "
// -> "- [ ] done".
function withReplacedListMarker(line, prefix) {
  const m = line.match(LIST_MARKER_RE);
  return m ? m[1] + prefix + line.slice(m[0].length) : prefix + line;
}

export function toggleLinePrefix(value, start, end, prefix) {
  const [lineStart, lineEnd] = expandToLineRange(value, start, end);
  const before = value.slice(0, lineStart);
  const after = value.slice(lineEnd);
  const selected = value.slice(lineStart, lineEnd);
  const lines = selected.split('\n');
  const listPrefix = isListPrefix(prefix);
  // Blank lines can't carry a prefix — treat them as neutral like
  // applyLineFormat does, or `- a\n\n- b` becomes `- - a\n\n- - b` on toggle.
  // For a list prefix "already prefixed" means the marker EQUALS `prefix`:
  // "- [x] done" is not "- done" and must be swapped, not toggled off —
  // otherwise applying "- " to a task list strips just "- " and leaves the
  // corrupt "[x] done" as plain text.
  const allPrefixed = lines.every(line => {
    if (!line) return true;
    if (!listPrefix) return line.startsWith(prefix);
    const m = line.match(LIST_MARKER_RE);
    return !!m && m[0].slice(m[1].length) === prefix;
  });
  const newLines = allPrefixed
    ? lines.map(line => {
        if (!listPrefix) return line.slice(prefix.length);
        const m = line.match(LIST_MARKER_RE);
        return m ? m[1] + line.slice(m[0].length) : line;
      })
    : lines.map(line => (line ? (listPrefix ? withReplacedListMarker(line, prefix) : prefix + line) : line));
  const replacement = newLines.join('\n');
  return {
    value: before + replacement + after,
    start: lineStart,
    end: lineStart + replacement.length,
  };
}

/** Set/clear an ATX heading level ("# ", "## ", ...) on the first line of the selection. */
export function toggleHeading(value, start, end, level) {
  const [lineStart, lineEnd] = expandToLineRange(value, start, end);
  const before = value.slice(0, lineStart);
  const after = value.slice(lineEnd);
  const selected = value.slice(lineStart, lineEnd);
  const breakIdx = selected.indexOf('\n');
  const firstLine = breakIdx < 0 ? selected : selected.slice(0, breakIdx);
  let rest = breakIdx < 0 ? '' : selected.slice(breakIdx);
  // The selection may be a setext heading ("Title\n==="): its underline is
  // the line right after the first. Rewriting only the first line as ATX
  // would leave a stray `===`/`---` that re-parses as a paragraph — drop
  // that one underline line, keeping anything that follows it.
  rest = rest.replace(/^\n[ \t]*(?:=+|-+)[ \t]*(?=\r?\n|$)/, '');
  const match = firstLine.match(/^(#{1,6})\s+(.*)$/);
  const bareText = match ? match[2] : firstLine;
  const newFirstLine = level > 0 ? `${'#'.repeat(level)} ${bareText}` : bareText;
  const replacement = newFirstLine + rest;
  return {
    value: before + replacement + after,
    start: lineStart,
    end: lineStart + replacement.length,
  };
}

/** Replace the selection with arbitrary text, placing the cursor after it. */
export function insertText(value, start, end, text) {
  const replacement = value.slice(0, start) + text + value.slice(end);
  return { value: replacement, start: start + text.length, end: start + text.length };
}

/** Insert text at the beginning of the line containing `start` (e.g. thematic break). */
export function insertAtLineStart(value, start, text) {
  const [lineStart] = expandToLineRange(value, start, start);
  const replacement = value.slice(0, lineStart) + text + value.slice(lineStart);
  const delta = text.length;
  return { value: replacement, start: start + delta, end: start + delta };
}

/**
 * Insert a self-contained block snippet (thematic break, table, …) at the
 * start of the line containing `start`, separated from preceding content by
 * a blank line when needed. A `---` or `| table |` row dropped directly
 * onto the line after a paragraph does NOT start a new block — `---`
 * becomes a setext heading underline and table rows merge into the
 * paragraph — so the leading blank line is structural, not cosmetic.
 */
export function insertBlockSnippet(value, start, blockText) {
  const [lineStart] = expandToLineRange(value, start, start);
  const before = value.slice(0, lineStart);
  const after = value.slice(lineStart);
  const lead = before === '' || before.endsWith('\n\n') ? '' : '\n';
  const text = lead + blockText;
  return { value: before + text + after, start: lineStart + text.length, end: lineStart + text.length };
}

/** Build a markdown link from selected text (or a placeholder) and a URL. */
export function makeLink(value, start, end, url, placeholder = 'text') {
  const selected = value.slice(start, end).trim() || placeholder;
  return insertText(value, start, end, `[${selected}](${url})`);
}

/** Build a markdown image from selected text (used as alt) and a URL. */
export function makeImage(value, start, end, url, placeholder = 'alt text') {
  const selected = value.slice(start, end).trim() || placeholder;
  return insertText(value, start, end, `![${selected}](${url})`);
}

/**
 * Block kinds whose textarea source is inline markdown where `\` + newline
 * produces a real hard line break. Code blocks and HTML blocks treat `\` as
 * a literal character; headings, thematic breaks and link-ref-defs end at
 * the newline anyway; a newline inside a table row breaks the table.
 */
export function blockTakesHardBreak(kind) {
  return kind === 'paragraph' || kind === 'block-quote'
    || kind === 'ordered-list' || kind === 'unordered-list';
}

/**
 * Insert a hard line break at the caret: a `\` marker plus the newline, so
 * the rendered output actually shows the break — a bare `\n` is a CommonMark
 * softbreak and renders as a space, which reads as "Enter did nothing".
 * The marker is skipped where a break already exists (an odd run of
 * trailing `\`, or 2+ trailing spaces) — another `\` would escape the
 * marker into a literal, or just pile up redundant markers.
 * A caret on an already-blank line gets a plain newline instead: Enter on
 * an empty line means "new block", and the resulting blank line ends this
 * one — the `\` variant would emit an invisible break marker and the block
 * could never be exited by keyboard.
 */
export function insertHardBreak(value, start, end) {
  const before = value.slice(0, start);
  const lineStart = before.lastIndexOf('\n') + 1;
  const lineEnd = value.indexOf('\n', end);
  const restOfLine = value.slice(end, lineEnd < 0 ? value.length : lineEnd);
  if (before.slice(lineStart).trim() === '' && restOfLine.trim() === '') {
    // This blank line ends the paragraph. A `\` marker left on the line
    // above (from the previous Enter) would now sit at the END of the
    // paragraph, where CommonMark renders it as a literal backslash —
    // drop it, so "text, Enter, Enter" yields a clean paragraph split.
    if (lineStart > 0 && endsWithBreakMarker(value.slice(0, lineStart - 1))) {
      const v = value.slice(0, lineStart - 2) + value.slice(lineStart - 1);
      return insertText(v, start - 1, end - 1, '\n');
    }
    return insertText(value, start, end, '\n');
  }
  // Trailing spaces are a break marker only after real content — leading
  // whitespace ("  |text") is indentation, not the end of a text line.
  const hasBreak = endsWithBreakMarker(before) || /\S {2,}$/.test(before.slice(lineStart));
  return insertText(value, start, end, hasBreak ? '\n' : '\\\n');
}

// An odd run of trailing backslashes is a live `\` marker; an even run is
// escaped literal backslashes.
function endsWithBreakMarker(s) {
  let n = 0;
  while (n < s.length && s[s.length - 1 - n] === '\\') n++;
  return n % 2 === 1;
}

/**
 * Drop a hard-break marker left dangling at the end of a block's text
 * (Enter pressed, then the edit was closed): the trailing `\` would render
 * as a literal backslash at the end of the paragraph. Only a marker that
 * is followed by a line break is touched — a backslash the user typed as
 * the very last character is content and stays.
 */
export function stripDanglingHardBreak(value) {
  const tail = value.match(/\s*$/)[0];
  const body = value.slice(0, value.length - tail.length);
  if (!tail.includes('\n') || !endsWithBreakMarker(body)) return value;
  return body.slice(0, -1);
}

/** A minimal 2x2 GFM table template, inserted at the cursor. */
export function makeTable() {
  return '| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |\n';
}

/** Wrap the selection in a fenced code block. */
export function wrapCodeBlock(value, start, end) {
  const selected = value.slice(start, end);
  // The outer fence must be longer than any fence inside the selection —
  // a ``` line in the code would otherwise close the wrapper prematurely.
  let maxRun = 2;
  for (const m of selected.matchAll(/`{3,}|~{3,}/g)) {
    maxRun = Math.max(maxRun, m[0].length);
  }
  const fence = '`'.repeat(maxRun + 1);
  return insertText(value, start, end, fence + '\n' + selected + '\n' + fence + '\n');
}
