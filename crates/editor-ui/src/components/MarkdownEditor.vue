<template>
  <div id="editor-container">
    <div ref="gutter" id="line-gutter"></div>
    <div
      v-if="currentViewMode === 'source'"
      id="block-editor"
      class="source-view"
    >
      <div v-if="sourceLoading" class="md-block-placeholder">Loading source...</div>
      <pre v-else class="md-source-view">{{ sourceText || '(empty)' }}</pre>
    </div>
    <div
      v-else
      ref="viewport"
      id="block-editor"
      @scroll.passive="onScroll"
    >
      <div v-if="!layoutReady" class="md-block-placeholder" style="padding: 24px; text-align: center">Loading blocks…</div>
      <div
        v-show="layoutReady"
        class="scroll-sizer"
        :style="{ height: `${totalHeight}px` }"
      >
        <div
          v-for="b in visibleBlocks"
          :key="b.index"
          class="md-block"
          :data-block-index="b.index"
          :class="{ editing: editingBlockIndex === b.index }"
          :style="{ position: 'absolute', top: `${b.top}px`, left: 0, right: 0 }"
          @mousedown="onBlockMouseDown(b.index, $event)"
          @click="onBlockClick(b.index, $event)"
        >
          <template v-if="editingBlockIndex === b.index">
            <textarea
              v-model="editSource"
              :ref="setTextareaRef"
              class="md-block-textarea"
              spellcheck="false"
              @input="onTextareaInput"
              @blur="exitEdit(false)"
              @keydown="onKeydown"
              @keyup="updateCursorFromTextarea"
              @click="updateCursorFromTextarea"
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
  viewMode: { type: String, default: 'rendered' }, // 'rendered' | 'source'
});

const emit = defineEmits(['dirty', 'cursor', 'blockCount']);

const currentViewMode = ref(props.viewMode);
watch(() => props.viewMode, (v) => { currentViewMode.value = v; });

const viewport = ref(null);
const gutter = ref(null);

const blocks = ref([]); // meta: { index, kind, start, end }
const blockData = ref(new Map()); // index -> { source, node, ... }
const blockHeights = ref([]); // px, measured
const parsedOffset = ref(0);
const totalLen = ref(0);
const hasMoreToParse = ref(false);
const chunkParsing = ref(false);

const scrollTop = ref(0);
const clientHeight = ref(600);
let ignoreScroll = 0;
let loadingVisible = false;
let lastDocId = null;
let anchorAfterEdit = -1;

const editingBlockIndex = ref(-1);
const editingBlockHeight = ref(0);
const editSource = ref('');
const editOriginal = ref('');
const loadingBlockIndex = ref(-1);
const suppressScroll = ref(false);

const sourceText = ref('');
const sourceLoading = ref(false);

const LINE_HEIGHT = 20;
const MIN_BLOCK_HEIGHT = 60;
const AVG_CHARS_PER_LINE = 45;
const RENDER_BUFFER = 300; // px above/below

const blockLayout = ref([]);
const totalHeight = ref(0);
const layoutReady = ref(false);

let textareaEl = null;

function setTextareaRef(el) {
  if (el) textareaEl = el;
}

function recomputeLayout(startIdx = 0) {
  const list = blockLayout.value.slice();
  let top = 0;
  if (startIdx > 0 && list[startIdx - 1]) {
    top = list[startIdx - 1].top + list[startIdx - 1].height;
  }
  for (let i = startIdx; i < blocks.value.length; i++) {
    const b = blocks.value[i];
    const h = getBlockHeight(i);
    if (list[i]) {
      list[i].top = top;
      list[i].height = h;
      list[i].data = blockData.value.get(i) || b;
    } else {
      list.push({
        index: i,
        meta: b,
        data: blockData.value.get(i) || b,
        top,
        height: h,
      });
    }
    top += h;
  }
  list.length = blocks.value.length;
  blockLayout.value = list;
  const pad = hasMoreToParse.value ? clientHeight.value * 2 : 0;
  totalHeight.value = top + pad;
  emit('blockCount', blocks.value.length);
}

// Merge new block metadata while preserving cached data/heights for unchanged blocks.
// Returns the first index whose meta or cached data changed.
function syncBlockState(newMeta) {
  const oldData = blockData.value;
  const oldHeights = blockHeights.value;
  const oldBlocks = blocks.value;
  let delta = 0;
  if (anchorAfterEdit >= 0 && oldBlocks[anchorAfterEdit] && newMeta[anchorAfterEdit]) {
    delta = (newMeta[anchorAfterEdit].end || 0) - (oldBlocks[anchorAfterEdit].end || 0);
  }
  const keyToData = new Map();
  const shiftedKeyToData = new Map();
  for (let i = 0; i < oldBlocks.length; i++) {
    if (!oldData.has(i)) continue;
    const b = oldBlocks[i];
    const data = oldData.get(i);
    const height = oldHeights[i];
    const key = `${b.kind}:${b.start}:${b.end}`;
    if (!keyToData.has(key)) keyToData.set(key, { data, height });
    if (anchorAfterEdit >= 0 && i > anchorAfterEdit) {
      const shiftedKey = `${b.kind}:${b.start + delta}:${b.end + delta}`;
      if (!shiftedKeyToData.has(shiftedKey)) shiftedKeyToData.set(shiftedKey, { data, height });
    }
  }
  const newBlocks = newMeta.map((m, i) => ({ index: i, ...m }));
  const newData = new Map();
  const newHeights = new Array(newBlocks.length).fill(0);
  let firstChanged = Infinity;
  for (let i = 0; i < newBlocks.length; i++) {
    const b = newBlocks[i];
    if (i === anchorAfterEdit) { firstChanged = Math.min(firstChanged, i); continue; }
    const key = `${b.kind}:${b.start}:${b.end}`;
    const shiftedKey = `${b.kind}:${b.start}:${b.end}`;
    const kept = (anchorAfterEdit >= 0 && i > anchorAfterEdit ? shiftedKeyToData.get(shiftedKey) : null) || keyToData.get(key);
    if (kept) {
      newData.set(i, kept.data);
      newHeights[i] = kept.height;
    } else {
      firstChanged = Math.min(firstChanged, i);
    }
  }
  blocks.value = newBlocks;
  blockData.value = newData;
  blockHeights.value = newHeights;
  recomputeLayout(firstChanged < Infinity ? firstChanged : 0);
  return firstChanged;
}

function getBlockHeight(i) {
  if (editingBlockIndex.value === i && editingBlockHeight.value > 0) {
    return editingBlockHeight.value;
  }
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

async function loadSourceView() {
  if (currentViewMode.value !== 'source') return;
  sourceLoading.value = true;
  try {
    sourceText.value = await invoke('get_document_text');
  } catch (e) {
    console.error('loadSourceView:', e);
    sourceText.value = '';
  } finally {
    sourceLoading.value = false;
  }
}

async function loadDocument() {
  if (currentViewMode.value === 'source') {
    await loadSourceView();
    return;
  }
  if (!props.doc?.id) {
    blocks.value = [];
    blockData.value = new Map();
    blockLayout.value = [];
    totalHeight.value = 0;
    emit('blockCount', 0);
    return;
  }
  const isDocChange = props.doc.id !== lastDocId;
  lastDocId = props.doc.id;
  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  const savedClient = viewport.value ? viewport.value.clientHeight : clientHeight.value;
  // Anchor by the block currently in the middle of the viewport.
  let anchorIndex = -1;
  let anchorOffset = 0;
  if (!isDocChange && blockLayout.value.length) {
    const mid = savedScroll + savedClient / 2;
    const found = blockLayout.value.find(b => b.top <= mid && b.top + b.height > mid);
    if (found) {
      anchorIndex = found.index;
      anchorOffset = mid - found.top;
    }
  }
  layoutReady.value = false;
  editingBlockIndex.value = -1;
  editingBlockHeight.value = 0;
  blockData.value = new Map();
  blockHeights.value = [];
  try {
    const meta = await invoke('get_syntax_tree_meta');
    const [off, total] = await invoke('get_parsed_offset');
    parsedOffset.value = off;
    totalLen.value = total;
    hasMoreToParse.value = off < total;
    blocks.value = meta.map((m, i) => ({ index: i, ...m }));
    blockHeights.value = new Array(blocks.value.length).fill(0);
    recomputeLayout();
    scrollTop.value = isDocChange ? 0 : savedScroll;
    await updateVisibleAndLoad();
    // Wait for the first real height measurement before restoring scroll
    // so the browser doesn't clamp to an underestimated totalHeight.
    await new Promise(r => requestAnimationFrame(r));
    await nextTick();
    measureHeights();
    await nextTick();
    let target = isDocChange ? 0 : savedScroll;
    if (!isDocChange && anchorIndex >= 0 && blockLayout.value[anchorIndex]) {
      const b = blockLayout.value[anchorIndex];
      target = Math.max(0, b.top + anchorOffset - savedClient / 2);
    }
    setScrollTop(target);
  } catch (e) {
    console.error('loadDocument:', e);
  }
}

watch(() => [props.doc?.id, currentViewMode.value], loadDocument);

function onScroll() {
  if (ignoreScroll > 0) { ignoreScroll--; return; }
  if (suppressScroll.value || !viewport.value) return;
  scrollTop.value = viewport.value.scrollTop;
  if (gutter.value) gutter.value.scrollTop = scrollTop.value;
  updateVisibleAndLoad();
  maybeParseNextChunk();
}

function setScrollTop(value) {
  if (!viewport.value) return;
  if (viewport.value.scrollTop === value) return;
  ignoreScroll++;
  viewport.value.scrollTop = value;
  scrollTop.value = value;
}

function restoreScroll(savedScroll) {
  if (!viewport.value) return;
  const ch = viewport.value.clientHeight || clientHeight.value;
  let target = savedScroll;
  if (anchorAfterEdit >= 0 && blockLayout.value[anchorAfterEdit]) {
    const b = blockLayout.value[anchorAfterEdit];
    target = Math.max(0, b.top - ch / 4);
    anchorAfterEdit = -1;
  }
  setScrollTop(target);
}

let scrollRaf = 0;
function scheduleScroll() {
  if (scrollRaf) return;
  scrollRaf = requestAnimationFrame(() => {
    scrollRaf = 0;
    onScroll();
  });
}

async function updateVisibleAndLoad() {
  if (loadingVisible) return;
  loadingVisible = true;
  try {
    const toLoad = visibleBlocks.value
      .filter(b => !blockData.value.has(b.index) && loadingBlockIndex.value !== b.index && editingBlockIndex.value !== b.index)
      .map(b => b.index);
    if (toLoad.length) {
      const results = await Promise.all(toLoad.map(i => loadBlockData(i)));
      for (const data of results) {
        if (data) {
          blockData.value.set(data.index, data);
          blockHeights.value[data.index] = 0;
        }
      }
    }
    nextTick(() => requestAnimationFrame(measureHeights));
  } finally {
    loadingVisible = false;
  }
}

async function loadBlockData(index) {
  if (loadingBlockIndex.value === index || editingBlockIndex.value === index) return null;
  if (blockData.value.has(index)) return null;
  try {
    const data = await invoke('get_block_data', { blockIndex: index });
    if (data) return { ...data, index };
  } catch (e) {
    console.error('loadBlockData:', e);
  }
  return null;
}

function measureHeights() {
  if (!viewport.value) return;
  const els = viewport.value.querySelectorAll('[data-block-index]');
  let firstChanged = Infinity;
  for (const el of els) {
    const idx = Number(el.dataset.blockIndex);
    if (editingBlockIndex.value === idx) continue;
    const h = el.offsetHeight;
    if (h > 0 && h !== blockHeights.value[idx]) {
      blockHeights.value[idx] = h;
      firstChanged = Math.min(firstChanged, idx);
    }
  }
  if (firstChanged < Infinity) recomputeLayout(firstChanged);
  layoutReady.value = true;
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
  loadingBlockIndex.value = -1;
  if (index < 0 || index >= blocks.value.length) return;

  let data = blockData.value.get(index);
  if (!data) {
    loadingBlockIndex.value = index;
    try {
      data = await invoke('get_block_data', { blockIndex: index });
      if (data) blockData.value.set(index, data);
    } catch (e) {
      console.error('enterEdit get_block_data:', e);
    }
    if (loadingBlockIndex.value !== index) return;
    loadingBlockIndex.value = -1;
  }
  if (!data) return;

  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  editingBlockIndex.value = index;
  editSource.value = data.source || '';
  editOriginal.value = editSource.value;
  nextTick(() => {
    const el = textareaEl;
    if (el) {
      autoSize(el);
      el.focus({ preventScroll: true });
      el.selectionStart = el.value.length;
      el.selectionEnd = el.value.length;
      updateCursorFromTextarea();
    }
    setScrollTop(savedScroll);
  });
}

async function exitEdit(force) {
  if (editingBlockIndex.value < 0) return;
  const index = editingBlockIndex.value;
  const old = editOriginal.value;
  const changed = editSource.value !== old;

  if (changed || force) {
    try {
      await invoke('replace_block', { blockIndex: index, newSource: editSource.value });
      const meta = await invoke('get_syntax_tree_meta');
      anchorAfterEdit = index;
      syncBlockState(meta);
      const data = await invoke('get_block_data', { blockIndex: index });
      if (data) blockData.value.set(index, data);
      if (changed) emit('dirty', true);
    } catch (e) {
      console.error('exitEdit replace:', e);
    }
  }

  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  editingBlockIndex.value = -1;
  editingBlockHeight.value = 0;
  editSource.value = '';
  editOriginal.value = '';
  textareaEl = null;
  nextTick(() => {
    updateVisibleAndLoad().then(() => {
      nextTick(() => requestAnimationFrame(() => {
        measureHeights();
        restoreScroll(savedScroll);
      }));
    });
  });
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

function onTextareaInput(e) {
  const el = e.target || textareaEl;
  autoSize(el);
  updateCursorFromTextarea();
}

function autoSize(el) {
  if (!el || !el.isConnected) {
    el = textareaEl;
  }
  if (!el || !el.isConnected) return;
  el.style.height = 'auto';
  el.style.height = `${el.scrollHeight}px`;
  if (editingBlockIndex.value >= 0) {
    const oldHeight = editingBlockHeight.value;
    editingBlockHeight.value = el.scrollHeight;
    if (editingBlockHeight.value !== oldHeight) {
      recomputeLayout();
    }
  }
}

function posToLineCol(text, offset) {
  const lines = text.substring(0, offset).split('\n');
  const line = lines.length;
  const col = lines[lines.length - 1].length + 1;
  return { line, col };
}

function updateCursorFromTextarea() {
  const el = textareaEl;
  if (!el) return;
  const pos = posToLineCol(editSource.value, el.selectionStart);
  emit('cursor', pos);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

function getActiveTextarea() {
  if (editingBlockIndex.value < 0) return null;
  if (textareaEl && textareaEl.isConnected) return textareaEl;
  return viewport.value?.querySelector('textarea.md-block-textarea') || null;
}

function findBlockFromSelection() {
  const sel = window.getSelection();
  if (!sel?.rangeCount) return null;
  let node = sel.anchorNode;
  while (node && node !== viewport.value) {
    if (node.nodeType === 1 && node.classList?.contains('md-block')) {
      const idx = node.dataset?.blockIndex;
      if (idx != null) return { el: node, index: parseInt(idx, 10) };
    }
    node = node.parentNode;
  }
  return null;
}

async function getBlockSource(index) {
  let data = blockData.value.get(index);
  if (!data) {
    try {
      data = await invoke('get_block_data', { blockIndex: index });
      if (data) blockData.value.set(index, data);
    } catch (e) {
      console.error('getBlockSource:', e);
    }
  }
  return data?.source || '';
}

async function replaceBlockSource(index, newSource) {
  try {
    const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
    await invoke('replace_block', { blockIndex: index, newSource });
    emit('dirty', true);
    const meta = await invoke('get_syntax_tree_meta');
    anchorAfterEdit = index;
    syncBlockState(meta);
    const data = await invoke('get_block_data', { blockIndex: index });
    if (data) blockData.value.set(index, data);
    await updateVisibleAndLoad();
    nextTick(() => requestAnimationFrame(() => {
      measureHeights();
      restoreScroll(savedScroll);
    }));
  } catch (e) {
    console.error('replaceBlockSource:', e);
  }
}

// Find a plain-text string in Markdown source, returning {start, end} byte offsets.
function findPlainTextInSource(source, plainText) {
  const markers = ['**', '__', '*', '_', '~~', '`'];
  let i = 0;
  let plain = '';
  const sourcePos = [];
  while (i < source.length) {
    let matched = false;
    for (const m of markers) {
      if (source.substring(i, i + m.length) === m) {
        i += m.length;
        matched = true;
        break;
      }
    }
    if (matched) continue;
    if (source[i] === '[' || source[i] === ']' || source[i] === '(' || source[i] === ')') {
      if (source[i] === ']' && i + 1 < source.length && source[i + 1] === '(') {
        i++;
        if (source[i] === '(') i++;
        while (i < source.length && source[i] !== ')') i++;
        if (i < source.length) i++;
        continue;
      }
      if (source[i] === '[' || source[i] === '!') { i++; continue; }
      i++;
      continue;
    }
    sourcePos.push(i);
    plain += source[i];
    i++;
  }
  const idx = plain.indexOf(plainText);
  if (idx < 0) return null;
  return { start: sourcePos[idx], end: sourcePos[idx + plainText.length - 1] + 1 };
}

function toggleWrap(ta, prefix, suffix) {
  if (!ta) return;
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const value = ta.value;
  const selected = value.substring(start, end) || '';
  const before = value.substring(0, start);
  const after = value.substring(end);
  const hasPrefix = before.endsWith(prefix);
  const hasSuffix = after.startsWith(suffix);
  if (selected && hasPrefix && hasSuffix) {
    // unwrap
    ta.value = before.slice(0, -prefix.length) + selected + after.slice(suffix.length);
    ta.selectionStart = start - prefix.length;
    ta.selectionEnd = end - prefix.length;
  } else if (selected) {
    // wrap
    ta.value = before + prefix + selected + suffix + after;
    ta.selectionStart = start + prefix.length;
    ta.selectionEnd = end + prefix.length;
  } else {
    // nothing selected, insert wrappers and place cursor between
    ta.value = before + prefix + suffix + after;
    ta.selectionStart = start + prefix.length;
    ta.selectionEnd = start + prefix.length;
  }
  editSource.value = ta.value;
  nextTick(() => autoSize(ta));
  updateCursorFromTextarea();
}

function toggleLinePrefix(ta, prefix) {
  if (!ta) return;
  const value = ta.value;
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const before = value.substring(0, start);
  const after = value.substring(end);
  const sel = value.substring(start, end);
  const lines = sel.split('\n');
  const allPrefixed = lines.every(line => line.startsWith(prefix));
  let newSel;
  if (allPrefixed) {
    newSel = lines.map(line => line.slice(prefix.length)).join('\n');
  } else {
    newSel = lines.map(line => (line ? prefix + line : '')).join('\n');
  }
  const newBefore = before;
  const newAfter = after;
  ta.value = newBefore + newSel + newAfter;
  ta.selectionStart = start;
  ta.selectionEnd = start + newSel.length;
  editSource.value = ta.value;
  nextTick(() => autoSize(ta));
  updateCursorFromTextarea();
}

function toggleHeadingInTextarea(ta, level) {
  if (!ta) return;
  const value = ta.value;
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const before = value.substring(0, start);
  const after = value.substring(end);
  const sel = value.substring(start, end);
  let firstLineBreak = sel.indexOf('\n');
  if (firstLineBreak < 0) firstLineBreak = sel.length;
  const firstLine = sel.substring(0, firstLineBreak);
  const rest = sel.substring(firstLineBreak);
  const match = firstLine.match(/^(#{1,6})\s+(.*)$/);
  let newFirstLine;
  if (level === 0) {
    newFirstLine = match ? match[2] : firstLine;
  } else {
    newFirstLine = `${'#'.repeat(level)} ${match ? match[2] : firstLine}`;
  }
  const newSel = newFirstLine + rest;
  ta.value = before + newSel + after;
  ta.selectionStart = start;
  ta.selectionEnd = start + newSel.length;
  editSource.value = ta.value;
  nextTick(() => autoSize(ta));
  updateCursorFromTextarea();
}

function insertLinkInTextarea(ta) {
  if (!ta) return;
  const start = ta.selectionStart;
  const end = ta.selectionEnd;
  const value = ta.value;
  const selected = value.substring(start, end).trim() || 'text';
  const url = prompt('URL:', 'https://');
  if (!url) return;
  const replacement = `[${selected}](${url})`;
  ta.value = value.substring(0, start) + replacement + value.substring(end);
  ta.selectionStart = start + replacement.length;
  ta.selectionEnd = start + replacement.length;
  editSource.value = ta.value;
  nextTick(() => autoSize(ta));
  updateCursorFromTextarea();
}

async function insertThematicBreak() {
  const ta = getActiveTextarea();
  if (ta) {
    const start = ta.selectionStart;
    const value = ta.value;
    const line = value.lastIndexOf('\n', start) + 1;
    ta.value = value.substring(0, line) + '---\n' + value.substring(line);
    editSource.value = ta.value;
    nextTick(() => autoSize(ta));
    updateCursorFromTextarea();
  } else {
    // Insert as a new block at the end of the current document if not editing.
    const value = '---\n';
    await invoke('insert_text', { position: totalLen.value || 0, text: value });
    await loadDocument();
    emit('dirty', true);
  }
}

async function insertCodeBlock() {
  const ta = getActiveTextarea();
  if (ta) {
    const start = ta.selectionStart;
    const end = ta.selectionEnd;
    const value = ta.value;
    const selected = value.substring(start, end);
    const replacement = '```\n' + selected + '\n```\n';
    ta.value = value.substring(0, start) + replacement + value.substring(end);
    editSource.value = ta.value;
    nextTick(() => autoSize(ta));
    updateCursorFromTextarea();
  } else {
    await invoke('insert_text', { position: totalLen.value || 0, text: '```\n\n```\n' });
    await loadDocument();
    emit('dirty', true);
  }
}

async function applyInlineFormat(prefix, suffix) {
  const ta = getActiveTextarea();
  if (ta) {
    toggleWrap(ta, prefix, suffix || prefix);
    return;
  }
  const sel = window.getSelection();
  if (!sel?.rangeCount || sel.isCollapsed) return;
  const selectedText = sel.toString().trim();
  if (!selectedText) return;
  const found = findBlockFromSelection();
  if (!found) return;
  const source = await getBlockSource(found.index);
  if (!source) return;
  const result = findPlainTextInSource(source, selectedText);
  if (!result) {
    await enterEdit(found.index);
    return;
  }
  const suf = suffix || prefix;
  const before = source.substring(Math.max(0, result.start - prefix.length), result.start);
  const after = source.substring(result.end, result.end + suf.length);
  let newSource;
  if (before === prefix && after === suf) {
    newSource = source.substring(0, result.start - prefix.length) +
      source.substring(result.start, result.end) +
      source.substring(result.end + suf.length);
  } else {
    newSource = source.substring(0, result.start) +
      prefix + source.substring(result.start, result.end) + suf +
      source.substring(result.end);
  }
  await replaceBlockSource(found.index, newSource);
}

async function applyLineFormat(prefix) {
  const ta = getActiveTextarea();
  if (ta) {
    toggleLinePrefix(ta, prefix);
    return;
  }
  const found = findBlockFromSelection();
  if (!found) return;
  const source = await getBlockSource(found.index);
  if (!source) return;
  const lines = source.split('\n');
  const allPrefixed = lines.every(line => line.startsWith(prefix));
  const newSource = allPrefixed
    ? lines.map(line => line.slice(prefix.length)).join('\n')
    : lines.map(line => (line ? prefix + line : '')).join('\n');
  await replaceBlockSource(found.index, newSource);
}

async function applyHeading(level) {
  const ta = getActiveTextarea();
  if (ta) {
    toggleHeadingInTextarea(ta, level);
    return;
  }
  const found = findBlockFromSelection();
  if (!found) return;
  const source = await getBlockSource(found.index);
  if (!source) return;
  const stripped = source
    .replace(/^#{1,6}\s+/, '')
    .replace(/^={2,}\s*$\n?/m, '')
    .replace(/^-{2,}\s*$\n?/m, '');
  const prefix = level > 0 ? '#'.repeat(level) + ' ' : '';
  await replaceBlockSource(found.index, prefix + stripped);
}

async function applyLink() {
  const ta = getActiveTextarea();
  if (ta) {
    insertLinkInTextarea(ta);
    return;
  }
  const sel = window.getSelection();
  if (!sel?.rangeCount || sel.isCollapsed) return;
  const selectedText = sel.toString().trim();
  if (!selectedText) return;
  const url = prompt('URL:', 'https://');
  if (!url) return;
  const found = findBlockFromSelection();
  if (!found) return;
  const source = await getBlockSource(found.index);
  if (!source) return;
  const result = findPlainTextInSource(source, selectedText);
  if (!result) return;
  const newSource = source.substring(0, result.start) +
    `[${source.substring(result.start, result.end)}](${url})` +
    source.substring(result.end);
  await replaceBlockSource(found.index, newSource);
}

async function toggleTask() {
  await applyLineFormat('- [ ] ');
}

function focusFirst() {
  if (!viewport.value) return;
  viewport.value.focus();
}

let resizeObserver = null;

onMounted(() => {
  if (viewport.value) {
    clientHeight.value = viewport.value.clientHeight;
    scrollTop.value = viewport.value.scrollTop;
    resizeObserver = new ResizeObserver((entries) => {
      const h = entries[0]?.contentRect?.height || viewport.value?.clientHeight || 0;
      if (h > 0 && h !== clientHeight.value) {
        clientHeight.value = h;
        updateVisibleAndLoad();
      }
    });
    resizeObserver.observe(viewport.value);
  }
  loadDocument();
});

onUnmounted(() => {
  if (scrollRaf) cancelAnimationFrame(scrollRaf);
  if (resizeObserver) { resizeObserver.disconnect(); resizeObserver = null; }
});

function save() {
  return exitEdit(false);
}

function setViewMode(mode) {
  currentViewMode.value = mode;
}

const isSourceView = computed(() => currentViewMode.value === 'source');

defineExpose({
  applyInlineFormat,
  applyLineFormat,
  applyHeading,
  applyLink,
  toggleTask,
  insertThematicBreak,
  insertCodeBlock,
  getActiveTextarea,
  editingBlockIndex,
  blocks,
  focusFirst,
  loadDocument,
  save,
  setViewMode,
  isSourceView,
});
</script>
