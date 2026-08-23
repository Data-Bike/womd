import { describe, it, expect } from 'vitest';
import { byteOffsetToIndex, indexToByteOffset, byteOffsetToTextareaIndex } from './byteOffset.js';

describe('byteOffsetToIndex', () => {
  it('is a no-op for pure ASCII text', () => {
    const text = 'hello world';
    expect(byteOffsetToIndex(text, 0)).toBe(0);
    expect(byteOffsetToIndex(text, 6)).toBe(6);
    expect(byteOffsetToIndex(text, text.length)).toBe(text.length);
  });

  it('accounts for 2-byte Cyrillic characters', () => {
    // 'Привет' = 6 Cyrillic chars, each 2 bytes in UTF-8 = 12 bytes.
    // 1-byte ASCII space is at byte 12 (JS index 6).
    // 'м' begins at byte 13 and ends at byte 14 (JS index 7).
    // 'и' begins at byte 15 (JS index 8).
    const text = 'Привет мир';
    expect(byteOffsetToIndex(text, 12)).toBe(6); // start of space
    expect(byteOffsetToIndex(text, 13)).toBe(7); // after space, start of 'м'
    expect(byteOffsetToIndex(text, 14)).toBe(7); // inside 'м'
    expect(byteOffsetToIndex(text, 15)).toBe(8); // after 'м', start of 'и'
  });

  it('matches the reported bug scenario: text after a Cyrillic run', () => {
    const text = 'Кириллица breaks selection';
    const needle = 'breaks';
    const jsIndex = text.indexOf(needle);
    // Simulate what the Rust backend would report: the byte offset of the
    // same match, computed via UTF-8 byte length of everything before it.
    const before = text.slice(0, jsIndex);
    const byteOffset = new TextEncoder().encode(before).length;
    expect(byteOffset).toBeGreaterThan(jsIndex); // proves the two diverge here
    expect(byteOffsetToIndex(text, byteOffset)).toBe(jsIndex);
  });

  it('handles 3-byte CJK characters', () => {
    const text = '日本語 test';
    const byteOffset = new TextEncoder().encode('日本語').length; // 9 bytes
    expect(byteOffsetToIndex(text, byteOffset)).toBe(3); // 3 JS chars
  });

  it('handles 4-byte astral characters (surrogate pairs)', () => {
    const text = '😀😀 rest'; // each emoji: 4 bytes UTF-8, 2 UTF-16 code units
    const byteOffset = new TextEncoder().encode('😀😀').length; // 8 bytes
    expect(byteOffsetToIndex(text, byteOffset)).toBe(4); // 2 surrogate pairs = 4 JS indices
  });

  it('clamps to the string length for an out-of-range offset', () => {
    const text = 'short';
    expect(byteOffsetToIndex(text, 1000)).toBe(text.length);
  });

  it('treats offset 0 (or negative) as index 0', () => {
    expect(byteOffsetToIndex('anything', 0)).toBe(0);
    expect(byteOffsetToIndex('anything', -5)).toBe(0);
  });
});

describe('indexToByteOffset', () => {
  it('is a no-op for pure ASCII text', () => {
    const text = 'hello world';
    expect(indexToByteOffset(text, 6)).toBe(6);
  });

  it('accounts for multi-byte characters', () => {
    // 'Привет' = 6 chars * 2 bytes = 12, space = 1 byte, 'м' starts at byte 13.
    const text = 'Привет мир';
    expect(indexToByteOffset(text, 6)).toBe(12); // after 'Привет'
    expect(indexToByteOffset(text, 7)).toBe(13); // after the space, start of 'м'
  });

  it('round-trips at every code-point boundary for a variety of texts', () => {
    const samples = ['plain ascii', 'Кириллица и текст', '日本語 mixed テスト', '😀 emoji 🎉 test'];
    for (const text of samples) {
      let jsIndex = 0;
      while (jsIndex <= text.length) {
        const byteOffset = indexToByteOffset(text, jsIndex);
        expect(byteOffsetToIndex(text, byteOffset)).toBe(jsIndex);
        if (jsIndex === text.length) break;
        const cp = text.codePointAt(jsIndex);
        jsIndex += cp > 0xffff ? 2 : 1;
      }
    }
  });

  it('clamps to the string length for an out-of-range index', () => {
    const text = 'short';
    expect(indexToByteOffset(text, 1000)).toBe(new TextEncoder().encode(text).length);
  });
});

describe('byteOffsetToTextareaIndex', () => {
  it('is a no-op when the raw text has no carriage returns', () => {
    const raw = 'Кириллица test';
    const byteOffset = new TextEncoder().encode('Кириллица ').length;
    expect(byteOffsetToTextareaIndex(raw, byteOffset)).toBe(byteOffsetToIndex(raw, byteOffset));
  });

  it('compensates for CRLF line endings that the textarea normalizes to LF', () => {
    // Raw backend bytes: 'ab\r\ncd' -> 6 bytes. Textarea API value: 'ab\ncd' (5 chars).
    const raw = 'ab\r\ncd';
    const apiValue = 'ab\ncd';
    // Match 'cd' in the raw document starts at byte 4, ends at byte 6 (exclusive).
    expect(byteOffsetToTextareaIndex(raw, 4)).toBe(apiValue.indexOf('cd')); // 3
    expect(byteOffsetToTextareaIndex(raw, 6)).toBe(apiValue.indexOf('cd') + 2); // 5
  });

  it('compensates for multiple CRLF sequences', () => {
    const raw = 'line1\r\nline2\r\nline3';
    const apiValue = 'line1\nline2\nline3';
    // 'line3' starts at raw byte (5 + 2 + 5 + 2) = 14, ends at 19.
    const line3Start = new TextEncoder().encode('line1\r\nline2\r\n').length;
    expect(line3Start).toBe(14);
    expect(byteOffsetToTextareaIndex(raw, line3Start)).toBe(apiValue.indexOf('line3')); // 12
    expect(byteOffsetToTextareaIndex(raw, line3Start + 5)).toBe(apiValue.indexOf('line3') + 5); // 17
  });

  it('handles bare CR (old Mac) line endings the same way', () => {
    // Raw 'ab\rcd' (5 chars, 5 bytes). Textarea API value: 'ab\ncd' (5 chars).
    const raw = 'ab\rcd';
    const apiValue = 'ab\ncd';
    expect(byteOffsetToTextareaIndex(raw, 3)).toBe(apiValue.indexOf('cd'));       // start of 'c'
    expect(byteOffsetToTextareaIndex(raw, 5)).toBe(apiValue.indexOf('cd') + 2);   // end of 'd'
  });

  it('combines multi-byte UTF-8 and CRLF normalization', () => {
    // 'Ки' = 4 bytes, then CRLF (2 bytes), then 'Ми' = 4 bytes. Total raw = 10 bytes.
    // API value: 'Ки\nМи' (7 chars): К(0) и(1) \n(2) М(3) и(4).
    const raw = 'Ки\r\nМи';
    const apiValue = 'Ки\nМи';
    expect(byteOffsetToTextareaIndex(raw, 6)).toBe(apiValue.indexOf('Ми')); // 3
    expect(byteOffsetToTextareaIndex(raw, 10)).toBe(apiValue.length); // 5
  });
});
