<template>
  <div id="editor-container">
    <div ref="gutter" id="line-gutter"></div>
    <div
      ref="viewport"
      id="block-editor"
      @scroll.passive="onScroll"
    >
      <div
        class="scroll-sizer"
        :style="{ height: `${totalHeight}px` }"
      >
        <div
          v-for="b in visibleBlocks"
          :key="b.index"
          class="md-block"
          :data-block-index="b.index"
          :style="{ position: 'absolute', top: `${b.top}px`, left: 0, right: 0 }"
          @mousedown="onBlockMouseDown(b.index, $event)"
          @click="onBlockClick(b.index, $event)"
        >
          <template v-if="editingBlockIndex === b.index">
            <textarea
              ref="textarea"
              v-model="editSource"
              class="md-block-textarea"
              spellcheck="false"
              @input="autoSize"
              @blur="exitEdit(false)"
              @keydown="onKeydown"
            />
          </template>
          <div v-else v-html="renderBlockHtml(b.data)"></div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, computed, watch, nextTick, onMounted, onUnmounted } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { renderBlockHtml } from '../render.js';

const props = defineProps({
  doc: Object, // { id: string, name: string, path: string | null }
});

const emit = defineEmits(['dirty']);

const viewport = ref(null);
const gutter = ref(null);
const textarea = ref(null);

const blocks = ref([]); // meta: { index, kind, start, end }
const blockData = ref(new Map()); // index -> { source, node, ... }
const blockHeights = ref([]); // px, measured
const parsedOffset = ref(0);
const totalLen = ref(0);
const hasMoreToParse = ref(false);
const chunkParsing = ref(false);

const scrollTop = ref(0);
const clientHeight = ref(600);

const editingBlockIndex = ref(-1);
const editSource = ref('');
const editOriginal = ref('');
const loadingBlockIndex = ref(-1);
const suppressScroll = ref(false);

const LINE_HEIGHT = 20;
const MIN_BLOCK_HEIGHT = 60;
const AVG_CHARS_PER_LINE = 45;
const RENDER_BUFFER = 300; // px above/below

// Cache block layout in a single O(N) pass. Do not use a computed that calls
// an O(N) cumulative function per block — that is O(N^2) and hangs on 25k blocks.
const blockLayout = ref([]);
const totalHeight = ref(0);

function recomputeLayout() {
  const list = [];
  let top = 0;
  for (let i = 0; i < blocks.value.length; i++) {
    const b = blocks.value[i];
    const h = getBlockHeight(i);
    list.push({
      index: i,
      meta: b,
      data: blockData.value.get(i) || b,
      top,
      height: h,
    });
    top += h;
  }
  blockLayout.value = list;
  const pad = hasMoreToParse.value ? clientHeight.value * 2 : 0;
  totalHeight.value = top + pad;
}

function getBlockHeight(i) {
  const h = blockHeights.value[i];
  if (h > 0) return h;
  const b = blocks.value[i];
  if (!b) return MIN_BLOCK_HEIGHT;
  const span = Math.max(0, (b.end || 0) - (b.start || 0));
  return Math.max(MIN_BLOCK_HEIGHT, Math.round((span / AVG_CHARS_PER_LINE) * LINE_HEIGHT));
}

const visibleBlocks = computed(() => {
  const st = scrollTop.value - RENDER_BUFFER;
  const en = scrollTop.value + clientHeight.value + RENDER_BUFFER;
  return blockLayout.value.filter(b => {
    const top = b.top;
    const bottom = top + b.height;
    return bottom >= st && top <= en;
  });
});

async function loadDocument() {
  if (!props.doc?.id) {
    blocks.value = [];
    blockData.value = new Map();
    return;
  }
  try {
    const meta = await invoke('get_syntax_tree_meta');
    const [off, total] = await invoke('get_parsed_offset');
    parsedOffset.value = off;
    totalLen.value = total;
    hasMoreToParse.value = off < total;
    blocks.value = meta.map((m, i) => ({ index: i, ...m }));
    blockHeights.value = new Array(blocks.value.length).fill(0);
    recomputeLayout();
    scrollTop.value = 0;
    // Load visible block data.
    updateVisibleAndLoad();
  } catch (e) {
    console.error('loadDocument:', e);
  }
}

watch(() => props.doc?.id, loadDocument, { immediate: true });

function onScroll() {
  if (suppressScroll.value || !viewport.value) return;
  scrollTop.value = viewport.value.scrollTop;
  if (gutter.value) gutter.value.scrollTop = scrollTop.value;
  updateVisibleAndLoad();
  maybeParseNextChunk();
}

let scrollRaf = 0;
function scheduleScroll() {
  if (scrollRaf) return;
  scrollRaf = requestAnimationFrame(() => {
    scrollRaf = 0;
    onScroll();
  });
}

function updateVisibleAndLoad() {
  // Ensure data is loaded for all visible blocks and measure heights.
  nextTick(measureHeights);
  for (const b of visibleBlocks.value) {
    if (!blockData.value.has(b.index)) {
      loadBlockData(b.index);
    }
  }
}

async function loadBlockData(index) {
  if (loadingBlockIndex.value === index || editingBlockIndex.value === index) return;
  if (blockData.value.has(index)) return;
  try {
    const data = await invoke('get_block_data', { block_index: index });
    if (data) {
      blockData.value.set(index, data);
      blockHeights.value[index] = 0; // re-measure
      nextTick(measureHeights);
    }
  } catch (e) {
    console.error('loadBlockData:', e);
  }
}

function measureHeights() {
  if (!viewport.value) return;
  const els = viewport.value.querySelectorAll('[data-block-index]');
  let changed = false;
  for (const el of els) {
    const idx = Number(el.dataset.blockIndex);
    if (editingBlockIndex.value === idx) continue;
    const h = el.offsetHeight;
    if (h > 0 && h !== blockHeights.value[idx]) {
      blockHeights.value[idx] = h;
      changed = true;
    }
  }
  if (changed) recomputeLayout();
}

async function maybeParseNextChunk() {
  if (!hasMoreToParse.value || chunkParsing.value) return;
  const parsedEnd = totalHeight.value - (hasMoreToParse.value ? clientHeight.value * 2 : 0);
  if (scrollTop.value < parsedEnd) return;
  chunkParsing.value = true;
  try {
    const [newOffset, total, newBlockCount] = await invoke('parse_next_chunk');
    parsedOffset.value = newOffset;
    totalLen.value = total;
    hasMoreToParse.value = newOffset < total;
    const meta = await invoke('get_syntax_tree_meta');
    const oldBlocks = blocks.value;
    const oldHeights = blockHeights.value;
    const newBlocks = meta.map((m, i) => {
      const old = i < oldBlocks.length ? oldBlocks[i] : null;
      if (old && old.kind === m.kind && old.start === m.start && old.end === m.end) {
        return { ...old, ...m };
      }
      return { index: i, ...m };
    });
    blocks.value = newBlocks;
    blockHeights.value = newBlocks.map((b, i) => oldHeights[i] || 0);
    recomputeLayout();
    updateVisibleAndLoad();
  } catch (e) {
    console.error('parse_next_chunk:', e);
  } finally {
    chunkParsing.value = false;
  }
}

let mouseDownX = 0;
let mouseDownY = 0;

function onBlockMouseDown(index, e) {
  mouseDownX = e.clientX;
  mouseDownY = e.clientY;
}

function onBlockClick(index, e) {
  if (e.target.closest('a')) return;
  const dx = Math.abs(e.clientX - mouseDownX);
  const dy = Math.abs(e.clientY - mouseDownY);
  if (dx > 3 || dy > 3) return;
  const sel = window.getSelection();
  if (sel && !sel.isCollapsed && sel.toString().trim().length > 0) return;
  e.stopPropagation();
  enterEdit(index);
}

async function enterEdit(index) {
  if (editingBlockIndex.value === index) return;
  if (editingBlockIndex.value >= 0) {
    await exitEdit(true);
  }
  // Cancel any stale load.
  loadingBlockIndex.value = -1;
  if (index < 0 || index >= blocks.value.length) return;

  // Load data if missing.
  let data = blockData.value.get(index);
  if (!data) {
    loadingBlockIndex.value = index;
    try {
      data = await invoke('get_block_data', { block_index: index });
      if (data) blockData.value.set(index, data);
    } catch (e) {
      console.error('enterEdit get_block_data:', e);
    }
    if (loadingBlockIndex.value !== index) return; // interrupted
    loadingBlockIndex.value = -1;
  }
  if (!data) return;

  editingBlockIndex.value = index;
  editSource.value = data.source || '';
  editOriginal.value = editSource.value;
  nextTick(() => {
    autoSize();
    textarea.value?.focus({ preventScroll: true });
  });
}

async function exitEdit(force) {
  if (editingBlockIndex.value < 0) return;
  const index = editingBlockIndex.value;
  const old = editOriginal.value;
  const changed = editSource.value !== old;

  if (changed || force) {
    try {
      await invoke('replace_block', { block_index: index, new_source: editSource.value });
      // Refresh block list and reload this block.
      const meta = await invoke('get_syntax_tree_meta');
      blocks.value = meta.map((m, i) => {
        const oldMeta = blocks.value[i];
        if (oldMeta && oldMeta.kind === m.kind && oldMeta.start === m.start && oldMeta.end === m.end) {
          return { ...oldMeta, ...m };
        }
        return { index: i, ...m };
      });
      blockHeights.value = new Array(blocks.value.length).fill(0);
      recomputeLayout();
      const data = await invoke('get_block_data', { block_index: index });
      if (data) blockData.value.set(index, data);
      if (changed) emit('dirty', true);
    } catch (e) {
      console.error('exitEdit replace:', e);
    }
  }

  editingBlockIndex.value = -1;
  editSource.value = '';
  editOriginal.value = '';
  nextTick(measureHeights);
}

function onKeydown(e) {
  if (e.key === 'Escape') {
    e.preventDefault();
    exitEdit(false);
  } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    exitEdit(false);
  } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
    e.preventDefault();
    exitEdit(false);
  }
}

function autoSize() {
  if (!textarea.value) return;
  const el = textarea.value;
  el.style.height = 'auto';
  el.style.height = `${el.scrollHeight}px`;
}

onMounted(() => {
  if (viewport.value) {
    clientHeight.value = viewport.value.clientHeight;
    scrollTop.value = viewport.value.scrollTop;
  }
});

onUnmounted(() => {
  if (scrollRaf) cancelAnimationFrame(scrollRaf);
});
</script>
