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
 * The dominant line ending of a text: the most frequent of the three
 * families ("\r\n" pairs, lone "\r", lone "\n"), preferring "\r\n" then
 * "\n" on ties. A lone `\r` is a real line ending (classic Mac) — a CR-only
 * file must restore `\n` back to `\r`, not stay LF.
 */
export function dominantLineEnding(text) {
  const s = String(text);
  const pairs = (s.match(/\r\n/g) || []).length;
  const crs = (s.match(/\r(?!\n)/g) || []).length;
  const lfs = (s.match(/(?<!\r)\n/g) || []).length;
  if (pairs > 0 && pairs >= crs && pairs >= lfs) return '\r\n';
  if (crs > lfs) return '\r';
  return '\n';
}

/**
 * Restore the original block's line endings in edited textarea output.
 * `original` is the raw block source as loaded from the backend; `edited`
 * is the LF-only text coming out of the textarea. Every `\n` in the edited
 * text becomes the block's dominant ending so unchanged lines keep their
 * original bytes and the edit diff stays minimal.
 */
export function restoreLineEndings(original, edited) {
  if (String(edited).includes('\r')) return edited;
  const ending = dominantLineEnding(original);
  if (ending === '\r\n') return String(edited).replace(/\n/g, '\r\n');
  if (ending === '\r') return String(edited).replace(/\n/g, '\r');
  return edited;
}

/**
 * True when the edited textarea text differs from the original in CONTENT —
 * i.e. not merely because of API line-ending normalization.
 */
export function contentChanged(original, edited) {
  return normalizeForTextarea(original) !== normalizeForTextarea(edited);
}

/**
 * Classify a document's line-ending style for display: 'CRLF', 'CR', 'LF',
 * or 'Mixed' when more than one family is present. Same rules as the
 * backend's `detect_line_ending`: a single stray CRLF in an LF file (or
 * vice versa) is Mixed, not a mislabeled dominant type. Unlike
 * `dominantLineEnding` (which picks a style to restore), every family
 * counts once here — presence, not majority, decides Mixed.
 */
export function detectLineEnding(text) {
  const s = String(text);
  let crlf = 0, lf = 0, cr = 0;
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '\r') {
      if (s[i + 1] === '\n') { crlf++; i++; } else { cr++; }
    } else if (s[i] === '\n') {
      lf++;
    }
  }
  const kinds = (crlf > 0) + (lf > 0) + (cr > 0);
  if (kinds > 1) return 'Mixed';
  if (crlf) return 'CRLF';
  if (cr) return 'CR';
  return 'LF';
}

/**
 * Classify from a bounded prefix sample. Truncating right between the `\r`
 * and `\n` of a CRLF pair would leave a dangling `\r` that reads as a bare
 * CR — mislabeling a pure-CRLF file as Mixed (or CR when the pair was the
 * only break in the sample). The dangling byte is dropped before counting.
 */
export function detectLineEndingSampled(text, limit = 4096) {
  const s = String(text);
  let sample = s.substring(0, limit);
  if (s.length > sample.length && sample.endsWith('\r')) {
    sample = sample.slice(0, -1);
  }
  return detectLineEnding(sample);
}
