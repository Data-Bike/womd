// Pure text-search helpers used by MarkdownEditor.vue's find/replace engine.
// Kept dependency-free (no DOM, no Vue) so match-finding and replacement
// math can be unit tested directly.

/**
 * Find every occurrence of `query` in `value`.
 *
 * Returns an array of `{ start, end, groups }` ranges (sorted, non-overlapping),
 * or `null` if `regex` is true and `query` is not a valid regular expression
 * (callers use this to show an "invalid regex" state instead of crashing).
 *
 * `groups` is the raw `RegExp.exec()` match array (only meaningful when
 * `regex` is true) — it's what lets replacement templates use `$1`, `$&`, etc.
 */
export function findMatches(value, query, { caseSensitive = false, regex = false } = {}) {
  if (!query) return [];

  if (regex) {
    let re;
    try {
      re = new RegExp(query, caseSensitive ? 'g' : 'gi');
    } catch {
      return null;
    }
    const matches = [];
    let m;
    while ((m = re.exec(value)) !== null) {
      matches.push({ start: m.index, end: m.index + m[0].length, groups: m });
      // Avoid an infinite loop on zero-length matches (e.g. `a*`).
      if (m[0].length === 0) re.lastIndex += 1;
    }
    return matches;
  }

  const hay = caseSensitive ? value : value.toLowerCase();
  const needle = caseSensitive ? query : query.toLowerCase();
  const matches = [];
  let from = 0;
  for (;;) {
    const idx = hay.indexOf(needle, from);
    if (idx < 0) break;
    matches.push({ start: idx, end: idx + needle.length, groups: null });
    from = idx + needle.length;
  }
  return matches;
}

/** Index of the first match starting at or after `pos`, wrapping to 0. -1 if there are no matches. */
export function matchIndexAtOrAfter(matches, pos) {
  if (!matches.length) return -1;
  const idx = matches.findIndex(m => m.start >= pos);
  return idx < 0 ? 0 : idx;
}

/** Expand `$1`, `$2`, ... `$&`, `$$` in a regex replacement template using a match's capture groups. */
export function expandReplacement(template, groups) {
  if (!groups) return template;
  return template.replace(/\$(\$|&|\d+)/g, (_full, ref) => {
    if (ref === '$') return '$';
    if (ref === '&') return groups[0] ?? '';
    return groups[Number(ref)] ?? '';
  });
}

/** Text to substitute in for a single match, honoring regex capture-group references. */
export function resolveReplacementText(match, template, isRegex) {
  return isRegex ? expandReplacement(template, match.groups) : template;
}

/** Replace a single match in `value`, returning the new string. */
export function replaceMatch(value, match, replacementText) {
  return value.slice(0, match.start) + replacementText + value.slice(match.end);
}

/** Replace every match in `value` in one pass (processed back-to-front so offsets stay valid). */
export function replaceAllMatches(value, matches, template, isRegex) {
  let result = value;
  for (let i = matches.length - 1; i >= 0; i--) {
    const text = resolveReplacementText(matches[i], template, isRegex);
    result = replaceMatch(result, matches[i], text);
  }
  return result;
}
