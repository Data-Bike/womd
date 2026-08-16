// AST-based Markdown rendering, ported from the legacy vanilla frontend.

export function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function escapeAttr(s) {
  return String(s).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

export function renderBlockHtml(block) {
  if (!block) return '';
  if (block.node) return renderAstNode(block.node);
  if (block.source) return `<p>${escapeHtml(block.source)}</p>`;
  return `<div class="md-block-placeholder" style="padding:8px;color:var(--fg-muted)">…</div>`;
}

export function renderAstNode(node) {
  if (!node) return "";
  switch (node.type) {
    case "Heading":
      return `<h${node.level}>${renderAstChildren(node.children)}</h${node.level}>`;
    case "Paragraph":
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
      return `<a href="${escapeAttr(node.destination)}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""}>${renderAstChildren(node.children)}</a>`;
    case "Image":
      return `<img alt="${escapeAttr(node.alt)}" src="${escapeAttr(node.destination)}"${node.title ? ` title="${escapeAttr(node.title)}"` : ""} />`;
    case "Autolink":
      return `<a href="${escapeAttr(node.url)}">${escapeHtml(node.url)}</a>`;
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
  let html = `<${tag}${node.ordered && node.start !== 1 ? ` start="${node.start}"` : ""}>`;
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

export function renderAstTable(node) {
  let html = "<table><thead><tr>";
  for (let i = 0; i < node.header.length; i++) {
    const cell = node.header[i];
    const align = node.alignments[i] || "none";
    const style = align !== "none" ? ` style="text-align:${align}"` : "";
    html += `<th${style}>${renderAstChildren(cell.children)}</th>`;
  }
  html += "</tr></thead><tbody>";
  for (const row of node.rows) {
    html += "<tr>";
    for (let i = 0; i < row.length; i++) {
      const cell = row[i];
      const align = node.alignments[i] || "none";
      const style = align !== "none" ? ` style="text-align:${align}"` : "";
      html += `<td${style}>${renderAstChildren(cell.children)}</td>`;
    }
    html += "</tr>";
  }
  html += "</tbody></table>";
  return html;
}
