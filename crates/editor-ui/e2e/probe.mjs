// Click-driven E2E probe against the Vite dev server with a mocked Tauri
// backend (e2e/mock-tauri.mjs). Captures console errors + screenshots.
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const SHOTS = new URL('./shots/', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1');
fs.mkdirSync(SHOTS, { recursive: true });

const errors = [];
const warnings = [];
const results = [];
function check(name, ok, extra = '') {
  results.push({ name, ok, extra });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);
}
const shot = async (page, name) => {
  await page.screenshot({ path: path.join(SHOTS, name + '.png'), fullPage: false });
};

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
let step = 'init';
const setStep = async (s) => { step = s; await page.evaluate((x) => { window.__step = x; }, s).catch(() => {}); };
let expectedConsoleErrors = 0;
page.on('pageerror', (e) => errors.push(`[${step}] pageerror: ${e.message}`));
page.on('console', (msg) => {
  if (msg.type() === 'error') {
    if (expectedConsoleErrors > 0 && msg.text().includes('exitEdit replace')) {
      expectedConsoleErrors--;
      return;
    }
    errors.push(`[${step}] console.error: ${msg.text()}`);
  }
  if (msg.type() === 'warning' && !msg.text().includes('[mock]')) warnings.push(msg.text());
});
let dialogResponse = null;
let confirmResponse = null; // false → answer "Cancel" to the next confirm
let expectedAlerts = 0;
let lastAlert = '';
let alertCount = 0;
let confirmCount = 0;
let lastConfirm = '';
page.on('dialog', async (d) => {
  // confirms are expected UX (dirty-tab close) — accept them; prompts get a
  // scripted answer via dialogResponse; alerts count as errors unless a
  // section explicitly expects one (rejected-commit UX).
  if (d.type() === 'confirm') {
    confirmCount++;
    lastConfirm = d.message();
    if (confirmResponse === false) { confirmResponse = null; await d.dismiss(); }
    else await d.accept();
    return;
  }
  if (d.type() === 'prompt' && dialogResponse != null) {
    const v = dialogResponse; dialogResponse = null;
    await d.accept(v);
    return;
  }
  if (d.type() === 'alert') alertCount++;
  if (d.type() === 'alert' && expectedAlerts > 0) {
    expectedAlerts--;
    lastAlert = d.message();
    await d.accept();
    return;
  }
  errors.push(`[${step}] dialog(${d.type()}): ${d.message()}`);
  await d.dismiss();
});

await page.addInitScript({ path: new URL('./mock-tauri.mjs', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1') });
// Diagnostics: log every programmatic textarea.value write + which element.
await page.addInitScript(() => {
  window.__valLog = [];
  const d = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value');
  Object.defineProperty(HTMLTextAreaElement.prototype, 'value', {
    get: d.get,
    set(v) {
      window.__valLog.push(`SET:${this.className}=${JSON.stringify(String(v)).slice(0, 80)}`);
      return d.set.call(this, v);
    },
  });
});

await setStep("1. Boot");
await page.goto('http://localhost:5173/');
await page.waitForSelector('.md-block', { timeout: 15000 });
const blockCount = await page.locator('.md-block').count();
check('boot: welcome doc renders blocks', blockCount > 0, `${blockCount} blocks`);
await shot(page, '01-welcome');

await setStep("2. Block edit commit");
// Block 1 is a blank-line trivia block: with the trailing separator
// stripped from the editable text it opens EMPTY (previously the textarea
// held a phantom "\n" line — the caret landed past it and line-level
// formats hit the empty line instead of the block's content).
await page.locator('.md-block[data-block-index="1"]').click();
const ta = page.locator('textarea.md-block-textarea');
await ta.waitFor({ state: 'visible', timeout: 5000 });
const taBlank = await ta.inputValue();
check('blank-line block opens with empty textarea', taBlank === '', JSON.stringify(taBlank));
await page.keyboard.press('Escape');
await page.waitForTimeout(300);
// Block 2 is a real paragraph — opens with its source, minus the trailing
// separator.
await page.locator('.md-block[data-block-index="2"]').click();
await ta.waitFor({ state: 'visible', timeout: 5000 });
const taVal = await ta.inputValue();
check('block click opens textarea with source', taVal.includes('source-preserving'), JSON.stringify(taVal.slice(0, 40)));
check('editable text has no phantom trailing line', !taVal.endsWith('\n'), JSON.stringify(taVal.slice(-12)));
await ta.fill('Edited **paragraph** here.');
// Click another block to commit (blur path).
await page.locator('.md-block[data-block-index="0"]').click();
await page.waitForTimeout(300);
const mockText1 = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('block edit commits to buffer', mockText1.includes('Edited **paragraph** here.'), mockText1.slice(0, 80).replace(/\n/g, '|'));
// The stripped separator must be re-appended on commit — the edited
// paragraph must not merge into the following blank line / list block.
check('commit restores the block separator',
  mockText1.includes('Edited **paragraph** here.\n\n- type here'),
  JSON.stringify(mockText1.slice(0, 110)));
const wasBlockReplace = await page.evaluate(() => window.__mockInvokeCalls.some(([c]) => c === 'replace_block'));
check('block edit used replace_block (granular)', wasBlockReplace);
await shot(page, '02-block-edit');

await setStep("3. Toolbar formatting");
// Regression: a toolbar click used to blur the textarea → exitEdit()
// committed + unmounted it BEFORE the format handler ran — the formatting
// silently never applied and the edit session was destroyed.
await page.locator('.md-block[data-block-index="1"]').click();
await ta.waitFor({ state: 'visible', timeout: 5000 });
await ta.fill('select me\n');
await ta.press('Control+Home');
await ta.press('Shift+End');
await page.locator('#btn-bold').click();
await page.waitForTimeout(300);
// The textarea must still be mounted AND focused — no blur/commit happened.
const stillEditing = await page.locator('textarea.md-block-textarea').count();
check('toolbar click keeps edit session', stillEditing === 1);
const taVal2 = await ta.inputValue();
check('bold wraps selection', taVal2.includes('**select me**'), taVal2.slice(0, 60));
// Heading via select (the control whose blur is whitelisted).
await ta.fill('make heading\n');
await ta.press('Control+Home');
await page.locator('#sel-heading').focus();
await page.selectOption('#sel-heading', '2');
await page.waitForTimeout(300);
const taVal3 = await ta.inputValue();
check('heading select adds ##', taVal3.startsWith('##'), taVal3.slice(0, 60));
await page.keyboard.press('Escape');
await page.waitForTimeout(300);

await setStep("4. New doc source view");
await page.locator('#btn-new').click();
await page.waitForTimeout(400);
await page.locator('#btn-view-toggle').click();
const srcTa = page.locator('textarea:not(.md-block-textarea)').first();
await srcTa.waitFor({ state: 'visible', timeout: 5000 });
await srcTa.fill('# Title\n\nline one foo\nline two foo\n');
await page.waitForTimeout(300);
// Source-view edits are committed only when leaving source view (or on
// save) — dirty flag must be visible already though.
const dirtyDot = await page.locator('#dirty-indicator.active').count();
check('source edit marks dirty indicator', dirtyDot === 1);
// Toggle back to rendered — saveSource must push the text into the buffer.
await page.locator('#btn-view-toggle').click();
await page.waitForTimeout(600);
const mockText2 = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('source view edits reach buffer', mockText2.includes('line one foo'), mockText2.slice(0, 50).replace(/\n/g, '|'));
await shot(page, '03-source-view');

await setStep("5. Find & replace");
await page.keyboard.press('Control+f');
const findInput = page.locator('.find-input').first();
await findInput.waitFor({ state: 'visible', timeout: 5000 });
await findInput.fill('foo');
await page.waitForTimeout(400);
const findStatus = await page.locator('.find-bar, .find-status, .find-input').evaluateAll((els) => els.map((e) => e.textContent).join(' '));
// Replace all via ⇄ toggle.
const replToggle = page.locator('button.find-btn.toggle[title="Toggle replace"]');
if (await replToggle.count()) {
  await replToggle.click();
  const replInput = page.locator('.find-input').nth(1);
  await replInput.fill('bar');
  await page.locator('button.find-btn', { hasText: 'Replace All' }).click();
  await page.waitForTimeout(400);
}
const mockText3 = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('replace all applied', mockText3.includes('line one bar') && !mockText3.includes('foo'), mockText3.slice(0, 60).replace(/\n/g, '|'));
await page.keyboard.press('Escape');
await shot(page, '04-find');

await setStep("6. Undo/redo");
await page.locator('#btn-undo').click();
await page.waitForTimeout(300);
const mockText4 = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('undo restores foo', mockText4.includes('foo'), mockText4.slice(0, 60).replace(/\n/g, '|'));
await page.locator('#btn-redo').click();
await page.waitForTimeout(300);
const mockText5 = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('redo reapplies bar', mockText5.includes('bar'), mockText5.slice(0, 60).replace(/\n/g, '|'));

await setStep("7. Open file dialog");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
const blocksNotes = await page.locator('.md-block').count();
check('open notes.md renders blocks', blocksNotes > 10, `${blocksNotes} blocks`);
const titleText = await page.locator('.md-block h1').first().textContent().catch(() => '');
check('h1 rendered', titleText.includes('Notes'), titleText);
await shot(page, '05-notes');

await setStep("8. Rendered elements");
const hasTable = await page.locator('.md-block table').count();
const hasCode = await page.locator('.md-block pre, .md-block code').count();
const hasQuote = await page.locator('.md-block blockquote').count();
const hasList = await page.locator('.md-block ul, .md-block ol').count();
check('renders table/code/quote/list', hasTable > 0 && hasCode > 0 && hasQuote > 0 && hasList > 0, `t=${hasTable} c=${hasCode} q=${hasQuote} l=${hasList}`);

await setStep("9. Task list");
// Rendered task checkboxes are intentionally disabled (block click opens
// the source editor) — verify the rendered marker exists and that clicking
// the task block opens an editor showing the [ ]/[x] source.
const taskItems = await page.locator('.md-block li.task-item').count();
check('task items rendered (disabled checkbox)', taskItems >= 2, `${taskItems} task items`);
const listBlock = page.locator('.md-block', { has: page.locator('li.task-item') }).first();
if (await listBlock.count()) {
  await listBlock.click();
  const ta2 = page.locator('textarea.md-block-textarea');
  await ta2.waitFor({ state: 'visible', timeout: 5000 });
  const ta2val = await ta2.inputValue();
  check('task block edit shows [ ] source', ta2val.includes('[ ]') && ta2val.includes('[x]'), ta2val.slice(0, 80).replace(/\n/g, '|'));
  await page.keyboard.press('Escape');
  await page.waitForTimeout(300);
} else {
  check('task block edit shows [ ] source', false, 'task list block not clickable');
}

await setStep("10. Tab lifecycle");
const tabsBefore = await page.locator('.tab, [class*="tab"]').count();
await page.locator('#btn-new').click();
await page.waitForTimeout(300);
// close active tab via close button if exists
const closeBtn = page.locator('.tab-close, button[title="Close"], .tab .close').first();
if (await closeBtn.count()) {
  await closeBtn.click();
  await page.waitForTimeout(300);
}
const tabsAfter = await page.evaluate(() => window.__tauriMock.state.tabs.length);
check('tab new/close lifecycle', tabsAfter >= 2, `${tabsAfter} tabs in model`);

await setStep("11. Ctrl+S save");
await page.evaluate(() => { window.__mockDialogSave = '/repo/saved.md'; });
await page.keyboard.press('Control+s');
await page.waitForTimeout(400);
const saved = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return { path: t.path, dirty: t.dirty, v: window.__tauriMock.vfs.get(t.path || '') };
});
check('Ctrl+S saves document', saved.path === '/repo/saved.md' && saved.dirty === false, JSON.stringify({ path: saved.path, dirty: saved.dirty }));

await setStep("12. Git panel");
await page.locator('#btn-git').click();
await page.waitForTimeout(600);
const gitRows = await page.locator('.git-panel, [class*="git"]').count();
check('git panel opens', gitRows > 0, `${gitRows} git els`);
await shot(page, '06-git');
// Click through GitPanel tabs.
for (const tabName of ['Branches', 'History', 'Diff', 'Stash', 'Tags', 'Remotes']) {
  const t = page.locator('button, .git-tab, [role="tab"]', { hasText: tabName }).first();
  if (await t.count()) { await t.click(); await page.waitForTimeout(250); }
}
await shot(page, '07-git-tabs');
// Click a changed file → diff view.
const fileRow = page.locator('text=notes.md').first();
if (await fileRow.count()) { await fileRow.click(); await page.waitForTimeout(400); }
await shot(page, '08-git-diff');

await setStep("13. File tree");
await page.locator('#btn-tree-toggle').click();
await page.waitForTimeout(500);
const treeItems = await page.locator('.file-tree [class*="item"], .tree-item, [class*="file"]').count();
check('file tree visible', treeItems > 0, `${treeItems} items`);
await shot(page, '09-filetree');

await setStep("14. Big doc virtualization");
// open via dialog path
await page.evaluate(() => { window.__mockDialogOpen = '/repo/big.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(800);
const bigBlocks = await page.locator('.md-block').count();
const bigMeta = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return { len: t.bytes.length, blocks: window.__tauriMock.docInfo(t).block_count };
});
check('big doc opens', bigBlocks > 0, `${bigBlocks} rendered of ${bigMeta.blocks} blocks`);
// Scroll to bottom to force lazy block loads — the tail block must appear.
await page.evaluate(() => { const v = document.querySelector('#block-editor'); v.scrollTop = v.scrollHeight; });
await page.waitForTimeout(600);
const tailVisible = await page.locator('.md-block', { hasText: 'Paragraph number 399' }).count();
check('virtualized scroll renders tail block', tailVisible === 1);
await shot(page, '10-bigdoc-scrolled');

await setStep("15. Ctrl+N");
await page.keyboard.press('Control+n');
await page.waitForTimeout(300);
const tabsCount = await page.evaluate(() => window.__tauriMock.state.tabs.length);
check('Ctrl+N opens new tab', tabsCount >= 3, `${tabsCount} tabs`);

// ── 16. CRLF document: block edit must restore \r\n endings ────────────
await setStep("16. CRLF edit");
// The textarea API normalizes to \n; exitEdit must restore the document's
// own EOL on commit, or a CRLF file gets silently rewritten to LF.
await page.evaluate(() => { window.__mockDialogOpen = '/repo/crlf.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
const crlfBlocks = await page.locator('.md-block').count();
check('crlf.md opens', crlfBlocks >= 4, `${crlfBlocks} blocks`);
// Edit the list block.
const crlfList = page.locator('.md-block', { has: page.locator('li') }).first();
if (await crlfList.count()) {
  await crlfList.click();
  const ta3 = page.locator('textarea.md-block-textarea');
  await ta3.waitFor({ state: 'visible', timeout: 5000 });
  await ta3.fill('- a\r\n- b\n- c\n');
  await page.keyboard.press('Escape');
  await page.waitForTimeout(400);
}
const crlfText = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
const crlfCount = (crlfText.match(/\r\n/g) || []).length;
const loneLf = (crlfText.match(/[^\r]\n/g) || []).length;
check('crlf edit preserves CRLF endings', crlfText.includes('- c\r\n') && loneLf === 0, `crlf=${crlfCount} loneLf=${loneLf}`);

// ── 17. Menu bar click-through ──────────────────────────────────────────
await setStep("17. Menu bar");
await page.locator('.menu-top', { hasText: 'File' }).first().click();
await page.waitForTimeout(300);
const menuItems = await page.locator('.menu-dropdown .menu-item:not(.separator)').count();
check('File menu opens with items', menuItems > 0, `${menuItems} items`);
await shot(page, '11-menu');
// Standard menubar behavior: hover to the next menu switches while open.
await page.locator('.menu-top', { hasText: 'Edit' }).first().hover();
await page.waitForTimeout(300);
const editMenuText = await page.locator('.menu-dropdown').first().textContent().catch(() => '');
check('hover switches to Edit menu', editMenuText.includes('Undo') && editMenuText.includes('Find'), editMenuText.slice(0, 40));
await page.keyboard.press('Escape');
// Click empty area to close any menu leftovers.
await page.locator('#block-editor').click({ position: { x: 10, y: 10 } }).catch(() => {});

// ── 18. Context menu (right-click) ──────────────────────────────────────
await setStep("18. Context menu");
await page.locator('.md-block').first().click({ button: 'right' });
await page.waitForTimeout(300);
const ctxItems = await page.locator('.context-menu .context-item:not(.separator)').count();
check('context menu opens on right-click', ctxItems > 5, `${ctxItems} items`);
// Context menu formatting must not kill an edit session (mousedown.prevent).
await page.locator('.context-menu .context-item', { hasText: 'Bold' }).first().click();
await page.waitForTimeout(300);
const ctxClosed = await page.locator('.context-menu').evaluate((el) => getComputedStyle(el).display === 'none').catch(() => true);
check('context menu closes after item click', ctxClosed);

// ── 19. Find bar navigation ─────────────────────────────────────────────
await setStep("19. Find bar");
// Back on notes.md for a doc with several matches (open_document dedups
// into the already-open tab).
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(500);
await page.keyboard.press('Control+f');
await page.locator('.find-input').first().fill('item');
await page.waitForTimeout(400);
const findCount = await page.locator('.find-count').textContent().catch(() => '');
check('find shows match count', /\d/.test(findCount) && !findCount.startsWith('0'), findCount.trim());
const rbBeforeFind = await page.evaluate(() =>
  window.__mockInvokeCalls.filter(([c]) => c === 'replace_block').length);
await page.keyboard.press('Enter'); // next match
await page.waitForTimeout(300);
// Regression: Enter navigates to the match (selected inside the block's
// textarea) but DOM focus must stay in the find input — otherwise the next
// Enter types a newline over the selected match and silently corrupts the
// document while the user thinks they're paging through matches.
const findFocus = await page.evaluate(() => document.activeElement?.className || '');
check('find Enter keeps focus in find bar', findFocus.includes('find-input'), findFocus);
await page.keyboard.press('Shift+Enter'); // prev match — must NOT type into the document
await page.waitForTimeout(200);
await page.keyboard.press('Enter'); // and back to next — still no typing
await page.waitForTimeout(200);
const rbAfterFind = await page.evaluate(() =>
  window.__mockInvokeCalls.filter(([c]) => c === 'replace_block').length);
check('find navigation leaves document untouched', rbAfterFind === rbBeforeFind,
  `replace_block calls ${rbBeforeFind} -> ${rbAfterFind}`);
const matchCountVal = await page.evaluate(() => window.__mockInvokeCalls.filter(([c]) => c === 'search_document').length);
check('find issues search_document calls', matchCountVal > 0, `${matchCountVal} calls`);
await page.locator('.find-btn.close').click();

// ── 20. Git stage → commit flow ─────────────────────────────────────────
await setStep("20. Git stage/commit");
// The git panel is already open (on Remotes) — switch to the Changes tab.
await page.locator('.git-tab', { hasText: 'Changes' }).first().click();
await page.waitForTimeout(400);
// Stage notes.md via its + button.
await page.locator('.git-mini-btn', { hasText: '+' }).first().click();
await page.waitForTimeout(400);
const stagedVisible = await page.locator('.git-section-title', { hasText: 'Staged' }).count();
check('stage moves file to Staged section', stagedVisible === 1);
// Unstage it back.
await page.locator('.git-mini-btn', { hasText: '−' }).first().click();
await page.waitForTimeout(400);
// Stage all via the same + (still shows in Changes).
await page.locator('.git-mini-btn', { hasText: '+' }).first().click();
await page.waitForTimeout(400);
await page.locator('input[placeholder="Commit message"]').fill('probe commit');
await page.locator('.git-action-btn', { hasText: 'Commit' }).click();
await page.waitForTimeout(500);
const commitOk = await page.evaluate(() => {
  // read mock git state through a fresh invoke
  const calls = window.__mockInvokeCalls.filter(([c]) => c === 'git_commit');
  return calls.length > 0 && calls[0][1]?.message === 'probe commit';
});
check('commit invoked with message', commitOk);
// History tab should show the new commit on top.
await page.locator('.git-tab', { hasText: 'History' }).first().click();
await page.waitForTimeout(400);
const probeCommit = await page.locator('.git-commit-entry', { hasText: 'probe commit' }).count();
check('history shows probe commit', probeCommit === 1);

// ── 21. Empty doc must not stick on "Loading blocks…" ───────────────────
await setStep("21. Empty doc");
await page.locator('#btn-new').click();
await page.waitForTimeout(600);
const stuckLoading = await page.locator('.md-block-placeholder', { hasText: 'Loading blocks' })
  .evaluateAll((els) => els.some((el) => el.offsetParent !== null)).catch(() => false);
check('empty doc does not show loading placeholder', !stuckLoading);

// ── 22. Link insertion via prompt dialog ────────────────────────────────
await setStep("22. Link insert");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/crlf.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
await page.locator('.md-block', { hasText: 'a' }).first().click();
const taL = page.locator('textarea.md-block-textarea');
await taL.waitFor({ state: 'visible', timeout: 5000 });
await taL.press('Control+End');
dialogResponse = 'https://example.com';
await page.locator('#btn-link').click();
await page.waitForTimeout(300);
const linkVal = await taL.inputValue();
check('link button inserts [text](url)', linkVal.includes('](https://example.com)') || linkVal.includes('https://example.com'), linkVal.slice(0, 80).replace(/\n/g, '|'));
await page.keyboard.press('Escape');
await page.waitForTimeout(300);

// ── 23. Help + Settings modals ──────────────────────────────────────────
await setStep("23. Modals");
await page.keyboard.press('F1');
await page.waitForTimeout(300);
const helpVisible = await page.locator('.modal-overlay:not(.hidden) .modal-title', { hasText: 'Help' }).count();
check('F1 opens Help modal', helpVisible === 1);
await page.locator('.modal-overlay:not(.hidden) .modal-close').first().click();
await page.waitForTimeout(200);
await page.locator('#btn-settings').click();
await page.waitForTimeout(300);
const settingsVisible = await page.locator('#settings-modal:not(.hidden)').count();
check('settings modal opens', settingsVisible === 1);
await page.locator('#settings-close').click();

// ── 24. Thematic break at EOF must not corrupt the last paragraph ───────
await setStep("24. Thematic break EOF");
// Regression: appending a bare "---\n" right after the last paragraph line
// re-parsed it as a SETEXT heading — the text silently became an <h2>.
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(500);
await page.locator('#btn-hr').click();
await page.waitForTimeout(700);
const hrTail = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('hr appended with blank separator', /\n\n---\n$/.test(hrTail), JSON.stringify(hrTail.slice(-45)));
const hrEls = await page.locator('.md-block hr').count();
check('thematic break renders as <hr>', hrEls >= 1, `${hrEls} hr`);
const h2Texts = await page.locator('.md-block h2').allTextContents();
check('last paragraph did not become a heading', !h2Texts.some((t) => t.includes('Trailing')), JSON.stringify(h2Texts));

// ── 25. Ctrl+B hotkey inside a block edit ───────────────────────────────
await setStep("25. Ctrl+B hotkey");
await page.locator('.md-block', { hasText: 'Trailing paragraph' }).first().click();
const ta4 = page.locator('textarea.md-block-textarea');
await ta4.waitFor({ state: 'visible', timeout: 5000 });
await ta4.press('Control+Home');
await ta4.press('Shift+End');
await page.keyboard.press('Control+b');
await page.waitForTimeout(300);
const ta4val = await ta4.inputValue();
check('Ctrl+B wraps selection', ta4val.includes('**Trailing paragraph.**'), ta4val.slice(0, 60));
await page.keyboard.press('Escape');
await page.waitForTimeout(400);

// ── 26. Regex search + invalid regex ────────────────────────────────────
await setStep("26. Regex find");
await page.keyboard.press('Control+f');
await page.locator('.find-input').first().fill('par.*ph');
await page.locator('.find-btn.toggle[title="Use regular expression"]').click();
await page.waitForTimeout(400);
const reCount = await page.locator('.find-count').textContent().catch(() => '');
check('regex search finds matches', /\d/.test(reCount) && !reCount.startsWith('0'), reCount.trim());
await page.locator('.find-input').first().fill('(unclosed');
await page.waitForTimeout(400);
const invalidCls = await page.locator('.find-count').evaluate((el) => el.className).catch(() => '');
check('invalid regex flagged', invalidCls.includes('invalid'), invalidCls);
await page.locator('.find-btn.close').click();

// ── 27. Drag-select in rendered view does not enter edit ────────────────
await setStep("27. Drag select");
const para = page.locator('.md-block', { hasText: 'Trailing paragraph' }).first();
const box = await para.boundingBox();
await page.mouse.move(box.x + 10, box.y + box.height / 2);
await page.mouse.down();
await page.mouse.move(box.x + box.width - 10, box.y + box.height / 2, { steps: 8 });
await page.mouse.up();
await page.waitForTimeout(400);
const noEditAfterDrag = await page.locator('textarea.md-block-textarea').count();
const selLen = await page.evaluate(() => window.getSelection()?.toString().length || 0);
check('drag-select keeps selection, no edit session', noEditAfterDrag === 0 && selLen > 3, `selLen=${selLen} editing=${noEditAfterDrag}`);
// Chromium keeps the selection alive through a click inside it (possible
// text-drag) — the first click only deselects. Clear it so the next steps
// are deterministic.
await page.evaluate(() => window.getSelection()?.removeAllRanges());

// ── 28. Sequential edits on two different blocks stay consistent ────────
// Each step must wait for the actual textarea CONTENT, not just visibility:
// enterEdit loads block data asynchronously, and a premature click lands in
// the still-loading (or still-list) session — the seq guard then aborts the
// half-entered edit, which is correct app behavior but invalid for the test.
await setStep("28. Sequential edits");
await page.locator('.md-block', { hasText: 'item one' }).first().click();
let ta5 = page.locator('textarea.md-block-textarea');
await ta5.waitFor({ state: 'visible', timeout: 5000 });
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const v1 = await ta5.inputValue();
await ta5.fill(v1.replace('item one', 'item one!'));
// Commit block A by clicking block B, then edit B too.
await page.locator('.md-block', { hasText: 'cyrillic' }).first().click();
ta5 = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('привет'),
  null, { timeout: 5000 });
const v2 = await ta5.inputValue();
await ta5.fill(v2.replace('текст', 'текст!'));
await page.locator('.md-block', { hasText: 'Notes' }).first().click(); // commit block B
await page.waitForTimeout(500);
const seqText = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
const rbCalls = await page.evaluate(() => window.__mockInvokeCalls
  .filter(([c]) => c === 'replace_block')
  .map(([, a, s]) => `[${s}]#${a.blockIndex}[${a.expectedStart},${a.expectedEnd}] "${(a.newSource || '').slice(0, 40).replace(/\n/g, '|')}"`)
  .join(' '));
const valTail = await page.evaluate(() =>
  (window.__gbd || []).slice(-8).map(([i, s]) => `gbd#${i}=${s}`).join(' ') + ' || ' +
  window.__valLog.slice(-8).join(' '));
const seqOk = seqText.includes('item one!') && seqText.includes('текст!');
check('sequential block edits both applied', seqOk, seqOk ? '' : rbCalls + ' || ' + valTail);

// ── 29. Find & Replace — single replace goes through the backend ────────
await setStep("29. Find replace single");
// Ctrl+H opens find WITH the replace row (the ⇄ toggle persists across
// opens, so a blind toggle click could switch a visible row OFF).
await page.keyboard.press('Control+h');
await page.locator('.find-input').first().fill('текст!');
await page.waitForTimeout(400);
await page.locator('.find-row').nth(1).locator('.find-input').fill('CYRTEXT');
await page.locator('.find-btn', { hasText: 'Replace' }).first().click();
await page.waitForTimeout(600);
const replCalls = await page.evaluate(() => window.__mockInvokeCalls.filter(([c]) => c === 'replace_match_in_document').length);
const replDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('find replace updated document via backend',
  replCalls === 1 && replDoc.includes('CYRTEXT') && !replDoc.includes('текст!'),
  `calls=${replCalls} doc=${replDoc.includes('CYRTEXT')}`);
await page.locator('.find-btn.close').click();
await page.waitForTimeout(200);

// ── 30. Undo/redo across a find-replace commit ──────────────────────────
await setStep("30. Undo/redo");
await page.keyboard.press('Control+z');
await page.waitForTimeout(600);
const undoneDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('undo reverts the replace', undoneDoc.includes('текст!') && !undoneDoc.includes('CYRTEXT'));
await page.keyboard.press('Control+y');
await page.waitForTimeout(600);
const redoneDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('redo re-applies the replace', redoneDoc.includes('CYRTEXT'));

// ── 31. Switching to source view commits the pending block edit ─────────
await setStep("31. View switch commits edit");
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taV = page.locator('textarea.md-block-textarea');
await taV.waitFor({ state: 'visible', timeout: 5000 });
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const vv = await taV.inputValue();
await taV.fill(vv.replace('item one!', 'item one VIEWTEST'));
await page.locator('#btn-view-toggle').click();
await page.waitForTimeout(800);
const srcVal = await page.locator('textarea:not(.md-block-textarea)').inputValue().catch(() => '');
check('view switch commits pending block edit', srcVal.includes('item one VIEWTEST'),
  srcVal.slice(0, 80).replace(/\n/g, '|'));
await page.locator('#btn-view-toggle').click();
await page.waitForTimeout(800);

// ── 32. Switching tabs commits the pending block edit ───────────────────
await setStep("32. Tab switch commits edit");
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taT = page.locator('textarea.md-block-textarea');
await taT.waitFor({ state: 'visible', timeout: 5000 });
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const vt = await taT.inputValue();
await taT.fill(vt.replace('item one VIEWTEST', 'item one TABTEST'));
await page.locator('.tab:not(.active)').first().click(); // away — commit must fire
await page.waitForTimeout(500);
await page.locator('.tab', { hasText: 'notes' }).first().click();
await page.waitForTimeout(600);
const tabDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return { name: t.name, text: new TextDecoder().decode(t.bytes) };
});
check('tab switch commits pending block edit',
  tabDoc.name === 'notes.md' && tabDoc.text.includes('item one TABTEST'),
  `tab=${tabDoc.name} has=${tabDoc.text.includes('item one TABTEST')}`);

// ── 33. Rendered link clicks — external vs relative ─────────────────────
// Clicking <a> inside a rendered block must NOT enter edit mode or let the
// browser navigate: external links go to open_url, relative ones open tabs.
await setStep("33. Link clicks");
await page.locator('.md-block a[href="https://example.com"]').first().click();
await page.waitForTimeout(300);
const urlCalls = await page.evaluate(() => window.__mockInvokeCalls.filter(([c]) => c === 'open_url'));
check('external link invokes open_url',
  urlCalls.length === 1 && urlCalls[0][1].url === 'https://example.com',
  JSON.stringify(urlCalls[0]?.[1] || {}));
const taAfterLink = await page.locator('textarea.md-block-textarea').count();
check('link click does not enter edit mode', taAfterLink === 0, `${taAfterLink} textareas`);
await page.locator('.md-block a[href="sub/deep.md"]').first().click();
await page.waitForTimeout(600);
const navTab = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return { name: t.name, path: t.path };
});
check('relative link opens file in a tab',
  navTab.name === 'deep.md' && navTab.path === '/repo/sub/deep.md',
  JSON.stringify(navTab));

// ── 34. File tree — go up to repo root, click a file, opens it ──────────
await setStep("34. File tree open");
// The tree root follows the active file's directory — deep.md put it in
// /repo/sub, so walk up to /repo first.
const rootName = await page.locator('#file-tree-root-name').textContent().catch(() => '');
if (!rootName.includes('repo')) {
  await page.locator('#btn-tree-up').click();
  await page.waitForTimeout(400);
}
const treeFile = page.locator('.file-tree-item.file', { hasText: 'new.md' }).first();
if (await treeFile.count()) {
  await treeFile.click();
  await page.waitForTimeout(500);
  const treeTab = await page.evaluate(() => {
    const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
    return t.name;
  });
  check('file tree click opens file', treeTab === 'new.md', `root=${rootName.trim()} tab=${treeTab}`);
} else {
  const items = await page.locator('.file-tree-item').allTextContents().catch(() => []);
  check('file tree click opens file', false, `root=${rootName.trim()} items=${items.slice(0, 6)}`);
}

// ── 35. Ctrl+Z commits the pending edit, then undoes it as one step ─────
// The uncommitted textarea text must NOT be silently dropped: undo() first
// commits it via save(), then undoes that commit — so the document returns
// to the pre-edit state and Ctrl+Y can bring the whole edit back.
await setStep("35. Ctrl+Z uncommitted edit");
await page.locator('.tab', { hasText: 'notes' }).first().click();
await page.waitForTimeout(600);
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taU = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const vu = await taU.inputValue();
await taU.fill(vu.replace('item one TABTEST', 'item one ZZZTEST'));
await page.keyboard.press('Control+z');
await page.waitForTimeout(700);
const undoDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('ctrl+z undoes the pending edit as one step',
  undoDoc.includes('item one TABTEST') && !undoDoc.includes('ZZZTEST'));
await page.keyboard.press('Control+y');
await page.waitForTimeout(600);
const redoDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('ctrl+y redoes it', redoDoc.includes('item one ZZZTEST'));

// ── 36. Save As (Ctrl+Shift+S) writes to the dialog path ────────────────
await setStep("36. Save As");
await page.evaluate(() => { window.__mockDialogSave = '/repo/saved-as.md'; });
await page.keyboard.press('Control+Shift+s');
await page.waitForTimeout(600);
const savedAs = await page.evaluate(() => ({
  exists: window.__tauriMock.vfs.has('/repo/saved-as.md'),
  content: window.__tauriMock.vfs.get('/repo/saved-as.md') || '',
}));
check('Ctrl+Shift+S saves to new path',
  savedAs.exists && savedAs.content.includes('item one ZZZTEST'),
  `exists=${savedAs.exists}`);

// ── 37. Closing a dirty tab asks for confirmation, then closes ──────────
// After Save As the tab was renamed, so reopen notes.md and dirty it with a
// fresh edit before closing.
await setStep("37. Dirty tab close");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taD = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const vd = await taD.inputValue();
await taD.fill(vd.replace('item one', 'item one DIRTY'));
await page.keyboard.press('Escape'); // commit → dirty flag set
await page.waitForTimeout(500);
await page.locator('.tab', { hasText: 'notes' }).first().locator('.tab-close').click();
await page.waitForTimeout(600);
const notesTabLeft = await page.locator('.tab', { hasText: 'notes.md' }).count();
check('dirty tab closes after confirm', notesTabLeft === 0, `${notesTabLeft} notes tabs left`);

// ── 38. Rejected commit keeps the edit session + user text ──────────────
// If the document changed behind the editor's back, replace_block rejects
// (stale coordinates). The app must alert AND keep the textarea open with
// the uncommitted text — never silently drop it.
await setStep("38. Stale commit rejection");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/saved-as.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taShift = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
const vs = await taShift.inputValue();
await taShift.fill(vs.replace('item one ZZZTEST', 'item one UNCOMMITTED'));
// Mutate the document behind the editor's back: the block's real span moves,
// so the frontend's expectedStart/End go stale.
await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  const ins = new TextEncoder().encode('## INJECTED\n\n');
  const nb = new Uint8Array(ins.length + t.bytes.length);
  nb.set(ins); nb.set(t.bytes, ins.length);
  t.bytes = nb;
});
expectedConsoleErrors = 1; // exitEdit logs the rejection to console.error
const confirmsBefore = confirmCount;
await page.locator('.md-block', { hasText: 'CYRTEXT' }).first().click();
await page.waitForTimeout(700);
const staleTa = page.locator('textarea.md-block-textarea');
const staleVal = await staleTa.count() ? await staleTa.inputValue() : '';
check('rejected commit keeps session with user text',
  lastConfirm.includes('document changed') && staleVal.includes('UNCOMMITTED'),
  `confirm=${lastConfirm.slice(0, 60)} ta=${staleVal.slice(0, 50)}`);
// One click-away gesture = ONE confirm dialog: blur's commit and the
// click's own exitEdit must not each fire the same doomed commit.
check('single confirm per rejected gesture', confirmCount - confirmsBefore === 1,
  `${confirmCount - confirmsBefore} confirms`);
// Abandon the session via the discard path: typing re-arms the commit, the
// retry rejects again, and answering Cancel drops the edit + reloads.
confirmResponse = false;
await staleTa.fill(staleVal + ' ');
expectedConsoleErrors = 1;
await page.keyboard.press('Escape');
await page.waitForTimeout(700);
const taAfterDiscard = await page.locator('textarea.md-block-textarea').count();
check('discard path drops session + resyncs', taAfterDiscard === 0, `${taAfterDiscard} textareas`);

// ── 39. Find navigation in source view selects without stealing focus ────
await setStep("39. Find in source view");
await page.locator('#btn-view-toggle').click();
await page.waitForTimeout(800);
await page.keyboard.press('Control+f');
await page.locator('.find-input').first().fill('ZZZTEST');
await page.waitForTimeout(400);
await page.keyboard.press('Enter');
await page.waitForTimeout(400);
const srcSel = await page.evaluate(() => {
  const ta = document.querySelector('textarea.md-source-view');
  return {
    focused: document.activeElement?.className || '',
    sel: ta ? ta.selectionEnd - ta.selectionStart : -1,
    txt: ta ? ta.value.slice(ta.selectionStart, ta.selectionEnd) : '',
  };
});
check('source-view find selects the match', srcSel.sel === 'ZZZTEST'.length && srcSel.txt === 'ZZZTEST',
  JSON.stringify(srcSel));
check('source-view find keeps focus in find bar', srcSel.focused.includes('find-input'), srcSel.focused);
await page.locator('.find-btn.close').click();

// ── 40. Toolbar formatting works in source view ──────────────────────────
await setStep("40. Toolbar in source view");
await page.evaluate(() => {
  const ta = document.querySelector('textarea.md-source-view');
  const i = ta.value.indexOf('ZZZTEST');
  ta.focus();
  ta.setSelectionRange(i, i + 'ZZZTEST'.length);
});
await page.locator('#btn-bold').click();
await page.waitForTimeout(300);
const srcAfterBold = await page.locator('textarea.md-source-view').inputValue();
check('toolbar bold wraps in source view', srcAfterBold.includes('**ZZZTEST**'));
await page.locator('#btn-view-toggle').click(); // back to rendered — commits source
await page.waitForTimeout(700);
const docAfterSrcBold = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('source-view format committed to buffer', docAfterSrcBold.includes('**ZZZTEST**'));

// ── 41. Status bar reports cursor position and line ending ───────────────
await setStep("41. Status bar");
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taSB = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item one'),
  null, { timeout: 5000 });
await page.waitForTimeout(300);
const cursorPos = await page.locator('#cursor-pos').textContent();
check('status bar shows Ln/Col', /Ln \d+, Col \d+/.test(cursorPos), cursorPos);
const blockCountTxt = await page.locator('#block-count').textContent();
check('status bar shows block count', /\d+ blocks/.test(blockCountTxt), blockCountTxt);
await page.keyboard.press('Escape');
await page.waitForTimeout(300);
// crlf.md must report CRLF in the status bar.
await page.evaluate(() => { window.__mockDialogOpen = '/repo/crlf.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(700);
const eolText = await page.locator('#line-ending').textContent();
check('status bar reports CRLF', eolText === 'CRLF', eolText);

// ── 42. Context menu formats a rendered-text selection ───────────────────
await setStep("42. Context menu on selection");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
const itemBlock = page.locator('.md-block', { hasText: 'item one' }).first();
const itemBox = await itemBlock.boundingBox();
// Drag-select across the first line ("item one") of the rendered list.
await page.mouse.move(itemBox.x + 8, itemBox.y + 10);
await page.mouse.down();
await page.mouse.move(itemBox.x + 110, itemBox.y + 10, { steps: 6 });
await page.mouse.up();
await page.waitForTimeout(200);
await itemBlock.click({ button: 'right' });
await page.waitForTimeout(300);
await page.locator('.context-menu .context-item', { hasText: 'Bold' }).first().click();
await page.waitForTimeout(500);
const ctxDoc = await page.evaluate(() => {
  const t = window.__tauriMock.state.tabs[window.__tauriMock.state.active];
  return new TextDecoder().decode(t.bytes);
});
check('context-menu bold wraps rendered selection', ctxDoc.includes('**item'),
  ctxDoc.slice(90, 220).replace(/\n/g, '|'));

// ── 43. Heading select applies to the editing block ─────────────────────
await setStep("43. Heading select");
await page.locator('.md-block', { hasText: 'cyrillic' }).first().click();
const taH = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('cyrillic'),
  null, { timeout: 5000 });
await page.locator('#sel-heading').selectOption('2');
await page.waitForTimeout(300);
const headVal = await taH.inputValue();
check('heading select prefixes block', headVal.startsWith('## '), headVal.slice(0, 40));
await page.keyboard.press('Escape');
await page.waitForTimeout(400);

// ── 44. Line-prefix toolbar buttons during block edit ───────────────────
await setStep("44. Line format buttons");
await page.locator('.md-block', { hasText: 'deep page' }).first().click();
const taQ = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('deep page'),
  null, { timeout: 5000 });
await page.locator('#btn-quote').click();
await page.waitForTimeout(300);
const quoteVal = await taQ.inputValue();
check('quote button prefixes lines', quoteVal.startsWith('> '), quoteVal.slice(0, 50));
await page.keyboard.press('Escape');
await page.waitForTimeout(400);

// ── 45. Menu items actually trigger their actions ────────────────────────
await setStep("45. Menu item actions");
await page.locator('.menu-top', { hasText: 'Help' }).first().click();
await page.waitForTimeout(300);
await page.locator('.menu-dropdown .menu-item', { hasText: 'About' }).first().click();
await page.waitForTimeout(400);
const aboutVisible = await page.locator('.modal-overlay:not(.hidden) .modal-title, .modal:not(.hidden)').count();
check('Help > About opens a modal', aboutVisible >= 1, `${aboutVisible} modals`);
// Close whatever modal opened (About or Help).
await page.locator('.modal-overlay:not(.hidden) .modal-close, .modal .modal-close').first().click().catch(() => {});
await page.waitForTimeout(200);

// ── 46. Ctrl+Enter commits the block edit and closes the session ────────
await setStep("46. Ctrl+Enter commits");
await page.locator('.md-block', { hasText: 'item one' }).first().click();
const taE = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('item'),
  null, { timeout: 5000 });
const ve = await taE.inputValue();
await taE.fill(ve.replace(/item/, 'item ENTERED'));
await taE.press('Control+Enter');
await page.waitForTimeout(500);
const afterCtrlEnter = await page.evaluate(() => ({
  ta: document.querySelectorAll('textarea.md-block-textarea').length,
  text: new TextDecoder().decode(
    window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes),
}));
check('Ctrl+Enter commits + closes session',
  afterCtrlEnter.ta === 0 && afterCtrlEnter.text.includes('item ENTERED'),
  `ta=${afterCtrlEnter.ta} has=${afterCtrlEnter.text.includes('item ENTERED')}`);

// ── 47. Find & Replace: single replace + replace all ─────────────────────
await setStep("47. Replace current + all");
// Fresh notes.md so the match count is deterministic.
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
await page.keyboard.press('Control+h');
await page.waitForSelector('.find-bar', { timeout: 5000 });
await page.locator('.find-bar .find-input').first().fill('item');
await page.waitForTimeout(500);
await page.locator('.find-bar .find-input').nth(1).fill('THING');
await page.locator('.find-bar .find-btn', { hasText: 'Replace All' }).click();
await page.waitForTimeout(700);
const afterReplaceAll = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
// The notes.md tab stayed open through earlier sections (dedup by path), so
// the first list item is '- **item ENTERED one**' — 2 'item' matches total.
check('replace all rewrote every match',
  afterReplaceAll.includes('THING ENTERED one') && afterReplaceAll.includes('THING two')
    && !afterReplaceAll.includes('item'),
  afterReplaceAll.slice(100, 200).replace(/\n/g, '|'));
const noResults = await page.locator('.find-count').textContent();
check('counter reports no results after replace-all',
  noResults === 'No results' || noResults.trim() === '', JSON.stringify(noResults));
await page.keyboard.press('Escape');
await page.waitForTimeout(200);

// ── 48. Match offsets refresh after a block commit ───────────────────────
await setStep("48. Stale-match refresh");
// Regression: searchMatches held byte offsets computed before the edit — a
// commit shifted every later byte, so the next Enter selected a wrong range
// (and Replace could send coordinates the backend would reject).
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
await page.keyboard.press('Control+f');
// 'deep' occurs twice, both after 'Trailing paragraph.': 'deep page' and 'deep.md'.
await page.locator('.find-bar .find-input').first().fill('deep');
await page.waitForTimeout(500);
await page.keyboard.press('Enter'); // match 1/2 selected in its block
await page.waitForTimeout(500);
// Edit a block ABOVE the matches — every 'deep' offset shifts by +len.
await page.locator('.md-block', { hasText: 'Trailing paragraph' }).first().click();
const ta48 = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('Trailing'),
  null, { timeout: 5000 });
await ta48.fill('Trailing paragraph ZZZ');
await page.keyboard.press('Escape');
await page.waitForTimeout(500);
await page.locator('.find-bar .find-input').first().press('Enter'); // next match
await page.waitForTimeout(600);
const selAfterShift = await page.evaluate(() => {
  const t = document.querySelector('textarea.md-block-textarea');
  return t ? t.value.substring(t.selectionStart, t.selectionEnd) : '(no ta)';
});
check('post-edit navigation still selects the match',
  selAfterShift === 'deep', JSON.stringify(selAfterShift));
await page.keyboard.press('Escape');
await page.waitForTimeout(200);

// ── 49. Find jump to a block deep in a virtualized document ──────────────
await setStep("49. Far find jump (virtualized)");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/big.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(800);
await page.keyboard.press('Control+f');
await page.locator('.find-bar .find-input').first().fill('Paragraph number 399');
await page.waitForTimeout(500);
await page.locator('.find-bar .find-input').first().press('Enter');
await page.waitForTimeout(1200); // ensureBlockVisible converges over frames
const farJump = await page.evaluate(() => {
  const t = document.querySelector('textarea.md-block-textarea');
  // The edited block renders AS the textarea — its text isn't in
  // textContent, so check the .md-block wrapper itself is in the DOM and
  // inside the viewport.
  const holder = t?.closest('.md-block');
  const r = holder?.getBoundingClientRect();
  const onScreen = !!r && r.bottom > 0 && r.top < innerHeight;
  return {
    ta: !!t,
    sel: t ? t.value.substring(t.selectionStart, t.selectionEnd) : '',
    holder: !!holder,
    onScreen,
  };
});
check('find jumps to tail block in 800-block doc',
  farJump.ta && farJump.sel === 'Paragraph number 399' && farJump.holder && farJump.onScreen,
  JSON.stringify(farJump));
await page.keyboard.press('Escape');
await page.waitForTimeout(200);

// ── 50. Regex replace-all with capture expansion ─────────────────────────
await setStep("50. Regex replace-all");
await page.keyboard.press('Control+h');
// Toggle state persists across find-bar opens — enable regex only if off.
const reBtn = page.locator('.find-bar .find-btn.toggle[title*="regular"]');
if (!((await reBtn.getAttribute('class')) || '').includes('active')) await reBtn.click();
await page.locator('.find-bar .find-input').first().fill('Paragraph number (\\d+)');
await page.waitForTimeout(500);
await page.locator('.find-bar .find-input').nth(1).fill('Para-$1');
await page.locator('.find-bar .find-btn', { hasText: 'Replace All' }).click();
await page.waitForTimeout(900);
const afterRegexAll = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
check('regex replace-all expands captures',
  afterRegexAll.includes('Para-399 with some filler')
    && !afterRegexAll.includes('Paragraph number'),
  afterRegexAll.slice(-120).replace(/\n/g, '|'));
await page.keyboard.press('Escape');
await page.waitForTimeout(200);

// ── 51. Ordered list swaps markers instead of stacking ───────────────────
await setStep("51. Ordered list swaps markers");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
// The list block still reads '- THING ENTERED one\n- THING two\n- [ ]...'
// from section 47 — ordered-list must REPLACE each marker, not produce
// corrupt '1. - item' lines.
await page.locator('.md-block', { hasText: 'THING two' }).first().click();
const taOL = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('THING'),
  null, { timeout: 5000 });
// Select the whole block first — line prefixes apply to the lines the
// selection touches (caret at end would only rewrite the last item).
await taOL.press('Control+a');
await page.locator('#btn-ol').click();
await page.waitForTimeout(300);
const olVal = await taOL.inputValue();
const olLines = olVal.split('\n').filter((l) => l);
check('ordered list replaces - markers with 1.',
  olLines.length > 0 && olLines.every((l) => /^1\. /.test(l)),
  JSON.stringify(olVal.slice(0, 80)));
check('no stacked "1. -" corruption', !olVal.includes('1. -'), '');
await page.keyboard.press('Escape');
await page.waitForTimeout(400);
const olCommitted = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
check('marker swap committed', olCommitted.includes('1. THING two'),
  olCommitted.slice(80, 200).replace(/\n/g, '|'));

// ── 52. Context-menu Link on a rendered-text selection ───────────────────
await setStep("52. Context Link on selection");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/notes.md'; });
await page.locator('#btn-open').click();
await page.waitForTimeout(600);
const trailBlock = page.locator('.md-block', { hasText: 'Trailing paragraph' }).first();
// The paragraph sits near the document tail — below the 900px fold — so a
// drag at its natural position lands off-screen and selects nothing.
await trailBlock.scrollIntoViewIfNeeded();
await page.waitForTimeout(300);
const trailBox = await trailBlock.boundingBox();
// Drag-select the first word ("Trailing") of the rendered paragraph.
await page.mouse.move(trailBox.x + 6, trailBox.y + 10);
await page.mouse.down();
await page.mouse.move(trailBox.x + 62, trailBox.y + 10, { steps: 6 });
await page.mouse.up();
await page.waitForTimeout(200);
await trailBlock.click({ button: 'right' });
await page.waitForTimeout(300);
dialogResponse = 'https://ctx.example';
await page.locator('.context-menu .context-item', { hasText: /^Link$/ }).first().click();
await page.waitForTimeout(600);
const linkDoc = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
check('context-menu link wraps rendered selection',
  linkDoc.includes('[Trailing](https://ctx.example)'),
  linkDoc.slice(-160).replace(/\n/g, '|'));

// ── 53. Emptying a block commits a block deletion ────────────────────────
await setStep("53. Empty block commit");
await page.locator('.md-block', { hasText: 'cyrillic' }).first().click();
const taDel = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('cyrillic'),
  null, { timeout: 5000 });
await taDel.fill('');
await page.keyboard.press('Escape');
await page.waitForTimeout(600);
const delDoc = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
check('empty block commit removes the block', !delDoc.includes('cyrillic'),
  delDoc.slice(-200).replace(/\n/g, '|'));

// ── 54. Context-menu Table inserts into the editing textarea ─────────────
await setStep("54. Context Table during edit");
await page.locator('.md-block', { hasText: 'quoted line' }).first().click();
const taTbl = page.locator('textarea.md-block-textarea');
await page.waitForFunction(
  () => document.querySelector('textarea.md-block-textarea')?.value.includes('quoted'),
  null, { timeout: 5000 });
await taTbl.click({ button: 'right' });
await page.waitForTimeout(300);
await page.locator('.context-menu .context-item', { hasText: /^Table$/ }).first().click();
await page.waitForTimeout(400);
const tblVal = await taTbl.inputValue();
check('context-menu table inserts markdown table',
  tblVal.includes('|') && /\|\s*-{2,}/.test(tblVal), JSON.stringify(tblVal.slice(0, 90)));
// ── 55. Context-menu Image inserts a data-url image ──────────────────────
await setStep("55. Context Image during edit");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/pic.png'; });
await taTbl.click({ button: 'right' });
await page.waitForTimeout(300);
await page.locator('.context-menu .context-item', { hasText: /^Image$/ }).first().click();
await page.waitForTimeout(600);
const imgVal = await taTbl.inputValue();
check('context-menu image inserts ![alt](data:url)',
  imgVal.includes('![alt text](data:image/png;base64,'),
  JSON.stringify(imgVal.slice(-100)));
await page.keyboard.press('Escape');
await page.waitForTimeout(500);
const imgDoc = await page.evaluate(() => new TextDecoder().decode(
  window.__tauriMock.state.tabs[window.__tauriMock.state.active].bytes));
check('image+table edit committed', imgDoc.includes('data:image/png;base64,'),
  imgDoc.slice(-160).replace(/\n/g, '|'));

// ── 56. Scroll anchoring: a resize above the viewport must not jerk ──────
await setStep("56. Scroll anchoring");
await page.evaluate(() => { window.__mockDialogOpen = '/repo/big.md'; });
await page.locator('#btn-open').click();
// Chunked parsing feeds blocks incrementally — wait for the FULL count
// (mock knows it synchronously) before scrolling mid-document.
await page.waitForFunction(() => {
  const m = window.__tauriMock;
  const total = m.docInfo(m.state.tabs[m.state.active]).block_count;
  const shown = parseInt(document.querySelector('#block-count')?.textContent || '0');
  return total > 0 && shown >= total;
}, null, { timeout: 15000 });
// Park the viewport mid-document (as a fraction of the real sizer height —
// estimated offsets, not a fixed px). The post-load scroll restore can land
// late and reset scrollTop to 0 — retry the assignment until it sticks.
for (let attempt = 0; attempt < 5; attempt++) {
  await page.evaluate(() => {
    const vp = document.querySelector('#block-editor');
    vp.scrollTop = Math.floor(vp.scrollHeight * 0.4);
  });
  try {
    await page.waitForFunction(
      () => document.querySelector('#block-editor').scrollTop > 1000,
      null, { timeout: 1200 });
    break;
  } catch { /* restore raced us — retry */ }
}
await page.waitForTimeout(900);
const anchorInfo = await page.evaluate(() => {
  const vp = document.querySelector('#block-editor');
  const vpTop = vp.getBoundingClientRect().top;
  const els = [...document.querySelectorAll('.md-block')];
  const top = els.find((el) => el.getBoundingClientRect().bottom > vpTop);
  // A rendered block ABOVE the viewport (inside overscan) to grow.
  const above = [...els].reverse().find((el) => el.getBoundingClientRect().bottom <= vpTop);
  return {
    scroll: vp.scrollTop,
    topIdx: top ? Number(top.dataset.blockIndex) : -1,
    topText: top ? top.textContent.slice(0, 40) : '',
    aboveIdx: above ? Number(above.dataset.blockIndex) : -1,
  };
});
check('anchor probe: rendered blocks exist above viewport',
  anchorInfo.aboveIdx >= 0 && anchorInfo.topIdx > anchorInfo.aboveIdx,
  JSON.stringify(anchorInfo));
const GROW = 320;
await page.evaluate((g) => {
  const vp = document.querySelector('#block-editor');
  const vpTop = vp.getBoundingClientRect().top;
  const els = [...document.querySelectorAll('.md-block')];
  const above = [...els].reverse().find((el) => el.getBoundingClientRect().bottom <= vpTop);
  if (above) {
    // A real child — ResizeObserver reports content-box height, so padding
    // tricks don't count; an in-flow child grows the box like real content.
    const filler = document.createElement('div');
    filler.style.height = `${g}px`;
    above.appendChild(filler);
  }
}, GROW);
await page.waitForTimeout(700); // ResizeObserver -> rebuildOffsets -> anchor shift
const afterGrow = await page.evaluate(() => {
  const vp = document.querySelector('#block-editor');
  const vpTop = vp.getBoundingClientRect().top;
  const els = [...document.querySelectorAll('.md-block')];
  const top = els.find((el) => el.getBoundingClientRect().bottom > vpTop);
  return {
    scroll: vp.scrollTop,
    topIdx: top ? Number(top.dataset.blockIndex) : -1,
    topText: top ? top.textContent.slice(0, 40) : '',
  };
});
// The grown block pushed everything below by ~GROW px; scrollTop must be
// compensated so the SAME block stays at the viewport top (no visual jump).
check('resize above viewport compensates scrollTop',
  Math.abs(afterGrow.scroll - (anchorInfo.scroll + GROW)) < 40,
  `${anchorInfo.scroll} -> ${afterGrow.scroll} (expected ~${anchorInfo.scroll + GROW})`);
check('same block stays at viewport top (no jump)',
  afterGrow.topIdx === anchorInfo.topIdx && afterGrow.topText === anchorInfo.topText,
  `${anchorInfo.topIdx}:"${anchorInfo.topText}" -> ${afterGrow.topIdx}:"${afterGrow.topText}"`);
// A resize BELOW the viewport must NOT move scrollTop.
const belowInfo = await page.evaluate(() => {
  const vp = document.querySelector('#block-editor');
  const vpBottom = vp.getBoundingClientRect().bottom;
  const els = [...document.querySelectorAll('.md-block')];
  const below = els.find((el) => el.getBoundingClientRect().top > vpBottom);
  if (below) {
    const filler = document.createElement('div');
    filler.style.height = '200px';
    below.appendChild(filler);
  }
  return { belowIdx: below ? Number(below.dataset.blockIndex) : -1, scroll: vp.scrollTop };
});
await page.waitForTimeout(700);
const scrollAfterBelow = await page.evaluate(
  () => document.querySelector('#block-editor').scrollTop);
check('resize below viewport leaves scrollTop alone',
  belowInfo.belowIdx >= 0 && scrollAfterBelow === belowInfo.scroll,
  `below=${belowInfo.belowIdx} ${belowInfo.scroll} -> ${scrollAfterBelow}`);

// ── Summary ─────────────────────────────────────────────────────────────
console.log('\n──── console errors ────');
for (const e of errors) console.log('  ' + e.slice(0, 300));
if (!errors.length) console.log('  (none)');
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed, ${errors.length} console errors`);
await browser.close();
process.exit(failed.length || errors.length ? 1 : 0);
