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

/** Toggle a per-line prefix (e.g. "- ", "> ") across every line touching the selection. */
export function toggleLinePrefix(value, start, end, prefix) {
  const before = value.slice(0, start);
  const after = value.slice(end);
  const selected = value.slice(start, end);
  const lines = selected.split('\n');
  const allPrefixed = lines.every(line => line.startsWith(prefix));
  const newLines = allPrefixed
    ? lines.map(line => line.slice(prefix.length))
    : lines.map(line => (line ? prefix + line : line));
  const replacement = newLines.join('\n');
  return {
    value: before + replacement + after,
    start,
    end: start + replacement.length,
  };
}

/** Set/clear an ATX heading level ("# ", "## ", ...) on the first line of the selection. */
export function toggleHeading(value, start, end, level) {
  const before = value.slice(0, start);
  const after = value.slice(end);
  const selected = value.slice(start, end);
  const breakIdx = selected.indexOf('\n');
  const firstLine = breakIdx < 0 ? selected : selected.slice(0, breakIdx);
  const rest = breakIdx < 0 ? '' : selected.slice(breakIdx);
  const match = firstLine.match(/^(#{1,6})\s+(.*)$/);
  const bareText = match ? match[2] : firstLine;
  const newFirstLine = level > 0 ? `${'#'.repeat(level)} ${bareText}` : bareText;
  const replacement = newFirstLine + rest;
  return {
    value: before + replacement + after,
    start,
    end: start + replacement.length,
  };
}

/** Replace the selection with arbitrary text, placing the cursor after it. */
export function insertText(value, start, end, text) {
  const replacement = value.slice(0, start) + text + value.slice(end);
  return { value: replacement, start: start + text.length, end: start + text.length };
}

/** Insert text at the beginning of the line containing `start` (e.g. thematic break). */
export function insertAtLineStart(value, start, text) {
  const lineStart = value.lastIndexOf('\n', start - 1) + 1;
  const replacement = value.slice(0, lineStart) + text + value.slice(lineStart);
  const delta = text.length;
  return { value: replacement, start: start + delta, end: start + delta };
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

/** A minimal 2x2 GFM table template, inserted at the cursor. */
export function makeTable() {
  return '| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |\n';
}

/** Wrap the selection in a fenced code block. */
export function wrapCodeBlock(value, start, end) {
  const selected = value.slice(start, end);
  return insertText(value, start, end, '```\n' + selected + '\n```\n');
}
