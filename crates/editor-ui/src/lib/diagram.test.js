import { describe, it, expect } from 'vitest';
import { diagramKind, renderAsyncBlockHtml, renderDiagramSource } from './diagram.js';

describe('diagramKind', () => {
  it('maps known fence languages to renderer kinds', () => {
    expect(diagramKind('mermaid')).toBe('mermaid');
    expect(diagramKind('mmd')).toBe('mermaid');
    expect(diagramKind('bpmn')).toBe('bpmn');
    expect(diagramKind('plantuml')).toBe('plantuml');
    expect(diagramKind('puml')).toBe('plantuml');
    expect(diagramKind('uml')).toBe('plantuml');
    expect(diagramKind('svg')).toBe('svg');
  });

  it('is case-insensitive and takes only the first info-string word', () => {
    expect(diagramKind('MERMAID')).toBe('mermaid');
    expect(diagramKind('  PlantUML  ')).toBe('plantuml');
    expect(diagramKind('mermaid {config}')).toBe('mermaid');
  });

  it('returns null for non-diagram fences', () => {
    for (const lang of ['rust', 'js', 'python', 'text', '', null, undefined]) {
      expect(diagramKind(lang), String(lang)).toBe(null);
    }
  });
});

describe('renderAsyncBlockHtml', () => {
  it('returns a pending placeholder with escaped source on first call', () => {
    const html = renderAsyncBlockHtml('svg', '<svg><script>alert(1)</script></svg>');
    expect(html).toContain('diagram-pending');
    expect(html).toContain('&lt;script&gt;');
    expect(html).not.toContain('<script>');
  });
});

describe('renderDiagramSource svg', () => {
  it('renders SVG source as an inert data-URI image', async () => {
    const src = '<svg xmlns="http://www.w3.org/2000/svg"><text>Hi</text></svg>';
    const html = await renderDiagramSource('svg', src);
    expect(html).toMatch(/^<img class="diagram diagram-svg"/);
    const b64 = html.match(/base64,([^"]+)/)[1];
    expect(decodeURIComponent(escape(atob(b64)))).toBe(src);
  });

  it('does NOT inline raw SVG markup (script would execute)', async () => {
    const html = await renderDiagramSource('svg', '<svg onload="alert(1)"/>');
    expect(html).not.toContain('onload');
    expect(html).not.toContain('<svg');
  });
});
