// Line-ending normalization for <textarea> editing.
//
// The HTML spec normalizes every line break in a textarea's API value to a
// single `\n`: `ta.value` (and therefore `v-model` text) can never contain
// `\r`. For a CRLF document that means entering a block edit silently
// converts the whole block to LF — the diff balloons from a few bytes to the
// entire block. These helpers preserve the block's original ending style:
// normalize before comparing, restore before committing back to the backend.

/**
 * The line ending a textarea produces (`\n`) for comparison purposes —
 * normalize the raw source the same way the DOM API does.
 * @param {string} raw - raw block source, possibly containing CRLF/CR.
 */
export function normalizeForTextarea(raw) {
  return String(raw).replace(/\r\n/g, '\n').replace(/\r/g, '\n');
}

/**
 * The dominant line ending of a text: "\r\n" if any CRLF pair exists,
 * otherwise "\n" (lone `\r` is treated as LF-style by textarea anyway).
 */
export function dominantLineEnding(text) {
  return String(text).includes('\r\n') ? '\r\n' : '\n';
}

/**
 * Restore the original block's line endings in edited textarea output.
 * `original` is the raw block source as loaded from the backend; `edited`
 * is the LF-only text coming out of the textarea. If the original block was
 * CRLF, every `\n` in the edited text becomes `\r\n` so unchanged lines keep
 * their original bytes and the edit diff stays minimal.
 */
export function restoreLineEndings(original, edited) {
  if (dominantLineEnding(original) === '\r\n' && !String(edited).includes('\r')) {
    return String(edited).replace(/\n/g, '\r\n');
  }
  return edited;
}

/**
 * True when the edited textarea text differs from the original in CONTENT —
 * i.e. not merely because of API line-ending normalization.
 */
export function contentChanged(original, edited) {
  return normalizeForTextarea(original) !== normalizeForTextarea(edited);
}
