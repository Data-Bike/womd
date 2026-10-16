// Bibliography rendering + Pandoc-style [@citekey] chips.
//
// ` ```bibtex `/` ```bib `, ` ```ris ` and ` ```csl-json ` fences render as a
// formatted bibliography card. The fence info string selects style and
// locale: ` ```bibtex ieee ru ` → IEEE style, Russian locale (defaults
// `apa` / `en-US`).
//
// Body text cites entries with `[@key]`, `[@a; @b]` or a bare `@key`.
// Chips resolve against the bibliographies of the CURRENT document only:
// the editor scans the document's block list for bibliography fences and
// hands them to setDocumentBibliographies() — so a citation resolves even
// when its bibliography block is far off-screen (the rendered view is
// virtualized and never renders it), and keys never leak between tabs.
// The chip label is produced by the bibliography's own CSL style:
// "(Knuth, 1984)" for APA, "[1]" for IEEE.
//
// A document without any bibliography renders `@` text untouched — an
// "@everyone" or "@Override" in ordinary prose must not turn into an
// unresolved-citation warning.
//
// Parsing is citation-js (BibTeX/RIS → CSL-JSON); formatting is citeproc-js
// with bundled CSL styles/locales (imported as ?raw strings, so everything
// stays inside the app — no network, no server).

import { registerBlockRenderer } from './diagram.js';

// ── Fence dispatch ──────────────────────────────────────────────────────

const FENCE_FORMAT = {
  bibtex: 'bibtex',
  bib: 'bibtex',
  biblatex: 'bibtex',
  ris: 'ris',
  'csl-json': 'csl-json',
  csl: 'csl-json',
  citeproc: 'csl-json',
};

const STYLE_ALIAS = {
  apa: 'apa',
  ieee: 'ieee',
  vancouver: 'vancouver',
  vanc: 'vancouver',
  chicago: 'chicago-author-date',
  'chicago-author-date': 'chicago-author-date',
};
const LOCALE_ALIAS = {
  en: 'en-US', 'en-us': 'en-US',
  ru: 'ru-RU', 'ru-ru': 'ru-RU',
};

/**
 * Fenced-block info string → async-block kind, or null.
 * "bibtex ieee ru" -> "bibliography:bibtex:ieee:ru-RU".
 * The kind string carries its arguments, so the shared cache key in
 * diagram.js automatically separates style/locale variants.
 */
export function citationKind(infoString) {
  if (!infoString) return null;
  const parts = infoString.trim().split(/\s+/);
  const fmt = FENCE_FORMAT[parts[0].toLowerCase()];
  if (!fmt) return null;
  const style = STYLE_ALIAS[(parts[1] || '').toLowerCase()] || 'apa';
  const locale = LOCALE_ALIAS[(parts[2] || '').toLowerCase()] || 'en-US';
  return `bibliography:${fmt}:${style}:${locale}`;
}

// ── Lazy engine loading ─────────────────────────────────────────────────

const STYLE_LOADERS = {
  apa: () => import('../csl/apa.csl?raw'),
  ieee: () => import('../csl/ieee.csl?raw'),
  vancouver: () => import('../csl/vancouver.csl?raw'),
  'chicago-author-date': () => import('../csl/chicago-author-date.csl?raw'),
};
const LOCALE_LOADERS = {
  'en-US': () => import('../csl/locales-en-US.xml?raw'),
  'ru-RU': () => import('../csl/locales-ru-RU.xml?raw'),
};

const xmlCache = new Map();
function loadXml(loader, key) {
  if (!xmlCache.has(key)) xmlCache.set(key, loader().then((m) => m.default));
  return xmlCache.get(key);
}

let citeLib = null;
async function loadCite() {
  if (!citeLib) {
    const core = await import('@citation-js/core');
    citeLib = core.Cite || core.default;
  }
  return citeLib;
}

let cslLib = null;
async function loadCsl() {
  if (!cslLib) cslLib = (await import('citeproc')).default;
  return cslLib;
}

// ── Parsing ─────────────────────────────────────────────────────────────

// Force the input type: citation-js otherwise autodetects, and a fence that
// happens to hold a bare URL/DOI would be routed to a network-fetching
// parser if such a plugin is ever added.
const FORCE_TYPE = { bibtex: '@biblatex/text', ris: '@ris/file' };

async function parseItems(fmt, source) {
  if (fmt === 'csl-json') {
    const data = JSON.parse(source);
    const arr = Array.isArray(data) ? data : (data.references || data.items || []);
    if (!Array.isArray(arr) || !arr.length) throw new Error('no CSL items in JSON');
    return arr.map(withId);
  }
  const Cite = await loadCite();
  if (fmt === 'bibtex') await import('@citation-js/plugin-bibtex');
  else if (fmt === 'ris') await import('@citation-js/plugin-ris');
  const cite = await Cite.async(source, { forceType: FORCE_TYPE[fmt] });
  const items = cite.data || [];
  if (!items.length) throw new Error('no entries parsed');
  return items.map(withId);
}

function withId(item, i) {
  const id = String(item.id ?? item['citation-key'] ?? `item-${i}`);
  return { ...item, id };
}

// ── Formatting (citeproc-js) ────────────────────────────────────────────

function stripTags(html) {
  return String(html)
    .replace(/<[^>]+>/g, '')
    .replace(/&#(\d+);/g, (_, n) => String.fromCharCode(Number(n)))
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&')
    .replace(/\s+/g, ' ')
    .trim();
}

async function buildBibliography(fmt, style, locale, source) {
  const items = await parseItems(fmt, source);
  const CSL = await loadCsl();
  const [styleXml, localeXml] = await Promise.all([
    loadXml(STYLE_LOADERS[style], `style:${style}`),
    loadXml(LOCALE_LOADERS[locale], `locale:${locale}`),
  ]);
  const byId = Object.fromEntries(items.map((it) => [it.id, it]));
  const sys = {
    retrieveLocale: () => localeXml,
    retrieveItem: (id) => byId[id],
  };
  // (sys, style, lang, forceLang): forceLang makes ` ```bibtex apa ru `
  // actually produce a Russian bibliography even for styles declaring
  // default-locale="en-US".
  const engine = new CSL.Engine(sys, styleXml, locale, true);
  engine.updateItems(items.map((it) => it.id));
  const [params, entries] = engine.makeBibliography();

  // key -> { label, full }: label is the style's own in-text citation for
  // the entry (numeric styles number by bibliography order, matching the
  // card); full is the plain-text entry for the chip tooltip.
  const refs = new Map();
  for (let i = 0; i < entries.length; i++) {
    const full = stripTags(entries[i]);
    for (const id of params.entry_ids[i] || []) {
      const item = byId[id];
      if (!item) continue;
      let label;
      try { label = stripTags(engine.makeCitationCluster([{ id }])); } catch { label = ''; }
      refs.set(item['citation-key'] || id, { id, label: label || `@${id}`, full });
    }
  }

  const bibClass = params.hangingindent ? 'csl-bib-body csl-hanging' : 'csl-bib-body';
  const count = entries.length;
  const html = `<div class="bibliography">` +
    `<div class="bibliography-head">bibliography · ${style} · ${locale} · ${count} ${count === 1 ? 'entry' : 'entries'}</div>` +
    `<div class="${bibClass}">${entries.join('\n')}</div>` +
    `</div>`;
  return { html, refs };
}

// ── Bibliography store ──────────────────────────────────────────────────

const BIB_MAX = 50;
const bibStore = new Map(); // bibKey -> Promise<{ html, refs }>
const resolved = new Map(); // bibKey -> { refs } | { error }
let activeKeys = [];        // bibKeys of the current document, in order

function bibKey(fmt, style, locale, source) {
  return `${fmt}:${style}:${locale}\n${source}`;
}

function ensureBibliography(fmt, style, locale, source) {
  const key = bibKey(fmt, style, locale, source);
  if (!bibStore.has(key)) {
    if (bibStore.size >= BIB_MAX) {
      // Evict the oldest bibliography no open document still uses.
      for (const k of bibStore.keys()) {
        if (!activeKeys.includes(k)) { bibStore.delete(k); resolved.delete(k); break; }
      }
    }
    const p = buildBibliography(fmt, style, locale, source);
    p.then(
      (r) => resolved.set(key, { refs: r.refs }),
      (e) => resolved.set(key, { error: String(e?.message || e) }),
    );
    bibStore.set(key, p);
  }
  return { key, promise: bibStore.get(key) };
}

function parseKind(kind) {
  const [, fmt, style, locale] = kind.split(':');
  return { fmt, style, locale };
}

/**
 * Declare the current document's bibliography blocks
 * (`[{ info, source }]`, info = the fence info string). Resolves once all
 * of them are parsed/formatted (failures included), so the caller can
 * repaint citation chips.
 */
export async function setDocumentBibliographies(blocks) {
  const pending = [];
  const keys = [];
  for (const b of blocks || []) {
    const kind = citationKind(b.info);
    if (!kind) continue;
    const { fmt, style, locale } = parseKind(kind);
    const { key, promise } = ensureBibliography(fmt, style, locale, b.source ?? '');
    keys.push(key);
    pending.push(promise);
  }
  activeKeys = keys;
  await Promise.allSettled(pending);
}

/** Resolved entry for a cite key in the current document, else null. */
export function lookupCitation(key) {
  for (const k of activeKeys) {
    const r = resolved.get(k);
    const hit = r?.refs?.get(key);
    if (hit) return hit;
  }
  return null;
}

// Every active bibliography finished (ok or error) — only then may a miss
// be reported as "unresolved"; before that it is just "not loaded yet".
function allSettled() {
  return activeKeys.every((k) => resolved.has(k));
}

async function renderBibliography(source, fmt, style, locale) {
  const { promise } = ensureBibliography(fmt, style, locale, source);
  return (await promise).html;
}

registerBlockRenderer('bibliography', renderBibliography);

// ── [@key] / [@a; @b] / @key chips in body text ─────────────────────────

// Pandoc key grammar: starts with a letter/digit/_, internal punctuation
// allowed, but must END with a word char — "see @doe99." cites `doe99`,
// not `doe99.`.
const KEY = '[A-Za-z0-9_](?:[\\w:.#$%&+?<>~/-]*\\w)?';
// `[@a; @b]` bracketed group (unambiguous), or a bare `@key` not preceded
// by a word char / `@` — "user@mail.com" must not chip.
const CITE_RE = new RegExp(
  `\\[(@${KEY}(?:\\s*;\\s*@${KEY})*)\\]|(?<![\\w@])@(${KEY})`,
  'g',
);
const KEY_IN_GROUP_RE = new RegExp(`@(${KEY})`, 'g');

function escapeHtml(s) {
  return String(s)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}
function escapeAttr(s) { return escapeHtml(s).replaceAll("'", '&#39;'); }

function chip(hit) {
  return `<span class="cite-chip" title="${escapeAttr(hit.full)}">${escapeHtml(hit.label)}</span>`;
}
function unresolvedChip(key) {
  return `<span class="cite-chip cite-unresolved" title="${escapeAttr(`No bibliography entry for '${key}'`)}">${escapeHtml('@' + key)}</span>`;
}

/** Escape text, turning citations into chips (see module header). */
export function renderCitationText(text) {
  const s = String(text);
  if (!activeKeys.length || !s.includes('@')) return escapeHtml(s);
  const settled = allSettled();
  let out = '';
  let last = 0;
  for (const m of s.matchAll(CITE_RE)) {
    let rendered = null;
    if (m[1]) {
      const keys = [...m[1].matchAll(KEY_IN_GROUP_RE)].map((k) => k[1]);
      const hits = keys.map(lookupCitation);
      if (hits.every(Boolean)) {
        rendered = hits.map(chip).join('; ');
      } else if (settled) {
        rendered = keys.map((k, i) => (hits[i] ? chip(hits[i]) : unresolvedChip(k))).join('; ');
      }
    } else {
      // A bare @word is only a citation when it actually resolves.
      const hit = lookupCitation(m[2]);
      if (hit) rendered = chip(hit);
    }
    if (rendered == null) continue;
    out += escapeHtml(s.slice(last, m.index)) + rendered;
    last = m.index + m[0].length;
  }
  return out + escapeHtml(s.slice(last));
}
