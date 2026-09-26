import { describe, it, expect } from 'vitest';
import {
  normalizeForTextarea,
  dominantLineEnding,
  restoreLineEndings,
  contentChanged,
  detectLineEnding,
  detectLineEndingSampled,
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
  it('detects CRLF when pairs dominate or tie', () => {
    expect(dominantLineEnding('a\r\nb\n')).toBe('\r\n');
    expect(dominantLineEnding('a\r\nb\r\n')).toBe('\r\n');
  });

  it('reports LF when lone LF dominates', () => {
    expect(dominantLineEnding('a\nb\n')).toBe('\n');
    // Mostly-LF file with one stray CRLF stays LF — restoring to CRLF
    // would inflate the edit diff.
    expect(dominantLineEnding('a\nb\nc\r\n')).toBe('\n');
    expect(dominantLineEnding('')).toBe('\n');
  });

  it('reports CR for classic-Mac files', () => {
    expect(dominantLineEnding('a\rb')).toBe('\r');
    expect(dominantLineEnding('a\rb\rc\r')).toBe('\r');
    // CR beats LF only on strict majority, not ties.
    expect(dominantLineEnding('a\rb\rc\nd')).toBe('\r');
    expect(dominantLineEnding('a\rb\nc\nd')).toBe('\n');
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

  it('restores lone CR for a classic-Mac original', () => {
    const original = 'a\rb\rc\r';
    // The textarea can only produce LF — an unhandled CR original would
    // silently convert the whole file's endings on any edit.
    expect(restoreLineEndings(original, normalizeForTextarea(original))).toBe(original);
    expect(restoreLineEndings(original, 'a\nedited\n')).toBe('a\redited\r');
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

describe('detectLineEnding', () => {
  it('classifies each single-family document', () => {
    expect(detectLineEnding('a\nb\n')).toBe('LF');
    expect(detectLineEnding('a\r\nb\r\n')).toBe('CRLF');
    expect(detectLineEnding('a\rb\rc')).toBe('CR');
    expect(detectLineEnding('')).toBe('LF');
    expect(detectLineEnding('no newlines')).toBe('LF');
  });

  it('reports Mixed when more than one family is present', () => {
    expect(detectLineEnding('a\nb\r\n')).toBe('Mixed');
    expect(detectLineEnding('a\r\nb\n')).toBe('Mixed');
    expect(detectLineEnding('a\rb\n')).toBe('Mixed');
    // A lone trailing '\r' at a real EOF is bare CR, not truncated CRLF.
    expect(detectLineEnding('a\rb\r')).toBe('CR');
  });
});

describe('detectLineEndingSampled', () => {
  it('drops a dangling \r cut between \r and \n of a CRLF pair', () => {
    // 'a\nb\r\nc': text[3]='\r' and text[4]='\n' — a limit of 4 cuts the
    // pair in half, leaving 'a\nb\r' whose lone '\r' reads as bare CR.
    const text = 'a\nb\r\nc\r\n';
    expect(detectLineEnding(text.substring(0, 4))).toBe('Mixed'); // the bug
    expect(detectLineEndingSampled(text, 4)).toBe('LF');
    // A pure-CRLF file cut so a dangling '\r' trails a real CRLF pair.
    const crlf = 'a\r\nbc\r\ndef';
    expect(detectLineEnding(crlf.substring(0, 6))).toBe('Mixed'); // the bug
    expect(detectLineEndingSampled(crlf, 6)).toBe('CRLF');
  });

  it('keeps a trailing \r that is not truncated (real bare CR at EOF)', () => {
    const text = 'ab\rcd\r';
    expect(detectLineEndingSampled(text, 6)).toBe('CR');
    // Sample shorter than text but the last char is mid-document content —
    // 'd' here — nothing to drop.
    expect(detectLineEndingSampled(text, 5)).toBe('CR');
  });

  it('does not drop a genuine bare-CR line ending inside a larger CR file', () => {
    // CR-only file sampled mid-way: the last char in the sample is a real
    // '\r' line ending followed by more text — dropping it still yields CR,
    // so classification is unchanged either way.
    const text = 'a\rb\rc\rd\re\r';
    expect(detectLineEndingSampled(text, 5)).toBe('CR');
    expect(detectLineEndingSampled(text, 10)).toBe('CR');
  });
});
