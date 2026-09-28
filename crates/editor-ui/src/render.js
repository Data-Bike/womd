// AST-based Markdown rendering, ported from the legacy vanilla frontend.

import katex from 'katex';

export function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function escapeAttr(s) {
  // `&` must be escaped: the browser entity-decodes attribute values AFTER
  // our URL sanitization, so `javascript&colon;alert(1)` would otherwise
  // re-assemble into `javascript:alert(1)` and execute. Escaping `&` makes
  // the decoded value identical to the string we checked.
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// Sanitize link/image destinations: a Markdown file can carry
// `javascript:`/`vbscript:`/`data:` URLs that HTML-escaping does NOT make safe
// (the scheme still executes when the link is activated). Control characters
// and whitespace can smuggle the scheme past a naive prefix check
// ("java\tscript:"), so strip them before probing.
function sanitizeUrl(url, { allowDataImage = false } = {}) {
  const raw = String(url);
  const probe = raw.replace(/[\u0000-\u0020]+/g, "").toLowerCase();
  if (probe.startsWith("javascript:") || probe.startsWith("vbscript:")) {
    return "#";
  }
  if (probe.startsWith("data:")) {
    return allowDataImage && probe.startsWith("data:image/") ? raw : "#";
  }
  return raw;
}

export function safeHref(url) {
  return sanitizeUrl(url);
}

export function safeSrc(url) {
  return sanitizeUrl(url, { allowDataImage: true });
}

export function renderBlockHtml(block) {
  if (!block) return '';
  if (block.node) return renderAstNode(block.node);
  if (block.source) return `<p>${escapeHtml(block.source)}</p>`;
  return `<div class="md-block-placeholder" style="padding:8px;color:var(--fg-muted)">…</div>`;
}

function renderMath(content, display) {
  try {
    return katex.renderToString(content, { displayMode: display, throwOnError: false });
  } catch (e) {
    return `<span class="math-error" title="${escapeHtml(String(e))}">${escapeHtml(content)}</span>`;
  }
}

export function renderAstNode(node) {
  if (!node) return "";
  switch (node.type) {
    case "Heading": {
      // `v-html` sink: numeric fields come from the backend AST, but clamp
      // anyway — a tampered IPC payload must not inject attribute/markup.
      const level = Math.min(6, Math.max(1, Math.trunc(Number(node.level) || 1)));
      return `<h${level}>${renderAstChildren(node.children)}</h${level}>`;
    }
    case "Paragraph":
      // A single display-math paragraph is rendered as a block-level formula,
      // not wrapped in a <p>, to keep KaTeX's display output valid.
      if (
        node.children &&
        node.children.length === 1 &&
        node.children[0].type === "Math" &&
        node.children[0].display
      ) {
        return renderAstNode(node.children[0]);
      }
      return `<p>${renderAstChildren(node.children)}</p>`;
    case "ThematicBreak":
      return "<hr/>";
    case "BlockQuote":
      return `<blockquote>${renderAstChildren(node.children)}</blockquote>`;
    case "List":
      return renderAstList(node);
    case "CodeBlock":
      return `<pre><code${node.language ? ` class="language-${escapeAttr(node.language)}"` : ""}>${escapeHtml(node.content)}</code></pre>`;
    case "Table":
      return renderAstTable(node);
    case "HtmlBlock":
      return `<pre class="md-html-block">${escapeHtml(node.content)}</pre>`;
    case "LinkRefDef":
      return `<div style="display:none"></div>`;
    case "BlankLine":
      return "";
    case "Math":
      return renderMath(node.content, node.display);
    case "Text":
      return escapeHtml(node.text);
    case "Emphasis":
      return `<em>${renderAstChildren(node.children)}</em>`;
    case "Strong":
      return `<strong>${renderAstChildren(node.children)}</strong>`;
    case "Strikethrough":
      return `<del>${renderAstChildren(node.children)}</del>`;
    case "CodeSpan":
      return `<code>${escapeHtml(node.text)}</code>`;
    case "Link":
      return `<a href="${escapeAttr(safeHref(node.destination))}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""}>${renderAstChildren(node.children)}</a>`;
    case "Image":
      return `<img alt="${escapeAttr(node.alt)}" src="${escapeAttr(safeSrc(node.destination))}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""} />`;
    case "Autolink":
      return `<a href="${escapeAttr(safeHref(node.url))}">${escapeHtml(node.url)}</a>`;
    case "HardBreak":
      return "<br/>";
    case "RawHtml":
      return escapeHtml(node.content);
    default:
      return "";
  }
}

export function renderAstChildren(children) {
  if (!children) return "";
  return children.map(c => renderAstNode(c)).join("");
}

export function renderAstList(node) {
  const tag = node.ordered ? "ol" : "ul";
  const start = Math.trunc(Number(node.start) || 1);
  let html = `<${tag}${node.ordered && start !== 1 ? ` start="${start}"` : ""}>`;
  for (const item of node.items) {
    if (item.task) {
      const checked = item.task === "done";
      const content = renderAstChildren(item.children);
      html += `<li class="task-item"><input type="checkbox" ${checked ? "checked" : ""} disabled /><span>${content}</span></li>`;
    } else {
      const content = item.children.map(c => renderAstNode(c)).join("");
      html += `<li>${content}</li>`;
    }
  }
  html += `</${tag}>`;
  return html;
}

// Only these CSS values may reach `style="text-align:…"` — the AST alignment
// string is interpolated unescaped, so anything else is dropped to "none".
function safeAlign(a) {
  return a === "left" || a === "right" || a === "center" ? a : "none";
}

export function renderAstTable(node) {
  let html = "<table><thead><tr>";
  for (let i = 0; i < node.header.length; i++) {
    const cell = node.header[i];
    const align = safeAlign(node.alignments[i]);
    const style = align !== "none" ? ` style="text-align:${align}"` : "";
    html += `<th${style}>${renderAstChildren(cell.children)}</th>`;
  }
  html += "</tr></thead><tbody>";
  for (const row of node.rows) {
    html += "<tr>";
    for (let i = 0; i < row.length; i++) {
      const cell = row[i];
      const align = safeAlign(node.alignments[i]);
      const style = align !== "none" ? ` style="text-align:${align}"` : "";
      html += `<td${style}>${renderAstChildren(cell.children)}</td>`;
    }
    html += "</tr>";
  }
  html += "</tbody></table>";
  return html;
}
