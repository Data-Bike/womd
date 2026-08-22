import { describe, it, expect } from 'vitest';
import {
  wrapSelection,
  toggleLinePrefix,
  toggleHeading,
  insertText,
  insertAtLineStart,
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
});
