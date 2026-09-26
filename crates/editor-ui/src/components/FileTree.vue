<template>
  <div id="file-tree-panel" :style="{ width: `${width}px` }" :class="{ hidden: !visible }">
    <div id="file-tree-header">
      <span id="file-tree-root-name">{{ rootName }}</span>
      <div id="file-tree-actions">
        <button id="btn-tree-open-folder" title="Open folder" @click="openFolderDialog">📂</button>
        <button id="btn-tree-up" title="Parent folder" @click="goUp">↑</button>
        <button id="btn-tree-close" title="Close file tree" @click="$emit('close')">×</button>
      </div>
    </div>
    <div id="file-tree-search">
      <input id="file-tree-filter" v-model="filter" type="text" placeholder="Filter files..." />
    </div>
    <div id="file-tree-content">
      <div v-if="!root" class="file-tree-empty">No folder open</div>
      <div v-else-if="loading" class="file-tree-empty">Loading...</div>
      <div v-else-if="filteredEntries.length === 0" class="file-tree-empty">No files</div>
      <FileTreeItem
        v-for="entry in filteredEntries"
        :key="entry.path"
        :entry="entry"
        @open="$emit('open', $event)"
      />
    </div>
  </div>
</template>

<script setup>
import { ref, computed, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import FileTreeItem from './FileTreeItem.vue';

const props = defineProps({
  visible: Boolean,
  root: String,
  width: { type: Number, default: 260 },
});

const emit = defineEmits(['close', 'open', 'update:root']);

const loading = ref(false);
const entries = ref([]);
const filter = ref('');

const rootName = computed(() => {
  if (!props.root) return 'No folder open';
  const parts = props.root.replace(/\\/g, '/').split('/');
  return parts[parts.length - 1] || props.root;
});

const filteredEntries = computed(() => {
  const f = filter.value.trim().toLowerCase();
  if (!f) return entries.value;
  function walk(list) {
    const result = [];
    for (const e of list) {
      if (e.name.toLowerCase().includes(f)) {
        result.push(e);
      } else if (e.is_dir && e.children) {
        const children = walk(e.children);
        if (children.length) {
          result.push({ ...e, children, expanded: true });
        }
      }
    }
    return result;
  }
  return walk(entries.value);
});

async function loadEntries(dir) {
  if (!dir) { entries.value = []; return; }
  loading.value = true;
  try {
    const list = await invoke('list_directory', { dirPath: dir });
    entries.value = list.map(e => ({ ...e, children: null, expanded: false }));
  } catch (e) {
    console.error('loadEntries:', e);
    entries.value = [];
  } finally {
    loading.value = false;
  }
}

watch(() => props.root, loadEntries, { immediate: true });

async function openFolderDialog() {
  try {
    // multiple:false returns a plain path string (not an array) — indexing
    // [0] would emit the first CHARACTER of the path as the new root.
    const selected = await open({ directory: true, multiple: false });
    const path = Array.isArray(selected) ? selected[0] : selected;
    if (path) {
      emit('update:root', path);
    }
  } catch (e) {
    console.error('openFolderDialog:', e);
  }
}

async function goUp() {
  if (!props.root) return;
  try {
    const parent = await invoke('get_parent_dir', { dirPath: props.root });
    if (parent) {
      emit('update:root', parent);
    }
  } catch (e) {
    console.error('goUp:', e);
  }
}
</script>
