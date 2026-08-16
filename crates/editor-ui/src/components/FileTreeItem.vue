<template>
  <div>
    <div
      class="file-tree-item"
      :class="{ folder: entry.is_dir, file: !entry.is_dir, expanded: entry.expanded }"
      :style="{ paddingLeft: `${8 + depth * 16}px` }"
      @click="onClick"
    >
      <span v-if="entry.is_dir" class="ft-chevron" @click.stop="toggleExpand">{{ entry.expanded ? 'v' : '>' }}</span>
      <span v-else class="ft-chevron"></span>
      <span class="ft-icon">{{ entry.is_dir ? '[D]' : '[F]' }}</span>
      <span class="ft-name">{{ entry.name }}</span>
    </div>
    <div v-if="entry.is_dir && entry.expanded" class="file-tree-children">
      <div v-if="!entry.children" class="file-tree-empty">Loading...</div>
      <div v-else-if="entry.children.length === 0" class="file-tree-empty">Empty</div>
      <FileTreeItem
        v-for="child in entry.children"
        :key="child.path"
        :entry="child"
        :depth="depth + 1"
        @open="$emit('open', $event)"
      />
    </div>
  </div>
</template>

<script setup>
import { invoke } from '@tauri-apps/api/core';

defineOptions({ name: 'FileTreeItem' });

const props = defineProps({
  entry: Object,
  depth: { type: Number, default: 0 },
});

const emit = defineEmits(['open']);

async function toggleExpand() {
  const e = props.entry;
  if (!e.is_dir) return;
  e.expanded = !e.expanded;
  if (e.expanded && !e.children) {
    try {
      const list = await invoke('list_directory', { dirPath: e.path });
      e.children = list.map(c => ({ ...c, children: null, expanded: false }));
    } catch (err) {
      console.error('toggleExpand:', err);
      e.children = [];
    }
  }
}

function onClick() {
  if (props.entry.is_dir) {
    toggleExpand();
  } else {
    emit('open', props.entry.path);
  }
}
</script>
