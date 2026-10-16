// Async renderers for fenced code blocks (diagrams, bibliographies).
// renderBlockHtml() is a pure sync function, so a fenced block first emits
// a placeholder; the heavy engines (mermaid ~2 MB, bpmn-js,
// @plantuml/core ~5 MB, citeproc + CSL styles) are dynamically imported only
// when a document actually contains that block type, and the rendered HTML
// is cached by source text. When a render finishes, registered listeners are
// notified so the view can re-evaluate the block's v-html.
//
// Rendering is serialized through one queue: the PlantUML engine keeps
// shared internal state and silently overwrites a concurrent render, and
// mermaid/bpmn-js both stage detached DOM nodes that don't race well either.

const cache = new Map(); // `${lang}\n${source}` -> { status, html?, error? }
const CACHE_MAX = 200;   // bound memory: edited sources churn cache keys
const listeners = new Set();

/** Subscribe to "an async block finished rendering". Returns an unsubscribe fn. */
export function onAsyncBlockRendered(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function notify() {
  for (const fn of listeners) fn();
}

function escapeHtml(s) {
  return String(s)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

// Fence info-string -> renderer kind. `uml`/`puml` alias PlantUML since that
// is what ` ```uml ` blocks overwhelmingly contain; `mmd` is mermaid's file
// extension and shows up as an info string in the wild.
const LANG_KIND = {
  mermaid: 'mermaid',
  mmd: 'mermaid',
  bpmn: 'bpmn',
  bpmn2: 'bpmn',
  bpmn20: 'bpmn',
  plantuml: 'plantuml',
  puml: 'plantuml',
  uml: 'plantuml',
  svg: 'svg',
};

/** First word of a fenced block's info string -> diagram kind, or null. */
export function diagramKind(infoString) {
  if (!infoString) return null;
  const lang = infoString.trim().split(/\s+/)[0].toLowerCase();
  return LANG_KIND[lang] || null;
}

function cacheSet(key, entry) {
  // Overwriting a key (pending -> ok/error) must not evict an unrelated
  // entry, and re-inserting moves the key to the young end (LRU on write).
  cache.delete(key);
  if (cache.size >= CACHE_MAX) {
    // Map iterates in insertion order — evict the oldest pending/error/ok
    // entry alike; an evicted pending render just re-queues on next paint.
    cache.delete(cache.keys().next().value);
  }
  cache.set(key, entry);
}

// bpmn-js and PlantUML SVG goes into v-html. Both engines can carry
// document-controlled markup into the SVG (PlantUML `[[url]]` links, BPMN
// labels/documentation), so strip scripts, on* handlers and javascript:
// hrefs before it reaches the DOM. Mermaid sanitizes itself (strict mode).
let purifier = null;
async function sanitizeSvg(svg) {
  if (!purifier) purifier = (await import('dompurify')).default;
  return purifier.sanitize(svg, { USE_PROFILES: { svg: true, svgFilters: true } });
}

let uid = 0;

async function renderMermaid(source) {
  const { default: mermaid } = await import('mermaid');
  // 'strict' runs labels through DOMPurify and drops interactive features —
  // a fence must not become a clickjacking vector. Diagrams sit on a light
  // panel (see style.css) so they read the same under every app theme —
  // the 'default' mermaid theme matches that panel.
  mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: 'default' });
  const id = `womd-mmd-${uid++}`;
  try {
    const { svg } = await mermaid.render(id, source);
    return `<div class="diagram diagram-mermaid">${svg}</div>`;
  } catch (e) {
    // On a parse error mermaid leaves its temp render container (`d<id>`)
    // attached to <body> — one leaked node per failed render, i.e. per
    // keystroke while a diagram is being typed.
    document.getElementById(`d${id}`)?.remove();
    document.getElementById(id)?.remove();
    throw e;
  }
}

async function renderBpmn(source) {
  const { default: Viewer } = await import('bpmn-js/lib/Viewer');
  const host = document.createElement('div');
  host.style.cssText = 'position:absolute;left:-10000px;top:0;visibility:hidden;';
  document.body.appendChild(host);
  const viewer = new Viewer();
  try {
    viewer.attachTo(host);
    await viewer.importXML(source);
    const { svg } = await viewer.saveSVG();
    return `<div class="diagram diagram-bpmn">${await sanitizeSvg(svg)}</div>`;
  } finally {
    viewer.destroy();
    host.remove();
  }
}

let plantumlEngine = null;
function loadPlantuml() {
  // viz-global.js (Graphviz) and themes.js are classic scripts registering
  // globals — side-effect imports must run before the engine module loads.
  if (!plantumlEngine) {
    plantumlEngine = (async () => {
      const vizMod = await import('@plantuml/core/viz-global.js');
      // viz-global is UMD: in a plain browser it assigns globalThis.Viz, but
      // a bundler may route it through the `exports` branch — then the Viz
      // API lands on the module namespace and the engine's bare `Viz`
      // lookup finds nothing. Bridge the two outcomes.
      if (!globalThis.Viz && vizMod && typeof vizMod === 'object') {
        const viz = vizMod.default || vizMod;
        if (viz && typeof viz === 'object' && Object.keys(viz).length > 0) {
          globalThis.Viz = viz;
        }
      }
      await import('@plantuml/core/themes.js');
      return import('@plantuml/core/plantuml.js');
    })();
  }
  return plantumlEngine;
}

async function renderPlantuml(source) {
  const { renderToString } = await loadPlantuml();
  const svg = await new Promise((resolve, reject) => {
    renderToString(
      source.split('\n'),
      resolve,
      (msg) => reject(new Error(msg || 'PlantUML render failed')),
    );
  });
  return `<div class="diagram diagram-plantuml">${await sanitizeSvg(svg)}</div>`;
}

// An ` ```svg ` fence is inline SVG markup. Emitting it as innerHTML would
// run embedded <script>/on* handlers, so it goes through <img src=data:>,
// where scripting is inert. base64 avoids URL-escaping the whole document.
async function renderSvgImage(source) {
  const uri = 'data:image/svg+xml;base64,' + btoa(unescape(encodeURIComponent(source)));
  return `<img class="diagram diagram-svg" alt="svg diagram" src="${uri}"/>`;
}

const RENDERERS = {
  mermaid: renderMermaid,
  bpmn: renderBpmn,
  plantuml: renderPlantuml,
  svg: renderSvgImage,
};

/**
 * Register an async block renderer. `kind` may carry colon-separated
 * arguments (`bibliography:apa:ru`) — the part before the first ':' selects
 * the renderer, the rest are forwarded to it and stay part of the cache key.
 */
export function registerBlockRenderer(name, fn) {
  RENDERERS[name] = fn;
}

/** Render one block source to HTML via its engine (exposed for tests). */
export async function renderDiagramSource(kind, source) {
  const [name, ...args] = kind.split(':');
  const renderer = RENDERERS[name];
  if (!renderer) throw new Error(`no renderer registered for '${name}'`);
  return renderer(source, ...args);
}

// The queue is serial, so one engine that never settles (a PlantUML
// callback that never fires, a wedged layout) would leave every later
// diagram in the app pending forever. Bound each render.
const RENDER_TIMEOUT_MS = 60000;
function withTimeout(promise, kind) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${kind.split(':')[0]} render timed out`)), RENDER_TIMEOUT_MS);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

let queue = Promise.resolve();
function enqueue(kind, source, key) {
  queue = queue.then(async () => {
    try {
      const html = await withTimeout(renderDiagramSource(kind, source), kind);
      cacheSet(key, { status: 'ok', html });
    } catch (e) {
      cacheSet(key, { status: 'error', error: String(e?.message || e) });
    }
    notify();
  });
}

/**
 * Sync HTML for an async block fence: cached result when available, a
 * source placeholder (or error + source) otherwise. Never throws.
 */
export function renderAsyncBlockHtml(kind, source) {
  const key = `${kind}\n${source}`;
  let hit = cache.get(key);
  if (!hit) {
    hit = { status: 'pending' };
    cacheSet(key, hit);
    enqueue(kind, source, key);
  }
  if (hit.status === 'ok') return hit.html;
  if (hit.status === 'error') {
    return `<div class="diagram-error"><div class="diagram-error-msg">` +
      `Render failed: ${escapeHtml(hit.error)}</div>` +
      `<pre class="diagram-error-source">${escapeHtml(source)}</pre></div>`;
  }
  return `<pre class="diagram-pending">${escapeHtml(source)}</pre>`;
}
