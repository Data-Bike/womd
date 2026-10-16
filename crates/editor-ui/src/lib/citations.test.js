import { describe, it, expect, beforeEach } from 'vitest';
import { citationKind, renderCitationText, setDocumentBibliographies, lookupCitation } from './citations.js';

const BIB = `@book{knuth84, author={Knuth, Donald E.}, title={The TeXbook}, year={1984}, publisher={Addison-Wesley}}
@article{turing36, author={Turing, Alan M.}, title={On Computable Numbers}, journal={Proc. LMS}, year={1936}}`;

describe('citationKind', () => {
  it('maps bibliography fence languages with default style/locale', () => {
    expect(citationKind('bibtex')).toBe('bibliography:bibtex:apa:en-US');
    expect(citationKind('bib')).toBe('bibliography:bibtex:apa:en-US');
    expect(citationKind('ris')).toBe('bibliography:ris:apa:en-US');
    expect(citationKind('csl-json')).toBe('bibliography:csl-json:apa:en-US');
  });

  it('takes style and locale from the info string', () => {
    expect(citationKind('bibtex ieee')).toBe('bibliography:bibtex:ieee:en-US');
    expect(citationKind('bibtex chicago ru')).toBe('bibliography:bibtex:chicago-author-date:ru-RU');
    expect(citationKind('csl-json vancouver')).toBe('bibliography:csl-json:vancouver:en-US');
  });

  it('falls back to apa/en-US for unknown style/locale words', () => {
    expect(citationKind('bibtex bogus xx')).toBe('bibliography:bibtex:apa:en-US');
  });

  it('returns null for non-bibliography fences', () => {
    for (const lang of ['mermaid', 'rust', 'json', '', null]) {
      expect(citationKind(lang), String(lang)).toBe(null);
    }
  });
});

describe('renderCitationText without a bibliography', () => {
  beforeEach(() => setDocumentBibliographies([]));

  it('escapes plain text', () => {
    expect(renderCitationText('a <b> & "c"')).toBe('a &lt;b&gt; &amp; &quot;c&quot;');
  });

  it('leaves @mentions and [@key] untouched — no false "unresolved" chips', () => {
    expect(renderCitationText('ping @everyone, see [@doe99]')).toBe('ping @everyone, see [@doe99]');
  });
});

describe('renderCitationText with a document bibliography', () => {
  beforeEach(() => setDocumentBibliographies([{ info: 'bibtex', source: BIB }]));

  it('registers keys with style-produced labels', () => {
    expect(lookupCitation('knuth84').label).toBe('(Knuth, 1984)');
    expect(lookupCitation('knuth84').full).toContain('The TeXbook');
    expect(lookupCitation('nobody')).toBe(null);
  });

  it('renders [@key] as a resolved chip, consuming the brackets', () => {
    const html = renderCitationText('see [@knuth84] now');
    expect(html).toMatch(/^see <span class="cite-chip" title="[^"]*TeXbook[^"]*">\(Knuth, 1984\)<\/span> now$/);
  });

  it('renders a [@a; @b] group as one chip per key', () => {
    const html = renderCitationText('[@knuth84; @turing36]');
    expect(html.match(/class="cite-chip"/g).length).toBe(2);
    expect(html).toContain('(Turing, 1936)');
  });

  it('flags a missing key inside brackets as unresolved', () => {
    const html = renderCitationText('[@knuth84; @nobody]');
    expect(html).toContain('(Knuth, 1984)');
    expect(html).toContain('cite-chip cite-unresolved');
    expect(html).toContain('@nobody');
  });

  it('chips a bare @key only when it resolves; strips trailing punctuation', () => {
    const html = renderCitationText('per @knuth84. Ping @everyone or mail a@knuth84.org');
    expect(html.match(/cite-chip/g).length).toBe(1);
    expect(html).toContain('</span>. Ping @everyone or mail a@knuth84.org');
  });

  it('IEEE style produces numeric labels in bibliography order', async () => {
    await setDocumentBibliographies([{ info: 'bibtex ieee', source: BIB }]);
    expect(lookupCitation('knuth84').label).toBe('[1]');
    expect(lookupCitation('turing36').label).toBe('[2]');
  });

  it('keys do not leak once the document drops its bibliography', async () => {
    await setDocumentBibliographies([]);
    expect(lookupCitation('knuth84')).toBe(null);
  });
});
