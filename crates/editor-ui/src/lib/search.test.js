import { describe, it, expect } from 'vitest';
import {
  findMatches,
  matchIndexAtOrAfter,
  expandReplacement,
  resolveReplacementText,
  replaceMatch,
  replaceAllMatches,
} from './search.js';

describe('findMatches (plain text)', () => {
  it('finds all non-overlapping occurrences, case-insensitive by default', () => {
    const matches = findMatches('Foo bar foo BAR foo', 'foo');
    expect(matches).toEqual([
      { start: 0, end: 3, groups: null },
      { start: 8, end: 11, groups: null },
      { start: 16, end: 19, groups: null },
    ]);
  });

  it('respects case sensitivity when requested', () => {
    const matches = findMatches('Foo foo FOO', 'foo', { caseSensitive: true });
    expect(matches).toEqual([{ start: 4, end: 7, groups: null }]);
  });

  it('returns an empty array for an empty query', () => {
    expect(findMatches('anything', '')).toEqual([]);
  });

  it('returns an empty array when there are no matches', () => {
    expect(findMatches('hello world', 'xyz')).toEqual([]);
  });

  it('handles overlapping-looking needles without infinite looping', () => {
    const matches = findMatches('aaaa', 'aa');
    // Non-overlapping scan: matches at 0-2 and 2-4.
    expect(matches).toEqual([
      { start: 0, end: 2, groups: null },
      { start: 2, end: 4, groups: null },
    ]);
  });
});

describe('findMatches (regex)', () => {
  it('finds matches using a regular expression', () => {
    const matches = findMatches('cat, bat, hat', '[cb]at', { regex: true });
    expect(matches.map(m => [m.start, m.end])).toEqual([[0, 3], [5, 8]]);
  });

  it('returns null for an invalid regular expression instead of throwing', () => {
    expect(findMatches('anything', '(unclosed', { regex: true })).toBeNull();
  });

  it('does not hang on a zero-length-match pattern', () => {
    const matches = findMatches('abc', 'x*', { regex: true });
    // 'x*' matches the empty string at every position (0..=3).
    expect(matches.length).toBe(4);
    expect(matches.every(m => m.start === m.end)).toBe(true);
  });

  it('captures groups for use in replacement templates', () => {
    const matches = findMatches('2024-01-15', '(\\d{4})-(\\d{2})-(\\d{2})', { regex: true });
    expect(matches).toHaveLength(1);
    expect(matches[0].groups[1]).toBe('2024');
    expect(matches[0].groups[2]).toBe('01');
    expect(matches[0].groups[3]).toBe('15');
  });

  it('is case-insensitive by default, case-sensitive when requested', () => {
    expect(findMatches('FOO foo', 'foo', { regex: true })).toHaveLength(2);
    expect(findMatches('FOO foo', 'foo', { regex: true, caseSensitive: true })).toHaveLength(1);
  });
});

describe('matchIndexAtOrAfter', () => {
  const matches = [{ start: 2, end: 4 }, { start: 10, end: 12 }, { start: 20, end: 22 }];

  it('returns the first match at or after the given position', () => {
    expect(matchIndexAtOrAfter(matches, 5)).toBe(1);
    expect(matchIndexAtOrAfter(matches, 10)).toBe(1);
  });

  it('wraps to the first match if the position is past every match', () => {
    expect(matchIndexAtOrAfter(matches, 100)).toBe(0);
  });

  it('returns -1 when there are no matches at all', () => {
    expect(matchIndexAtOrAfter([], 0)).toBe(-1);
  });
});

describe('expandReplacement / resolveReplacementText', () => {
  it('substitutes numbered capture groups', () => {
    const matches = findMatches('2024-01-15', '(\\d{4})-(\\d{2})-(\\d{2})', { regex: true });
    expect(expandReplacement('$3/$2/$1', matches[0].groups)).toBe('15/01/2024');
  });

  it('supports $& (whole match) and $$ (literal dollar sign)', () => {
    const matches = findMatches('price', 'ri', { regex: true });
    expect(expandReplacement('[$&] costs $$5', matches[0].groups)).toBe('[ri] costs $5');
  });

  it('treats the template literally for plain-text (non-regex) matches', () => {
    const matches = findMatches('hello', 'ell');
    expect(resolveReplacementText(matches[0], '$1 stays literal', false)).toBe('$1 stays literal');
  });
});

describe('replaceMatch / replaceAllMatches', () => {
  it('replaces a single match in place', () => {
    const matches = findMatches('foo bar foo', 'foo');
    expect(replaceMatch('foo bar foo', matches[1], 'baz')).toBe('foo bar baz');
  });

  it('replaces every match without corrupting later offsets', () => {
    const matches = findMatches('foo bar foo baz foo', 'foo');
    const result = replaceAllMatches('foo bar foo baz foo', matches, 'X', false);
    expect(result).toBe('X bar X baz X');
  });

  it('replaces every match using per-match regex capture groups', () => {
    const value = 'a=1;b=2;c=3';
    const matches = findMatches(value, '(\\w)=(\\d)', { regex: true });
    const result = replaceAllMatches(value, matches, '$1:$2', true);
    expect(result).toBe('a:1;b:2;c:3');
  });
});
