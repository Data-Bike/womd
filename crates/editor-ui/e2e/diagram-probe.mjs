// Diagram-rendering probe: mermaid, bpmn-js, @plantuml/core, inline svg.
// Opens a mocked document with one fenced block per engine and waits for
// each async render to land (or produce a .diagram-error block).
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const SHOTS = new URL('./shots/', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1');
fs.mkdirSync(SHOTS, { recursive: true });

const errors = [];
const results = [];
function check(name, ok, extra = '') {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);
}

const DOC = `# Diagrams

A citation [@doe99] and a bare @knuth84 chip, plus a missing [@nobody]. Mail me@example.com, ping @everyone.

\`\`\`mermaid
graph TD
  A[Start] --> B{Ok?}
  B -->|yes| C[Done]
  B -->|no| A
\`\`\`

\`\`\`bpmn
<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" xmlns:bpmndi="http://www.omg.org/spec/BPMN/20100524/DI" xmlns:dc="http://www.omg.org/spec/DD/20100524/DC" xmlns:di="http://www.omg.org/spec/DD/20100524/DI" id="Defs_1" targetNamespace="http://example.com/bpmn">
  <bpmn:process id="p1">
    <bpmn:startEvent id="Start_1"><bpmn:outgoing>Flow_1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:task id="Task_1" name="Work"><bpmn:incoming>Flow_1</bpmn:incoming><bpmn:outgoing>Flow_2</bpmn:outgoing></bpmn:task>
    <bpmn:endEvent id="End_1"><bpmn:incoming>Flow_2</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="Flow_1" sourceRef="Start_1" targetRef="Task_1"/>
    <bpmn:sequenceFlow id="Flow_2" sourceRef="Task_1" targetRef="End_1"/>
  </bpmn:process>
  <bpmndi:BPMNDiagram id="D_1">
    <bpmndi:BPMNPlane id="P_1" bpmnElement="p1">
      <bpmndi:BPMNShape id="S_1" bpmnElement="Start_1"><dc:Bounds x="100" y="100" width="36" height="36"/></bpmndi:BPMNShape>
      <bpmndi:BPMNShape id="S_2" bpmnElement="Task_1"><dc:Bounds x="200" y="80" width="100" height="80"/></bpmndi:BPMNShape>
      <bpmndi:BPMNShape id="S_3" bpmnElement="End_1"><dc:Bounds x="370" y="100" width="36" height="36"/></bpmndi:BPMNShape>
      <bpmndi:BPMNEdge id="E_1" bpmnElement="Flow_1"><di:waypoint x="136" y="118"/><di:waypoint x="200" y="118"/></bpmndi:BPMNEdge>
      <bpmndi:BPMNEdge id="E_2" bpmnElement="Flow_2"><di:waypoint x="300" y="118"/><di:waypoint x="370" y="118"/></bpmndi:BPMNEdge>
    </bpmndi:BPMNPlane>
  </bpmndi:BPMNDiagram>
</bpmn:definitions>
\`\`\`

\`\`\`plantuml
@startuml
Alice -> Bob : Hello
Bob --> Alice : Hi
@enduml
\`\`\`

\`\`\`svg
<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40"><rect width="120" height="40" fill="#7aa2f7"/><text x="10" y="25" fill="#ffffff">SVG ok</text></svg>
\`\`\`

\`\`\`mermaid
graph TD
  A[unclosed --> 
\`\`\`

\`\`\`plantuml
@startuml
Alice -> Bob : [[javascript:alert(1) click]]
@enduml
\`\`\`

\`\`\`bibtex
@book{doe99, author={Doe, John}, title={Sample Book}, year={1999}, publisher={Test Pub}}
@book{knuth84, author={Knuth, Donald E.}, title={The TeXbook}, year={1984}, publisher={Addison-Wesley}}
\`\`\`
`;

const browser = await chromium.launch();
// Short viewport first: the bibliography (last block) is off-screen and
// never rendered by the virtualized view — chips must still resolve.
const page = await browser.newPage({ viewport: { width: 1200, height: 500 } });
page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
page.on('console', (msg) => {
  if (msg.type() === 'error') errors.push(`console.error: ${msg.text()}`);
});

await page.addInitScript({ path: new URL('./mock-tauri.mjs', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1') });
await page.addInitScript((doc) => { window.__tauriMock.vfs.set('/repo/diagrams.md', doc); }, DOC);

await page.goto('http://localhost:5173/');
await page.waitForSelector('.md-block', { timeout: 15000 });

await page.evaluate(() => { window.__mockDialogOpen = '/repo/diagrams.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(500);

// Each engine is a separate lazy chunk + render; plantuml is ~5 MB of
// TeaVM-compiled Java, so allow a generous deadline.
const deadline = Date.now() + 90000;
const readStats = () => page.evaluate(() => ({
  mermaid: document.querySelectorAll('.diagram-mermaid svg').length,
  bpmn: document.querySelectorAll('.diagram-bpmn svg').length,
  plantuml: document.querySelectorAll('.diagram-plantuml svg').length,
  svgImg: document.querySelectorAll('img.diagram-svg').length,
  bib: document.querySelectorAll('.bibliography .csl-entry').length,
  chipsResolved: [...document.querySelectorAll('.cite-chip:not(.cite-unresolved)')].map((e) => e.textContent),
  chipsUnresolved: document.querySelectorAll('.cite-chip.cite-unresolved').length,
  errors: [...document.querySelectorAll('.diagram-error-msg')].map((e) => e.textContent),
  pending: document.querySelectorAll('.diagram-pending').length,
  jsLinks: [...document.querySelectorAll('.md-block a, .md-block [href]')]
    .map((a) => a.getAttribute('href') || a.getAttribute('xlink:href') || '')
    .filter((h) => /^\s*javascript:/i.test(h)).length,
  strayMermaid: document.querySelectorAll('body > [id^="dwomd-mmd"], body > [id^="womd-mmd"]').length,
}));

let stats = await readStats();
for (let i = 0; i < 20 && stats.chipsResolved.length < 2; i++) {
  await page.waitForTimeout(500);
  stats = await readStats();
}
check('chips resolve while bibliography is off-screen',
  stats.bib === 0 && stats.chipsResolved.length === 2,
  `bibRendered=${stats.bib} resolved=${JSON.stringify(stats.chipsResolved)}`);

await page.setViewportSize({ width: 1200, height: 6000 });
await page.waitForTimeout(500);
stats = await readStats();
while (Date.now() < deadline) {
  // 2 plantuml (valid + js-link), broken mermaid → exactly 1 error block.
  if (stats.mermaid && stats.bpmn && stats.plantuml >= 2 && stats.svgImg && stats.bib && !stats.pending) break;
  await page.waitForTimeout(1000);
  stats = await readStats();
}
// Citation chips repaint one tick after the bibliography registers keys.
await page.waitForTimeout(1500);
stats = await readStats();

check('mermaid renders to inline svg', stats.mermaid >= 1, `count=${stats.mermaid}`);
check('bpmn renders to inline svg', stats.bpmn >= 1, `count=${stats.bpmn}`);
check('plantuml renders to inline svg', stats.plantuml >= 1, `count=${stats.plantuml}`);
check('svg fence renders as img', stats.svgImg >= 1, `count=${stats.svgImg}`);
check('bibtex renders bibliography entries', stats.bib >= 2, `entries=${stats.bib}`);
check('[@key] chips resolve to (Author, Year)',
  (stats.chipsResolved || []).some((t) => /Doe.*1999/.test(t)) &&
  (stats.chipsResolved || []).some((t) => /Knuth.*1984/.test(t)),
  JSON.stringify(stats.chipsResolved));
check('missing key stays unresolved; email/@everyone untouched', stats.chipsUnresolved === 1 && stats.chipsResolved.length === 2,
  `unresolved=${stats.chipsUnresolved} resolved=${stats.chipsResolved.length}`);
check('broken mermaid shows exactly one error block', stats.errors?.length === 1, (stats.errors || []).join(' | ').slice(0, 160));
check('no javascript: links survive in diagram SVG', stats.jsLinks === 0, `jsLinks=${stats.jsLinks}`);
check('failed mermaid render leaves no stray body nodes', stats.strayMermaid === 0, `stray=${stats.strayMermaid}`);
check('no pending placeholders left', stats.pending === 0, `pending=${stats.pending}`);
check('no console errors', errors.length === 0, errors.slice(0, 3).join(' | '));

await page.screenshot({ path: path.join(SHOTS, 'diagrams.png'), fullPage: true });

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
if (errors.length) console.log('Errors:', errors.slice(0, 5));
await browser.close();
process.exit(failed.length ? 1 : 0);
