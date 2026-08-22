<template>
  <div
    v-show="show"
    class="context-menu"
    :style="{ top: `${clampedY}px`, left: `${clampedX}px` }"
    @click.stop
  >
    <div
      v-for="(item, idx) in items"
      :key="idx"
      class="context-item"
      :class="{ separator: !item, disabled: item && item.disabled }"
      @click="onClick(item)"
    >
      <template v-if="item">
        <span class="context-item-label">{{ item.label }}</span>
        <span v-if="item.shortcut" class="context-shortcut">{{ item.shortcut }}</span>
      </template>
    </div>
  </div>
</template>

<script setup>
import { ref, watch, computed } from 'vue';

const props = defineProps({
  show: Boolean,
  x: { type: Number, default: 0 },
  y: { type: Number, default: 0 },
});

const emit = defineEmits([
  'close',
  'undo', 'redo', 'cut', 'copy', 'paste', 'select-all',
  'bold', 'italic', 'strikethrough', 'code', 'link',
  'heading', 'unordered-list', 'ordered-list', 'task-list', 'quote', 'hr', 'code-block',
  'table', 'image',
  'view-rendered', 'view-source', 'toggle-tree', 'toggle-git', 'settings'
]);

const items = [
  { label: 'Undo', action: () => emit('undo'), shortcut: 'Ctrl+Z' },
  { label: 'Redo', action: () => emit('redo'), shortcut: 'Ctrl+Y' },
  null,
  { label: 'Cut', action: () => emit('cut'), shortcut: 'Ctrl+X' },
  { label: 'Copy', action: () => emit('copy'), shortcut: 'Ctrl+C' },
  { label: 'Paste', action: () => emit('paste'), shortcut: 'Ctrl+V' },
  { label: 'Select All', action: () => emit('select-all'), shortcut: 'Ctrl+A' },
  null,
  { label: 'Bold', action: () => emit('bold'), shortcut: 'Ctrl+B' },
  { label: 'Italic', action: () => emit('italic'), shortcut: 'Ctrl+I' },
  { label: 'Strikethrough', action: () => emit('strikethrough') },
  { label: 'Inline Code', action: () => emit('code') },
  { label: 'Link', action: () => emit('link') },
  { label: 'Image', action: () => emit('image') },
  { label: 'Table', action: () => emit('table') },
  null,
  { label: 'Heading 1', action: () => emit('heading', 1) },
  { label: 'Heading 2', action: () => emit('heading', 2) },
  { label: 'Heading 3', action: () => emit('heading', 3) },
  null,
  { label: 'Unordered List', action: () => emit('unordered-list') },
  { label: 'Ordered List', action: () => emit('ordered-list') },
  { label: 'Task List', action: () => emit('task-list') },
  { label: 'Quote', action: () => emit('quote') },
  { label: 'Thematic Break', action: () => emit('hr') },
  { label: 'Code Block', action: () => emit('code-block') },
  null,
  { label: 'Rendered', action: () => emit('view-rendered') },
  { label: 'Source (Markdown)', action: () => emit('view-source') },
  null,
  { label: 'Toggle File Tree', action: () => emit('toggle-tree') },
  { label: 'Toggle Git Panel', action: () => emit('toggle-git') },
  { label: 'Settings', action: () => emit('settings') },
];

function onClick(item) {
  if (!item || item.disabled) return;
  emit('close');
  item.action();
}

// Clamp the menu inside the viewport so a right-click near the bottom/right
// edge doesn't render (partially) off-screen.
const menuWidth = 200;
const menuHeightEstimate = 480;
const clampedX = computed(() => Math.min(props.x, Math.max(0, window.innerWidth - menuWidth)));
const clampedY = computed(() => Math.min(props.y, Math.max(0, window.innerHeight - menuHeightEstimate)));

watch(() => props.show, (v) => {
  if (v) {
    const close = () => emit('close');
    window.addEventListener('click', close, { once: true });
    window.addEventListener('contextmenu', close, { once: true });
    window.addEventListener('scroll', close, { once: true });
  }
});
</script>

<style scoped>
.context-menu {
  position: fixed;
  min-width: 180px;
  background: var(--surface-0, #1e1e2e);
  border: 1px solid var(--surface-1, #313244);
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  z-index: 2000;
  padding: 4px 0;
  font: 13px system-ui, -apple-system, sans-serif;
  user-select: none;
}

.context-item {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 5px 14px 5px 12px;
  cursor: default;
  color: var(--fg, #cdd6f4);
}

.context-item:hover:not(.disabled):not(.separator) {
  background: var(--accent, #89b4fa);
  color: var(--surface-0, #1e1e2e);
}

.context-item.disabled {
  opacity: 0.4;
}

.context-item.separator {
  height: 1px;
  background: var(--surface-1, #313244);
  margin: 4px 8px;
  padding: 0;
}

.context-shortcut {
  color: var(--fg-muted, #a6adc8);
  font-size: 12px;
  margin-left: 20px;
}
</style>
