<template>
  <div class="menu-bar" @mouseleave="active = null" @mousedown.prevent>
    <div
      v-for="menu in menus"
      :key="menu.label"
      class="menu-top"
      :class="{ open: active === menu.label }"
      @mouseenter="onHover(menu)"
      @click="active = active === menu.label ? null : menu.label"
    >
      <span class="menu-label">{{ menu.label }}</span>
      <div v-if="active === menu.label" class="menu-dropdown" @click.stop>
        <div
          v-for="(item, idx) in menu.items"
          :key="idx"
          class="menu-item"
          :class="{ separator: !item, disabled: item && item.disabled }"
          @click="onClick(item)"
        >
          <template v-if="item">
            <span class="menu-item-label">{{ item.label }}</span>
            <span v-if="item.shortcut" class="menu-shortcut">{{ item.shortcut }}</span>
          </template>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref } from 'vue';

const emit = defineEmits([
  'new', 'open', 'save', 'save-as', 'close-window',
  'undo', 'redo', 'cut', 'copy', 'paste', 'select-all', 'find', 'find-next', 'find-replace',
  'view-rendered', 'view-source', 'toggle-tree', 'toggle-git',
  'heading', 'bold', 'italic', 'strikethrough', 'code', 'link',
  'unordered-list', 'ordered-list', 'task-list', 'quote', 'hr', 'code-block', 'table', 'image',
  'settings', 'about', 'help'
]);

const active = ref(null);

// Standard menubar behavior: hover switches menus only while one is open.
// An unconditional hover-open made every pass over the bar flash dropdowns
// and made click-to-open impossible (hover pre-opened → click toggled off).
function onHover(menu) {
  if (active.value && active.value !== menu.label) active.value = menu.label;
}

const menus = [
  {
    label: 'File',
    items: [
      { label: 'New', action: () => emit('new'), shortcut: 'Ctrl+N' },
      { label: 'Open...', action: () => emit('open'), shortcut: 'Ctrl+O' },
      { label: 'Save', action: () => emit('save'), shortcut: 'Ctrl+S' },
      { label: 'Save As...', action: () => emit('save-as') },
      null,
      { label: 'Exit', action: () => emit('close-window') },
    ]
  },
  {
    label: 'Edit',
    items: [
      { label: 'Undo', action: () => emit('undo'), shortcut: 'Ctrl+Z' },
      { label: 'Redo', action: () => emit('redo'), shortcut: 'Ctrl+Y' },
      null,
      { label: 'Cut', action: () => emit('cut'), shortcut: 'Ctrl+X' },
      { label: 'Copy', action: () => emit('copy'), shortcut: 'Ctrl+C' },
      { label: 'Paste', action: () => emit('paste'), shortcut: 'Ctrl+V' },
      { label: 'Select All', action: () => emit('select-all'), shortcut: 'Ctrl+A' },
      null,
      { label: 'Find...', action: () => emit('find'), shortcut: 'Ctrl+F' },
      { label: 'Find Next', action: () => emit('find-next'), shortcut: 'F3' },
      { label: 'Find and Replace...', action: () => emit('find-replace'), shortcut: 'Ctrl+H' },
    ]
  },
  {
    label: 'View',
    items: [
      { label: 'Rendered', action: () => emit('view-rendered') },
      { label: 'Source (Markdown)', action: () => emit('view-source') },
      null,
      { label: 'File Tree', action: () => emit('toggle-tree') },
      { label: 'Git Panel', action: () => emit('toggle-git') },
    ]
  },
  {
    label: 'Insert',
    items: [
      { label: 'Heading 1', action: () => emit('heading', 1) },
      { label: 'Heading 2', action: () => emit('heading', 2) },
      { label: 'Heading 3', action: () => emit('heading', 3) },
      { label: 'Heading 4', action: () => emit('heading', 4) },
      { label: 'Heading 5', action: () => emit('heading', 5) },
      { label: 'Heading 6', action: () => emit('heading', 6) },
      null,
      { label: 'Bold', action: () => emit('bold') },
      { label: 'Italic', action: () => emit('italic') },
      { label: 'Strikethrough', action: () => emit('strikethrough') },
      { label: 'Inline Code', action: () => emit('code') },
      { label: 'Link', action: () => emit('link') },
      null,
      { label: 'Unordered List', action: () => emit('unordered-list') },
      { label: 'Ordered List', action: () => emit('ordered-list') },
      { label: 'Task List', action: () => emit('task-list') },
      { label: 'Quote', action: () => emit('quote') },
      { label: 'Thematic Break', action: () => emit('hr') },
      { label: 'Code Block', action: () => emit('code-block') },
      { label: 'Table', action: () => emit('table') },
      { label: 'Image', action: () => emit('image') },
    ]
  },
  {
    label: 'Format',
    items: [
      { label: 'Bold', action: () => emit('bold'), shortcut: 'Ctrl+B' },
      { label: 'Italic', action: () => emit('italic'), shortcut: 'Ctrl+I' },
      { label: 'Strikethrough', action: () => emit('strikethrough') },
      { label: 'Inline Code', action: () => emit('code') },
      { label: 'Link', action: () => emit('link') },
      null,
      { label: 'Heading 1', action: () => emit('heading', 1) },
      { label: 'Heading 2', action: () => emit('heading', 2) },
      { label: 'Heading 3', action: () => emit('heading', 3) },
      null,
      { label: 'Unordered List', action: () => emit('unordered-list') },
      { label: 'Ordered List', action: () => emit('ordered-list') },
      { label: 'Task List', action: () => emit('task-list') },
      { label: 'Quote', action: () => emit('quote') },
    ]
  },
  {
    label: 'Tools',
    items: [
      { label: 'Settings', action: () => emit('settings') },
      null,
      { label: 'Toggle File Tree', action: () => emit('toggle-tree') },
      { label: 'Toggle Git Panel', action: () => emit('toggle-git') },
    ]
  },
  {
    label: 'Help',
    items: [
      { label: 'Help...', action: () => emit('help'), shortcut: 'F1' },
      null,
      { label: 'About WoMD', action: () => emit('about') },
    ]
  },
];

function onClick(item) {
  if (!item || item.disabled) return;
  active.value = null;
  item.action();
}
</script>

<style scoped>
.menu-bar {
  display: flex;
  align-items: center;
  background: var(--surface-0, #1e1e2e);
  border-bottom: 1px solid var(--surface-1, #313244);
  padding: 0 6px;
  font: 13px system-ui, -apple-system, sans-serif;
  user-select: none;
}

.menu-top {
  position: relative;
  padding: 4px 10px;
  cursor: default;
  color: var(--fg, #cdd6f4);
  border: 1px solid transparent;
}

.menu-top:hover,
.menu-top.open {
  background: var(--surface-1, #313244);
  border-color: var(--surface-2, #45475a);
}

.menu-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  min-width: 220px;
  background: var(--surface-0, #1e1e2e);
  border: 1px solid var(--surface-1, #313244);
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  z-index: 1000;
  padding: 4px 0;
}

.menu-item {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 5px 16px 5px 12px;
  cursor: default;
  color: var(--fg, #cdd6f4);
}

.menu-item:hover:not(.disabled):not(.separator) {
  background: var(--accent, #89b4fa);
  color: var(--surface-0, #1e1e2e);
}

.menu-item.disabled {
  opacity: 0.4;
}

.menu-item.separator {
  height: 1px;
  background: var(--surface-1, #313244);
  margin: 4px 8px;
  padding: 0;
}

.menu-shortcut {
  color: var(--fg-muted, #a6adc8);
  font-size: 12px;
  margin-left: 24px;
}
</style>
