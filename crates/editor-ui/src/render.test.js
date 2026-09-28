import { describe, it, expect } from 'vitest';
import { safeHref, safeSrc, renderAstNode } from './render.js';

describe('safeHref', () => {
  it('blocks javascript: scheme', () => {
    expect(safeHref('javascript:alert(1)')).toBe('#');
    expect(safeHref('JAVASCRIPT:alert(1)')).toBe('#');
  });

  it('blocks scheme smuggled through control chars', () => {
    expect(safeHref('java\tscript:alert(1)')).toBe('#');
    expect(safeHref('java\nscript:alert(1)')).toBe('#');
    expect(safeHref(' javascript:alert(1)')).toBe('#');
  });

  it('blocks vbscript: scheme', () => {
    expect(safeHref('vbscript:msgbox(1)')).toBe('#');
  });

  it('blocks data: URLs in links', () => {
    expect(safeHref('data:text/html,<script>alert(1)</script>')).toBe('#');
  });

  it('passes http/https/relative/mailto URLs', () => {
    expect(safeHref('https://example.com')).toBe('https://example.com');
    expect(safeHref('http://example.com')).toBe('http://example.com');
    expect(safeHref('./page.md')).toBe('./page.md');
    expect(safeHref('mailto:a@b.c')).toBe('mailto:a@b.c');
    expect(safeHref('#anchor')).toBe('#anchor');
  });
});

describe('escapeAttr in rendered attributes', () => {
  it('neutralizes entity-smuggled schemes (&colon; etc.)', () => {
    // Without `&` escaping, href="javascript&colon;alert(1)" would be
    // entity-decoded by the browser back into javascript:alert(1) — after
    // our sanitize check had already passed.
    const html = renderAstNode({
      type: 'Link',
      destination: 'javascript&colon;alert(1)',
      title: null,
      children: [{ type: 'Text', text: 'x' }],
    });
    expect(html).toContain('javascript&amp;colon;alert(1)');
    expect(html).not.toContain('javascript&colon;');
  });

  it('escapes & in link destinations with query strings', () => {
    const html = renderAstNode({
      type: 'Link',
      destination: 'https://x.test/?a=1&b=2',
      title: null,
      children: [{ type: 'Text', text: 'x' }],
    });
    expect(html).toContain('a=1&amp;b=2');
  });

  it('escapes < > & quotes in titles and alt text', () => {
    const html = renderAstNode({
      type: 'Link',
      destination: 'https://x.test',
      title: '"onmouseover="alert(1)&lt;',
      children: [{ type: 'Text', text: 'x' }],
    });
    // No raw `"` or `&` from the title may survive into the attribute —
    // otherwise it could break out and inject on* handlers.
    expect(html).toContain('&quot;onmouseover=&quot;alert(1)&amp;lt;');
  });
});

describe('safeSrc', () => {
  it('blocks javascript: in image src', () => {
    expect(safeSrc('javascript:alert(1)')).toBe('#');
  });

  it('blocks non-image data: URLs', () => {
    expect(safeSrc('data:text/html;base64,PHNjcmlwdD4=')).toBe('#');
  });

  it('allows embedded data:image URLs', () => {
    const u = 'data:image/png;base64,iVBOR';
    expect(safeSrc(u)).toBe(u);
    const svg = 'data:image/svg+xml;base64,PHN2Zw==';
    expect(safeSrc(svg)).toBe(svg);
  });
});

describe('renderAstNode link safety', () => {
  it('renders javascript: link href as #', () => {
    const html = renderAstNode({
      type: 'Link',
      destination: 'javascript:alert(1)',
      title: null,
      children: [{ type: 'Text', text: 'click' }],
    });
    expect(html).not.toContain('javascript:');
    expect(html).toContain('href="#"');
  });

  it('renders image with javascript: src as #', () => {
    const html = renderAstNode({
      type: 'Image',
      alt: 'x',
      destination: 'javascript:alert(1)',
      title: null,
    });
    expect(html).not.toContain('javascript:');
  });
});

describe('numeric/enum field hardening', () => {
  it('clamps heading level to 1..6', () => {
    const html = renderAstNode({ type: 'Heading', level: 99, children: [{ type: 'Text', text: 'x' }] });
    expect(html).toBe('<h6>x</h6>');
  });

  it('a tampered heading level cannot inject markup', () => {
    const html = renderAstNode({
      type: 'Heading',
      level: '1 onload=alert(1) data-x="',
      children: [{ type: 'Text', text: 'x' }],
    });
    expect(html).toBe('<h1>x</h1>');
  });

  it('a tampered list start cannot inject attributes', () => {
    const html = renderAstNode({
      type: 'List',
      ordered: true,
      start: '5" onclick="alert(1)',
      items: [{ task: null, children: [{ type: 'Text', text: 'a' }] }],
    });
    // Tampered non-numeric start falls back to 1 — no attribute emitted.
    expect(html).toBe('<ol><li>a</li></ol>');
  });

  it('a numeric list start renders as start attribute', () => {
    const html = renderAstNode({
      type: 'List',
      ordered: true,
      start: 5,
      items: [{ task: null, children: [{ type: 'Text', text: 'a' }] }],
    });
    expect(html).toContain('start="5"');
  });
});

describe('renderAstTable alignment hardening', () => {
  it('accepts only left/center/right', () => {
    const html = renderAstNode({
      type: 'Table',
      header: [{ children: [{ type: 'Text', text: 'h' }] }],
      rows: [[{ children: [{ type: 'Text', text: 'c' }] }]],
      alignments: ['left'],
    });
    expect(html).toContain('text-align:left');
  });

  it('a tampered alignment cannot inject CSS/markup', () => {
    const html = renderAstNode({
      type: 'Table',
      header: [{ children: [{ type: 'Text', text: 'h' }] }],
      rows: [[{ children: [{ type: 'Text', text: 'c' }] }]],
      alignments: ['x" onmouseover="alert(1)'],
    });
    expect(html).not.toContain('onmouseover');
    expect(html).not.toContain('text-align:x');
  });
});
