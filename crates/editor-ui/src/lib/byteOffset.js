// Conversion between UTF-8 byte offsets (what the Rust backend deals in —
// DocumentBuffer, search_document, block spans, etc. all count bytes) and
// JavaScript string indices (UTF-16 code units — what `.slice()`, `.length`,
// and <textarea>.selectionStart/selectionEnd all actually mean).
//
// These are the same number only for pure ASCII text. Any other character
// diverges: e.g. Cyrillic/Greek/accented Latin are 2 bytes in UTF-8 but 1
// UTF-16 code unit, CJK characters are typically 3 bytes but 1 code unit,
// and astral characters (many emoji) are 4 bytes *and* 2 UTF-16 code units
// (a surrogate pair). Using a byte offset directly as a selectionStart/
// selectionEnd (or vice versa) silently drifts by one unit per such
// character before the position in question — exactly the "selection is off
// by about the length of the Cyrillic text" bug this module fixes.

/** UTF-8 byte length of a single Unicode code point. */
function utf8Length(codePoint) {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

/**
 * Convert a UTF-8 `byteOffset` (as returned by the backend) into the
 * corresponding JavaScript string index into `text`. Clamps to
 * `text.length` if `byteOffset` is at or beyond the end of the string.
 *
 * Offsets that land in the middle of a multi-byte UTF-8 character are
 * floored to the start of that character, so the returned index is always
 * a valid cursor/selection boundary.
 */
export function byteOffsetToIndex(text, byteOffset) {
  if (byteOffset <= 0) return 0;
  let start = 0; // byte at which the current code point begins
  let i = 0;
  while (i < text.length) {
    const codePoint = text.codePointAt(i);
    const len = utf8Length(codePoint);
    if (byteOffset < start + len) return i; // inside or at the start of this char
    start += len;
    i += codePoint > 0xffff ? 2 : 1; // surrogate pairs occupy two JS indices
  }
  return text.length;
}

/**
 * Convert a JavaScript string index into `text` (e.g. from
 * textarea.selectionStart) into the corresponding UTF-8 byte offset.
 */
export function indexToByteOffset(text, index) {
  if (index <= 0) return 0;
  const clamped = Math.min(index, text.length);
  let bytes = 0;
  let i = 0;
  while (i < clamped) {
    const codePoint = text.codePointAt(i);
    bytes += utf8Length(codePoint);
    i += codePoint > 0xffff ? 2 : 1;
  }
  return bytes;
}

/**
 * Convert a UTF-8 byte offset into a `<textarea>` selection index.
 *
 * `<textarea>` values are "API values" per the HTML spec: every line break
 * (`\r`, `\r\n`, or `\n`) is normalized to a single `\n` before the value is
 * exposed to JavaScript. That means the string the user selects from
 * (`ta.value`) can have fewer characters than the raw UTF-8 bytes the Rust
 * backend operates on — one fewer for every `\r` in the original source.
 *
 * `rawText` must be the original string that corresponds to the backend bytes
 * (i.e. the same characters with any `\r` still intact). The returned index is
 * valid for the line-feed-normalized `ta.value`.
 */
export function byteOffsetToTextareaIndex(rawText, byteOffset) {
  if (byteOffset <= 0) return 0;
  const rawIndex = byteOffsetToIndex(rawText, byteOffset);
  // The HTML textarea API value normalizes every line ending (CR, CRLF, LF)
  // to a single LF. Only the CR in a CRLF pair is actually removed; a bare
  // CR is replaced by an LF keeping the same character count.
  let crlfCount = 0;
  for (let i = 0; i < rawIndex; i++) {
    if (rawText[i] === '\r' && rawText[i + 1] === '\n') crlfCount++;
  }
  return Math.max(0, rawIndex - crlfCount);
}
