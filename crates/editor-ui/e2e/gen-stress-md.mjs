// Generates a ~100 MiB Markdown stress document covering every construct the
// editor renders: ATX/setext headings, nested/ordered/task lists, block
// quotes (incl. nested + containing code), fenced/indented code, GFM tables,
// inline formatting, links/images/autolinks, link reference definitions,
// math blocks, HTML blocks, thematic breaks, hard breaks, unicode
// (Cyrillic, CJK, emoji, Arabic), long lines, and a trailing CRLF section.
//
// Usage: node e2e/gen-stress-md.mjs [outFile] [targetMiB]
//   defaults: test-data/womd-stress-100mb.md at repo root, 100 MiB
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = process.argv[2]
  ? path.resolve(process.argv[2])
  : path.join(repoRoot, 'test-data', 'womd-stress-100mb.md');
const TARGET = (parseFloat(process.argv[3] || '100')) * 1024 * 1024;
fs.mkdirSync(path.dirname(OUT), { recursive: true });

// Deterministic PRNG (xorshift32) so runs are reproducible.
let seed = 0x9e3779b9;
const rnd = () => {
  seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5;
  return (seed >>> 0) / 4294967296;
};
const pick = (arr) => arr[Math.floor(rnd() * arr.length)];

const WORDS = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf',
  'hotel', 'india', 'juliet', 'kilo', 'lima', 'mike', 'november', 'oscar'];
const LOREM = 'Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.';
const CYR = 'Съешь же ещё этих мягких французских булок, да выпей чаю.';
const CJK = 'markdown 编辑器 测试 中文 段落 内容';
const AR = 'نص عربي تجريبي للاتجاه من اليمين إلى اليسار';

function para(i, extra = '') {
  const w = `${pick(WORDS)}-${i}`;
  return `Paragraph ${w} ${extra}${LOREM} **bold-${w}** and *italic-${w}* and ` +
    `\`code-${w}\` and ~~strike-${w}~~ and a [link-${w}](https://example.com/${w}) ` +
    `plus an autolink <https://autolink.example/${w}> and a [ref link ${w}][ref-${w}].`;
}

function chapter(i) {
  const s = [];
  s.push(`\n<!-- chapter ${i} -->\n`);
  s.push(`# Chapter ${i} — ${pick(WORDS)}\n`);
  s.push(`Setext heading ${i}\n====\n`);
  s.push(`Setext two ${i}\n---\n`);
  for (let lvl = 2; lvl <= 6; lvl++) s.push(`${'#'.repeat(lvl)} Sub ${i}.${lvl}\n`);
  s.push(para(i) + '\n');
  s.push(para(i, 'long-tail ') + ' '.repeat(200) + 'trailing-spaces-ok  \n'); // trailing spaces = hard break
  // Long single line (~1 KiB) for horizontal-scrolling / wrap testing.
  s.push(`LONGLINE ${i}: ` + 'x'.repeat(900) + ` END-${i}\n`);
  // Unordered nested list
  s.push(`- item ${i}a\n- item ${i}b\n  - nested ${i}b1\n  - nested ${i}b2\n    - deep ${i}b2x\n- item ${i}c\n`);
  // Ordered list with non-1 start
  s.push(`5. ord ${i}a\n6. ord ${i}b\n9. ord ${i}c\n`);
  // Task list mixed
  s.push(`- [ ] open task ${i}\n- [x] done task ${i}\n- [ ] another open ${i}\n`);
  // Blockquote, nested, containing a list + code
  s.push(`> Quote ${i} line one\n>\n> > nested quote ${i}\n> - list-in-quote ${i}\n>\n> \`\`\`\n> code in quote ${i}\n> \`\`\`\n`);
  // Fenced code with languages + a fence containing inner backticks
  s.push('```rust\n' + `fn chapter_${i}() { println!("hello ${i}"); }\n` + '```\n');
  s.push('```python\n' + `def chapter_${i}():\n    return ${i}\n` + '```\n');
  s.push('~~~js\n' + `const v${i} = "${pick(WORDS)}";\n` + '~~~\n');
  s.push('````\n' + `code containing \`\`\` inside ${i}\n` + '````\n');
  // Indented code
  s.push(`    indented code ${i}\n    second line ${i}\n`);
  // Tables
  s.push(`| Col A ${i} | Col B ${i} | Col C ${i} |\n|:--|:--:|--:|\n| a${i} | b${i} | c${i} |\n| d${i} | e${i} | f${i} |\n`);
  s.push(`|x${i}|y${i}|\n|-|-|\n|1|2|\n`);
  // Math
  s.push(`$$E_${i} = m c^2 + \\frac{${i}}{n}$$\n`);
  s.push(`Inline math $x_{i}^{2} + y = ${i}$ inside a sentence.\n`);
  // Inline-heavy paragraph
  s.push(`***bold-italic ${i}*** __underline__ \`inline\` **[b](https://b.example/${i})** ![img ${i}](img-${i}.png "title ${i}")\n`);
  // HTML block
  s.push(`<div class="html-${i}">\n  <p>html block ${i} <b>bold html</b></p>\n</div>\n`);
  s.push(`<table><tr><td>raw table ${i}</td></tr></table>\n`);
  // Thematic breaks, all spellings
  s.push('---\n***\n___\n');
  // Link reference definition + usage
  s.push(`[ref-${i}]: https://ref.example/${i} "Ref title ${i}"\n`);
  s.push(`Reference used: [ref link ${i}] and shortcut [ref-${i}].\n`);
  // Unicode soup
  s.push(`${CYR} — ${i} — ${CJK} — ${AR} — emoji 🎉🚀✨ — math ∀∃∑∏ — #${i}\n`);
  // Definition-ish / keyboard / misc inline
  s.push(`Kbd <kbd>Ctrl</kbd>+<kbd>S</kbd>, footnote ref [^f${i}], escaped \\*not-em\\* ${i}.\n`);
  s.push(`[^f${i}]: footnote body for chapter ${i}\n`);
  return s.join('\n');
}

// Final section intentionally uses CRLF endings to exercise mixed-EOL handling.
const CRLF_SECTION = '\r\n# CRLF SECTION\r\n\r\n- crlf item 1\r\n- crlf item 2\r\n\r\n> crlf quote\r\n';

const t0 = Date.now();
const ws = fs.createWriteStream(OUT, { highWaterMark: 1 << 20 });
ws.write(`# WoMD stress document\n\nGenerated ${new Date().toISOString()} — deterministic xorshift seed.\n`);

let written = 0, chapters = 0;
const count = (s) => Buffer.byteLength(s, 'utf8');
ws.on('error', (e) => { console.error(e); process.exit(1); });

while (written < TARGET) {
  const chunk = chapter(chapters);
  written += count(chunk);
  chapters++;
  if (!ws.write(chunk)) await new Promise((r) => ws.once('drain', r));
}
ws.write(CRLF_SECTION);
written += count(CRLF_SECTION);
await new Promise((r) => ws.end(r));

const st = fs.statSync(OUT);
console.log(`wrote ${OUT}`);
console.log(`size: ${st.size} bytes (${(st.size / 1048576).toFixed(1)} MiB)`);
console.log(`chapters: ${chapters} — in ${((Date.now() - t0) / 1000).toFixed(1)}s`);
