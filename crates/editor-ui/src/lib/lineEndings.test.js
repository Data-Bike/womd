import { describe, it, expect } from 'vitest';
import {
  normalizeForTextarea,
  dominantLineEnding,
  restoreLineEndings,
  contentChanged,
} from './lineEndings.js';

describe('normalizeForTextarea', () => {
  it('converts CRLF and lone CR to LF like the textarea API', () => {
    expect(normalizeForTextarea('a\r\nb\nc\rd')).toBe('a\nb\nc\nd');
  });

  it('leaves LF text untouched', () => {
    expect(normalizeForTextarea('a\nb\n')).toBe('a\nb\n');
  });
});

describe('dominantLineEnding', () => {
  it('detects CRLF when any pair exists', () => {
    expect(dominantLineEnding('a\r\nb\n')).toBe('\r\n');
    expect(dominantLineEnding('a\r\nb\r\n')).toBe('\r\n');
  });

  it('reports LF otherwise', () => {
    expect(dominantLineEnding('a\nb\n')).toBe('\n');
    expect(dominantLineEnding('a\rb')).toBe('\n');
    expect(dominantLineEnding('')).toBe('\n');
  });
});

describe('restoreLineEndings', () => {
  it('restores CRLF for a CRLF original', () => {
    const original = 'line one\r\nline two\r\n';
    const edited = 'line one\nline two edited\n';
    expect(restoreLineEndings(original, edited)).toBe('line one\r\nline two edited\r\n');
  });

  it('keeps LF for an LF original', () => {
    expect(restoreLineEndings('a\nb\n', 'a\nb2\n')).toBe('a\nb2\n');
  });

  it('is a no-op when the edited text already has CRLF', () => {
    expect(restoreLineEndings('a\r\n', 'a\r\nb\r\n')).toBe('a\r\nb\r\n');
  });

  it('round-trips an unchanged CRLF block byte-identically', () => {
    const original = 'a\r\nb\r\nc\r\n';
    const edited = normalizeForTextarea(original);
    expect(restoreLineEndings(original, edited)).toBe(original);
  });
});

describe('contentChanged', () => {
  it('ignores pure line-ending normalization', () => {
    const original = 'a\r\nb\r\n';
    expect(contentChanged(original, normalizeForTextarea(original))).toBe(false);
  });

  it('detects real edits', () => {
    expect(contentChanged('a\r\nb\r\n', 'a\nX\n')).toBe(true);
    expect(contentChanged('a\nb\n', 'a\nb\n')).toBe(false);
    expect(contentChanged('a\nb\n', 'a\nb\nX')).toBe(true);
  });
});
