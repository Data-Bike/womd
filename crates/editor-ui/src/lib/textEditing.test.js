import { describe, it, expect } from 'vitest';
import {
  wrapSelection,
  toggleLinePrefix,
  toggleHeading,
  insertText,
  insertAtLineStart,
  insertBlockSnippet,
  makeLink,
  makeImage,
  makeTable,
  wrapCodeBlock,
} from './textEditing.js';

// These pure functions back both the block-level textarea editor and the
// full-document source view (MarkdownEditor.vue). They used to be
// implemented twice, independently, with subtly different (and in the
// source-view case, buggy) cursor-position math. Testing the single shared
// implementation here guarantees both editing surfaces behave identically.

describe('wrapSelection', () => {
  it('wraps a selection with a prefix/suffix', () => {
    const r = wrapSelection('hello world', 6, 11, '**');
    expect(r.value).toBe('hello **world**');
    expect(r.start).toBe(8);
    expect(r.end).toBe(13);
  });

  it('unwraps an already-wrapped selection (toggle off)', () => {
    const r = wrapSelection('hello **world**', 8, 13, '**');
    expect(r.value).toBe('hello world');
    expect(r.start).toBe(6);
    expect(r.end).toBe(11);
  });

  it('inserts an empty pair and places the caret between them when nothing is selected', () => {
    const r = wrapSelection('hello ', 6, 6, '**');
    expect(r.value).toBe('hello ****');
    expect(r.start).toBe(8);
    expect(r.end).toBe(8);
  });

  it('supports distinct prefix and suffix (links use this indirectly)', () => {
    const r = wrapSelection('note', 0, 4, '<!--', '-->');
    expect(r.value).toBe('<!--note-->');
  });
});

describe('toggleLinePrefix', () => {
  it('adds a prefix to every line touching the selection', () => {
    const value = 'one\ntwo\nthree';
    const r = toggleLinePrefix(value, 0, value.length, '- ');
    expect(r.value).toBe('- one\n- two\n- three');
  });

  it('removes the prefix if every selected line already has it (toggle off)', () => {
    const value = '- one\n- two\n- three';
    const r = toggleLinePrefix(value, 0, value.length, '- ');
    expect(r.value).toBe('one\ntwo\nthree');
  });

  it('does not add a prefix to an empty trailing line', () => {
    const value = 'one\n';
    const r = toggleLinePrefix(value, 0, value.length, '> ');
    expect(r.value).toBe('> one\n');
  });

  it('only affects the selected region, preserving text before/after it', () => {
    const value = 'before\nsel1\nsel2\nafter';
    const start = value.indexOf('sel1');
    const end = value.indexOf('sel2') + 'sel2'.length;
    const r = toggleLinePrefix(value, start, end, '> ');
    expect(r.value).toBe('before\n> sel1\n> sel2\nafter');
  });
});

describe('toggleHeading', () => {
  it('adds a heading prefix to the first line of the selection', () => {
    const r = toggleHeading('Title\nbody', 0, 5, 2);
    expect(r.value).toBe('## Title\nbody');
  });

  it('changes an existing heading level rather than stacking hashes', () => {
    const r = toggleHeading('### Title\nbody', 0, 9, 1);
    expect(r.value).toBe('# Title\nbody');
  });

  it('removes the heading when level is 0', () => {
    const r = toggleHeading('## Title\nbody', 0, 8, 0);
    expect(r.value).toBe('Title\nbody');
  });

  it('preserves the rest of a multi-line selection unchanged', () => {
    const r = toggleHeading('Title\nline two\nline three', 0, 25, 3);
    expect(r.value).toBe('### Title\nline two\nline three');
  });

  it('strips the setext underline when converting a setext heading', () => {
    // "Title\n===" is an H1 — without stripping, the rewrite leaves a stray
    // "===" that re-parses as a paragraph.
    expect(toggleHeading('Title\n===', 0, 9, 1).value).toBe('# Title');
    // Single-char underlines are valid too (CommonMark: one or more).
    expect(toggleHeading('Title\n=', 0, 7, 2).value).toBe('## Title');
    expect(toggleHeading('Title\n-', 0, 7, 2).value).toBe('## Title');
    // The underline may be followed by more selected lines.
    expect(toggleHeading('Title\n---\nrest', 0, 13, 1).value).toBe('# Title\nrest');
    // An underline line in the middle of a multi-line heading selection.
    expect(toggleHeading('Title\n===\nrest\nrest2', 0, 18, 3).value)
      .toBe('### Title\nrest\nrest2');
    // Indented underline (up to 3 spaces) also counts.
    expect(toggleHeading('Title\n  ===  ', 0, 13, 1).value).toBe('# Title');
  });

  it('does not strip a non-underline second line', () => {
    // "- item" is a list line, not an underline run of only `-` chars… but
    // "- item" is not a pure `-` run, so it must survive.
    expect(toggleHeading('Title\n- item', 0, 11, 1).value).toBe('# Title\n- item');
    expect(toggleHeading('Title\n=== x', 0, 11, 1).value).toBe('# Title\n=== x');
  });

  it('expands a collapsed caret to the whole line', () => {
    // A caret mid-word used to inject "## " into the middle of the line.
    expect(toggleHeading('abc def', 4, 4, 2).value).toBe('## abc def');
    // Same for an existing heading — the whole line is rewritten, the
    // caret's neighbours are untouched.
    expect(toggleHeading('# Title', 4, 4, 2).value).toBe('## Title');
    // Multi-line: caret on line 2 only rewrites that line.
    expect(toggleHeading('first\nsecond\nthird', 8, 8, 1).value)
      .toBe('first\n# second\nthird');
  });
});

describe('insertText / insertAtLineStart', () => {
  it('replaces the selection and places the caret after the inserted text', () => {
    const r = insertText('hello world', 6, 11, 'there');
    expect(r.value).toBe('hello there');
    expect(r.start).toBe(11);
    expect(r.end).toBe(11);
  });

  it('inserts at the start of the current line, not at the caret', () => {
    const value = 'first line\nsecond line';
    const caret = value.indexOf('second') + 3; // somewhere mid-way through "second"
    const r = insertAtLineStart(value, caret, '---\n');
    expect(r.value).toBe('first line\n---\nsecond line');
  });

  it('handles the first line of the document (no preceding newline)', () => {
    const r = insertAtLineStart('only line', 4, '---\n');
    expect(r.value).toBe('---\nonly line');
  });

  it('inserts at position 0 for a caret before a leading newline', () => {
    // Regression: lastIndexOf('\n', -1) matched the '\n' at index 0, so the
    // text landed after the first line instead of before it.
    const r = insertAtLineStart('\nabc', 0, '---\n');
    expect(r.value).toBe('---\n\nabc');
  });
});

describe('insertBlockSnippet', () => {
  it('adds a blank line before --- so it does not become a setext underline', () => {
    // Regression: a bare `---` after a paragraph line re-parses the line as
    // an <h2>, silently turning the user's text into a heading.
    const r = insertBlockSnippet('para\nnext', 6, '---\n');
    expect(r.value).toBe('para\n\n---\nnext');
  });

  it('does not double the blank line when one already exists', () => {
    const r = insertBlockSnippet('para\n\nnext', 6, '---\n');
    expect(r.value).toBe('para\n\n---\nnext');
  });

  it('inserts at document start with no separator', () => {
    const r = insertBlockSnippet('abc', 1, '---\n');
    expect(r.value).toBe('---\nabc');
  });

  it('keeps a table out of the preceding paragraph', () => {
    // GFM tables cannot interrupt a paragraph — rows would be swallowed.
    const r = insertBlockSnippet('para\nx', 5, makeTable());
    expect(r.value).toBe('para\n\n' + makeTable() + 'x');
  });

  it('places the caret after the inserted block', () => {
    const r = insertBlockSnippet('a\nb', 2, '---\n');
    expect(r.value).toBe('a\n\n---\nb');
    expect(r.start).toBe(r.value.indexOf('b'));
    expect(r.end).toBe(r.start);
  });
});

describe('makeLink / makeImage / makeTable / wrapCodeBlock', () => {
  it('builds a markdown link from the selected text', () => {
    const r = makeLink('see docs here', 4, 8, 'https://example.com');
    expect(r.value).toBe('see [docs](https://example.com) here');
  });

  it('falls back to a placeholder when nothing is selected', () => {
    const r = makeLink('', 0, 0, 'https://example.com');
    expect(r.value).toBe('[text](https://example.com)');
  });

  it('builds a markdown image with alt text', () => {
    const r = makeImage('diagram', 0, 7, 'https://example.com/x.png');
    expect(r.value).toBe('![diagram](https://example.com/x.png)');
  });

  it('produces a well-formed GFM table template', () => {
    const table = makeTable();
    expect(table).toMatch(/^\|.*\|\n\|[\s\-:|]+\|\n\|.*\|\n$/);
  });

  it('wraps the selection in a fenced code block', () => {
    const r = wrapCodeBlock('const x = 1;', 0, 12);
    expect(r.value).toBe('```\nconst x = 1;\n```\n');
  });

  it('uses a longer outer fence when the selection contains a fence', () => {
    // A ``` line inside the code would close a ``` wrapper prematurely.
    const r = wrapCodeBlock('md example:\n```\ncode\n```', 0, 26);
    expect(r.value).toBe('````\nmd example:\n```\ncode\n```\n````\n');
    // Longer inner runs get an even longer wrapper.
    const r2 = wrapCodeBlock('`````\nx\n`````', 0, 13);
    expect(r2.value).toBe('``````\n`````\nx\n`````\n``````\n');
  });
});

describe('toggleLinePrefix', () => {
  it('adds the prefix to every non-empty line', () => {
    const r = toggleLinePrefix('a\nb', 0, 3, '- ');
    expect(r.value).toBe('- a\n- b');
  });

  it('removes the prefix when every line already has it', () => {
    const r = toggleLinePrefix('- a\n- b', 0, 7, '- ');
    expect(r.value).toBe('a\nb');
  });

  it('treats blank lines as neutral for the all-prefixed check', () => {
    // A blank line inside the selection must not turn the toggle into an
    // "add" pass that doubles existing markers.
    expect(toggleLinePrefix('- a\n\n- b', 0, 7, '- ').value).toBe('a\n\nb');
    // …but still doesn't get a prefix when lines are added.
    expect(toggleLinePrefix('a\n\nb', 0, 4, '- ').value).toBe('- a\n\n- b');
  });

  it('expands a collapsed caret to the current line', () => {
    // A bare caret inside "bcd" must prefix that whole line — inserting
    // "- " mid-word would corrupt the document.
    const r = toggleLinePrefix('abc\nbcd\ndef', 5, 5, '- ');
    expect(r.value).toBe('abc\n- bcd\ndef');
    expect(r.start).toBe(4);
    expect(r.end).toBe(9);
  });

  it('expands a mid-line selection to whole lines', () => {
    // Selection from mid-"bcd" to mid-"def": both whole lines get the
    // prefix; a mid-line prefix would corrupt the text.
    const value = 'abc\nbcd\ndef\nghi';
    const start = value.indexOf('cd');        // mid-bcd
    const end = value.indexOf('e') + 1;        // mid-def
    const r = toggleLinePrefix(value, start, end, '> ');
    expect(r.value).toBe('abc\n> bcd\n> def\nghi');
  });

  it('does not extend a selection that ends on a line boundary', () => {
    // Selecting exactly lines 1-2 (ending at the '\n' before "ghi") must
    // not also prefix "ghi".
    const r = toggleLinePrefix('abc\nbcd\nghi', 0, 8, '- ');
    expect(r.value).toBe('- abc\n- bcd\nghi');
  });

  it('swaps a list marker instead of stacking a new one on top', () => {
    // "Ordered list" on a bullet list must produce "1. item", never the
    // corrupt "1. - item" — a real report: applyLineFormat used to prepend
    // the prefix unconditionally.
    expect(toggleLinePrefix('- one\n- two', 0, 10, '1. ').value).toBe('1. one\n1. two');
    // Task list over bullets replaces the marker wholesale.
    expect(toggleLinePrefix('- one\n- two', 0, 10, '- [ ] ').value).toBe('- [ ] one\n- [ ] two');
    // …including a checked task (the old checkbox marker goes too).
    expect(toggleLinePrefix('- [x] done\n- [ ] open', 0, 21, '- ').value).toBe('- done\n- open');
    // Indent survives the swap.
    expect(toggleLinePrefix('  - nested\n- top', 0, 16, '1. ').value).toBe('  1. nested\n1. top');
    // Non-list prefixes still stack: quote wraps list text.
    expect(toggleLinePrefix('- a', 0, 3, '> ').value).toBe('> - a');
    // Mixed content: plain lines get the prefix, marked lines swap.
    expect(toggleLinePrefix('- a\nb', 0, 5, '1. ').value).toBe('1. a\n1. b');
  });

  it('targets the first line for a caret at document start before a leading newline', () => {
    // Regression: lastIndexOf('\n', -1) is clamped to 0 and finds the '\n'
    // at index 0 itself, pushing lineStart to 1 and prefixing line 2.
    const r = toggleLinePrefix('\nabc', 0, 0, '- ');
    expect(r.value).toBe('\nabc'); // empty first line: nothing to prefix
    expect(r.start).toBe(0);
    // A caret on the second line still prefixes "abc", not a later line.
    const r2 = toggleLinePrefix('\nabc', 2, 2, '- ');
    expect(r2.value).toBe('\n- abc');
  });
});
