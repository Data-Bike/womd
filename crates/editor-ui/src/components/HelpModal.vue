<template>
  <div class="modal-overlay" :class="{ hidden: !open }" @click.self="close">
    <div class="modal-dialog" style="width: 640px;">
      <div class="modal-header">
        <span class="modal-title">Help</span>
        <button class="modal-close" @click="close">×</button>
      </div>
      <div class="modal-body help-body">
        <section>
          <h3>Keyboard shortcuts</h3>
          <ul class="help-list">
            <li><code>Ctrl+N</code> — New document</li>
            <li><code>Ctrl+O</code> — Open file</li>
            <li><code>Ctrl+S</code> — Save</li>
            <li><code>Ctrl+Shift+S</code> — Save As</li>
            <li><code>Ctrl+Z</code> — Undo</li>
            <li><code>Ctrl+Y</code> / <code>Ctrl+Shift+Z</code> — Redo</li>
            <li><code>Ctrl+X</code> — Cut</li>
            <li><code>Ctrl+C</code> — Copy</li>
            <li><code>Ctrl+V</code> — Paste</li>
            <li><code>Ctrl+A</code> — Select All</li>
            <li><code>Ctrl+F</code> — Find</li>
            <li><code>Ctrl+H</code> — Find and Replace</li>
            <li><code>F3</code> — Find next</li>
            <li><code>Shift+F3</code> — Find previous</li>
            <li><code>Ctrl+B</code> — Bold</li>
            <li><code>Ctrl+I</code> — Italic</li>
          </ul>
        </section>

        <section>
          <h3>Mathematical formulas</h3>
          <p>Mathematical expressions are rendered with <a href="https://katex.org/" target="_blank" rel="noopener">KaTeX</a>.</p>
          <ul class="help-list">
            <li><code>\(...\)</code> for inline math, e.g. <code>\( E = mc^2 \)</code></li>
            <li><code>\[...\]</code> for display math on its own line, e.g. <code>\[ x = \frac{-b \pm \sqrt{b^2-4ac}}{2a} \]</code></li>
          </ul>
          <p>
            A paragraph that contains only a display-math expression is rendered as a centered block formula.
            Invalid LaTeX is shown as plain text with an error tooltip.
          </p>
        </section>

        <section>
          <h3>Working with the editor</h3>
          <p>
            WoMD switches between a rendered block view and a raw Markdown source view.
            Click any block in rendered view to edit it directly, or switch to source view to edit the full file.
            Images can be inserted from the Insert menu or dragged into the editor; they are embedded as base64 data URLs.
          </p>
        </section>
      </div>
    </div>
  </div>
</template>

<script setup>
const props = defineProps({ open: Boolean });
const emit = defineEmits(['update:open']);

function close() {
  emit('update:open', false);
}
</script>

<style scoped>
.help-body section {
  margin-bottom: 16px;
}
.help-body h3 {
  margin: 0 0 8px;
  font-size: 14px;
  color: var(--fg);
}
.help-body p {
  margin: 0 0 8px;
  line-height: 1.5;
  color: var(--fg);
}
.help-list {
  list-style: none;
  padding: 0;
  margin: 0 0 12px;
}
.help-list li {
  padding: 3px 0;
  color: var(--fg);
  font-size: 13px;
}
.help-list li code {
  display: inline-block;
  min-width: 100px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  color: var(--accent);
  background: var(--bg-tab, rgba(137,180,250,0.08));
  padding: 1px 6px;
  border-radius: 4px;
}
.help-body a {
  color: var(--accent);
}
</style>
