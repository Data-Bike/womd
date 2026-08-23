<template>
  <div id="editor-container">
    <div ref="gutter" id="line-gutter">
      <template v-if="currentViewMode === 'source'">
        <div
          v-for="n in sourceLineCount"
          :key="'s' + n"
          class="gutter-line source-gutter-line"
        >{{ n }}</div>
      </template>
      <div
        v-else
        class="gutter-sizer"
        :style="{ height: `${totalHeight}px`, position: 'relative' }"
      >
        <div
          v-for="row in visibleBlocks"
          :key="'g' + row.index"
          class="gutter-line"
          :style="{ position: 'absolute', top: `${row.top}px`, height: `${row.height}px`, left: 0, right: 0 }"
        >{{ showsLineNumber(row.kind) ? row.startLine : '' }}</div>
      </div>
    </div>
    <div
      v-if="currentViewMode === 'source'"
      id="block-editor"
      class="source-view"
    >
      <div v-if="sourceLoading" class="md-block-placeholder">Loading source...</div>
      <textarea
        v-else
        ref="sourceTextarea"
        v-model="sourceText"
        class="md-source-view"
        spellcheck="false"
        @input="onSourceInput"
        @scroll="syncSourceScrollToGutter"
        @keydown="onSourceKeydown"
      />
    </div>
    <div
      v-else
      ref="viewport"
      id="block-editor"
      tabindex="0"
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
          :class="{ editing: editingIndex === b.index }"
          :style="{ position: 'absolute', top: `${b.top}px`, left: 0, right: 0 }"
          @mousedown="onBlockMouseDown(b.index, $event)"
          @click="onBlockClick(b.index, $event)"
        >
          <template v-if="editingIndex === b.index">
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
import { ref, computed, watch, nextTick, onMounted, onUnmounted, onUpdated } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { renderBlockHtml } from '../render.js';
import * as textEditing from '../lib/textEditing.js';

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------
// This component virtualizes a Markdown document as a list of "blocks"
// (paragraphs, headings, lists, ...). For performance on very large
// documents, the bulk of the per-block bookkeeping (metadata, measured
// heights, cumulative offsets, and cached rendered data) is kept in plain
// module-scoped variables instead of Vue refs/reactive objects. Vue's
// reactivity system has real per-object overhead; tracking thousands of
// block entries individually would make every mutation and every render
// pass slower as the document grows. Instead, a handful of small reactive
// "trigger" refs (scrollTop, clientHeight, layoutVersion, dataVersion)
// drive two cheap computed properties (visibleRange, visibleBlocks) that
// use binary search over a cumulative-offset array to find the visible
// slice in O(log n + visible count) time, regardless of document size.
// Only that small visible slice ever becomes part of Vue's reactive
// render tree (via the keyed v-for), so DOM diffing cost also stays
// bounded.

const props = defineProps({
  doc: Object, // { id: string, name: string, path: string | null }
  viewMode: { type: String, default: 'rendered' }, // 'rendered' | 'source'
});

const emit = defineEmits(['dirty', 'cursor', 'blockCount', 'searchStatus']);

const currentViewMode = ref(props.viewMode);
watch(() => props.viewMode, (v) => { currentViewMode.value = v; });

const viewport = ref(null);
const gutter = ref(null);

// ---------------------------------------------------------------------------
// Non-reactive layout state (see overview above).
// ---------------------------------------------------------------------------
let meta = [];          // [{ kind, start, end }] — one entry per parsed block
let startLines = [];    // 1-based document line number for each block's start
let heights = [];       // measured pixel heights; 0 = not yet measured
let offsets = [];       // offsets[i] = top of block i; offsets[n] = content height (no padding)
// Running global average of "measured pixels per source line", accumulated
// from every block whose height has actually been rendered+measured. Used
// to estimate the scroll position of a distant, never-rendered block (see
// estimateScrollForBlock) far more accurately than summing thousands of
// individually-*guessed* block heights via `offsets`: per-block guessing
// error compounds additively with distance in a large document, whereas a
// single calibrated ratio applied to a block's *exact* line number (known
// from startLines, not guessed) does not.
let measuredLineSum = 0;
let measuredPxSum = 0;
let measuredIndices = new Set(); // which blocks have already contributed to the calibration above
let cache = new Map();  // index -> { source, node, ... } full block data
let cacheKeys = [];     // insertion order of `cache`, for bounded eviction
const loadingSet = new Set(); // indices currently being fetched from the backend

const MAX_CACHE_ENTRIES = 2000;
const LINE_HEIGHT = 22;
const MIN_BLOCK_HEIGHT = 28;
const AVG_CHARS_PER_LINE = 45;
const RENDER_BUFFER = 800; // px of overscan above/below the viewport

let lastDocId = null;
let anchorAfterEdit = -1;
let ignoreScroll = 0;
let loadingVisible = false;
let textareaEl = null;
let resizeObserver = null;
let blockResizeObserver = null;
let visibleUpdateRaf = 0;

const parsedOffset = ref(0);
const totalLen = ref(0);
const hasMoreToParse = ref(false);
const chunkParsing = ref(false);

const scrollTop = ref(0);
const clientHeight = ref(600);
const layoutVersion = ref(0); // bumped whenever offsets/heights structurally change
const dataVersion = ref(0);   // bumped whenever cached block data changes
const totalHeight = ref(0);
const layoutReady = ref(false);

const editingIndex = ref(-1);
const editingHeight = ref(0);
const editSource = ref('');
const editOriginal = ref('');
const suppressScroll = ref(false);

const sourceText = ref('');
const sourceLoading = ref(false);
const sourceTextarea = ref(null);

const sourceLineCount = computed(() => Math.max(1, sourceText.value.split(/\r?\n/).length));

function syncSourceScrollToGutter() {
  if (gutter.value && sourceTextarea.value) {
    gutter.value.scrollTop = sourceTextarea.value.scrollTop;
  }
}

function onSourceInput() {
  emit('dirty', true);
}

// See onKeydown above: Ctrl+S is handled once, globally, by App.vue.
function onSourceKeydown() {}

async function saveSource() {
  try {
    const end = totalLen.value || (await invoke('get_parsed_offset'))[1];
    // replace_text's Rust parameter is a single struct named `args`; Tauri's
    // IPC looks the payload up by parameter name, so the fields must be
    // nested under an `args` key (unlike insert_text's two scalar params,
    // which are matched by their own individual names).
    await invoke('replace_text', { args: { start: 0, end, newText: sourceText.value } });
    emit('dirty', true);
    await loadDocument();
  } catch (e) {
    console.error('saveSource:', e);
  }
}

function setTextareaRef(el) {
  if (el) textareaEl = el;
}

function showsLineNumber(kind) {
  return kind !== 'blank-line' && kind !== 'link-ref-def';
}

// ---------------------------------------------------------------------------
// Layout engine: cumulative offsets + binary search
// ---------------------------------------------------------------------------

// Exact (not estimated) line count for block i, from the backend-provided
// startLines table — covers the whole document cheaply, independent of
// which blocks have actually been rendered.
function blockLineCount(i) {
  const next = startLines[i + 1];
  const cur = startLines[i];
  if (next != null && cur != null) return Math.max(1, next - cur);
  return 1;
}

// Feed a real (rendered) height measurement into the global px-per-line
// calibration. Called from measureHeights/onBlockResize whenever a block's
// actual height is learned. Only a block's *first* measurement counts,
// so a later remeasurement (e.g. after a window resize) can't skew the
// average by counting the same block's lines more than once.
function recordMeasuredHeight(i, px) {
  if (measuredIndices.has(i)) return;
  measuredIndices.add(i);
  measuredLineSum += blockLineCount(i);
  measuredPxSum += px;
}

// Estimate the absolute scroll offset of block `index` using the
// calibrated global px-per-line average and its *exact* line number,
// instead of the cumulative `offsets` array (see comment on
// measuredLineSum above for why that compounds error over distance).
function estimateScrollForBlock(index) {
  const avgPxPerLine = measuredLineSum > 0 ? measuredPxSum / measuredLineSum : LINE_HEIGHT;
  const line = startLines[index] ?? 0;
  return Math.max(0, avgPxPerLine * line);
}

function estimateHeight(i) {
  if (editingIndex.value === i && editingHeight.value > 0) return editingHeight.value;
  const h = heights[i];
  if (h > 0) return h;
  const m = meta[i];
  if (!m) return MIN_BLOCK_HEIGHT;
  let lines;
  const nextStart = startLines[i + 1];
  if (nextStart != null && startLines[i] != null) {
    lines = Math.max(1, nextStart - startLines[i]);
  } else {
    const span = Math.max(0, (m.end || 0) - (m.start || 0));
    lines = Math.max(1, Math.round(span / AVG_CHARS_PER_LINE));
  }
  let factor = 1.0;
  if (m.kind.includes('table')) factor = 4.0;
  else if (m.kind.includes('list')) factor = 2.5;
  else if (m.kind.startsWith('heading-')) factor = 1.4;
  lines = Math.max(1, Math.round(lines * factor));
  return Math.max(MIN_BLOCK_HEIGHT, lines * LINE_HEIGHT + 6);
}

// Rebuild cumulative offsets starting at `fromIndex` (everything before is
// assumed unchanged). O(n - fromIndex); called only when heights/meta
// actually change, never on every scroll frame.
function rebuildOffsets(fromIndex = 0) {
  let top = fromIndex > 0 && offsets[fromIndex] != null ? offsets[fromIndex] : 0;
  for (let i = fromIndex; i < meta.length; i++) {
    offsets[i] = top;
    top += estimateHeight(i);
  }
  offsets[meta.length] = top;
  offsets.length = meta.length + 1;
  const pad = hasMoreToParse.value ? clientHeight.value * 2 : 0;
  totalHeight.value = top + pad;
  layoutVersion.value++;
  emit('blockCount', meta.length);
}

// Largest index i such that offsets[i] <= y (i.e. the block containing y).
function findIndexAtOffset(y) {
  if (meta.length === 0) return 0;
  let lo = 0, hi = meta.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offsets[mid] <= y) lo = mid; else hi = mid - 1;
  }
  return lo;
}

const visibleRange = computed(() => {
  layoutVersion.value; // track
  if (meta.length === 0) return { start: 0, end: -1 };
  const lo = Math.max(0, scrollTop.value - RENDER_BUFFER);
  const hi = Math.min(offsets[meta.length] || 0, scrollTop.value + clientHeight.value + RENDER_BUFFER);
  const start = findIndexAtOffset(lo);
  const end = Math.min(meta.length - 1, findIndexAtOffset(hi));
  return { start, end: Math.max(start, end) };
});

const visibleBlocks = computed(() => {
  dataVersion.value; // track
  const { start, end } = visibleRange.value;
  const arr = [];
  for (let i = start; i <= end; i++) {
    const m = meta[i];
    if (!m) continue;
    arr.push({
      index: i,
      top: offsets[i],
      height: (offsets[i + 1] ?? offsets[i]) - offsets[i],
      kind: m.kind,
      start: m.start,
      end: m.end,
      startLine: startLines[i] || (i + 1),
      data: cache.get(i) || m,
    });
  }
  return arr;
});

// Read-only view of block metadata, exposed for backward compatibility.
const blocks = computed(() => meta.map((m, i) => ({ index: i, ...m })));

// ---------------------------------------------------------------------------
// Block data cache (bounded — avoids unbounded memory growth after lots of
// scrolling through a very large document).
// ---------------------------------------------------------------------------

function cacheSet(index, data) {
  if (!cache.has(index)) cacheKeys.push(index);
  cache.set(index, data);
  if (cache.size > MAX_CACHE_ENTRIES) evictCache();
}

function evictCache() {
  const target = Math.floor(MAX_CACHE_ENTRIES * 0.8);
  if (cache.size <= target) return;
  const { start, end } = visibleRange.value;
  const keep = [];
  let iterations = cacheKeys.length;
  while (cacheKeys.length && cache.size > target && iterations-- > 0) {
    const k = cacheKeys.shift();
    if (!cache.has(k)) continue;
    if (k >= start && k <= end) { keep.push(k); continue; }
    cache.delete(k);
  }
  cacheKeys.push(...keep);
}

// ---------------------------------------------------------------------------
// Structural sync: merge new block metadata (after load / chunk-parse /
// edit) while preserving cached data & measured heights for blocks whose
// identity (kind + byte span) is unchanged, or whose span merely shifted by
// a known delta because an earlier block in the document grew/shrank.
// ---------------------------------------------------------------------------

function keyOf(kind, start, end) {
  return `${kind}:${start}:${end}`;
}

function syncBlockState(newMetaRaw, newStartLines) {
  const oldMeta = meta;
  const oldHeights = heights;
  const oldCache = cache;

  let delta = 0;
  if (anchorAfterEdit >= 0 && oldMeta[anchorAfterEdit] && newMetaRaw[anchorAfterEdit]) {
    delta = (newMetaRaw[anchorAfterEdit].end || 0) - (oldMeta[anchorAfterEdit].end || 0);
  }

  const directMap = new Map();
  const shiftedMap = new Map();
  for (let i = 0; i < oldMeta.length; i++) {
    const b = oldMeta[i];
    const h = oldHeights[i];
    const d = oldCache.get(i);
    if (!(h > 0) && !d) continue;
    const key = keyOf(b.kind, b.start, b.end);
    if (!directMap.has(key)) directMap.set(key, { h, d });
    if (anchorAfterEdit >= 0 && i > anchorAfterEdit && delta !== 0) {
      const shiftedKey = keyOf(b.kind, b.start + delta, b.end + delta);
      if (!shiftedMap.has(shiftedKey)) shiftedMap.set(shiftedKey, { h, d });
    }
  }

  const newMeta = newMetaRaw.map(m => ({ kind: m.kind, start: m.start, end: m.end }));
  const newHeights = new Array(newMeta.length).fill(0);
  const newCache = new Map();
  // Use the previous measured height of the edited block as a starting
  // estimate so the following blocks don't jump/shift while it re-renders.
  if (anchorAfterEdit >= 0 && anchorAfterEdit < oldHeights.length) {
    newHeights[anchorAfterEdit] = oldHeights[anchorAfterEdit] || 0;
  }
  let firstChanged = Infinity;
  for (let i = 0; i < newMeta.length; i++) {
    if (i === anchorAfterEdit) { firstChanged = Math.min(firstChanged, i); continue; }
    const key = keyOf(newMeta[i].kind, newMeta[i].start, newMeta[i].end);
    const kept = (anchorAfterEdit >= 0 && i > anchorAfterEdit ? shiftedMap.get(key) : null) || directMap.get(key);
    if (kept) {
      newHeights[i] = kept.h || 0;
      if (kept.d) newCache.set(i, kept.d);
    } else {
      firstChanged = Math.min(firstChanged, i);
    }
  }

  meta = newMeta;
  heights = newHeights;
  cache = newCache;
  cacheKeys = Array.from(newCache.keys());
  if (newStartLines) startLines = newStartLines;
  anchorAfterEdit = -1;
  rebuildOffsets(firstChanged < Infinity ? firstChanged : 0);
  dataVersion.value++;
}

async function fetchLineNumbers(count) {
  if (!count) return [];
  try {
    return await invoke('get_block_line_numbers', { startIndex: 0, count });
  } catch (e) {
    console.error('get_block_line_numbers:', e);
    return [];
  }
}

// ---------------------------------------------------------------------------
// Document loading
// ---------------------------------------------------------------------------

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
    meta = []; heights = []; offsets = [0]; cache = new Map(); cacheKeys = []; startLines = [];
    measuredLineSum = 0; measuredPxSum = 0; measuredIndices = new Set();
    totalHeight.value = 0;
    layoutVersion.value++;
    dataVersion.value++;
    emit('blockCount', 0);
    return;
  }
  const isDocChange = props.doc.id !== lastDocId;
  lastDocId = props.doc.id;
  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  const savedClient = viewport.value ? viewport.value.clientHeight : clientHeight.value;
  // Anchor by the block currently in the middle of the viewport so we can
  // restore approximately the same visual position after re-layout.
  let anchorIndex = -1;
  let anchorOffset = 0;
  if (!isDocChange && meta.length) {
    const mid = savedScroll + savedClient / 2;
    anchorIndex = findIndexAtOffset(mid);
    anchorOffset = mid - (offsets[anchorIndex] || 0);
  }
  layoutReady.value = false;
  editingIndex.value = -1;
  editingHeight.value = 0;
  cache = new Map();
  cacheKeys = [];
  try {
    const rawMeta = await invoke('get_syntax_tree_meta');
    const [off, total] = await invoke('get_parsed_offset');
    parsedOffset.value = off;
    totalLen.value = total;
    hasMoreToParse.value = off < total;
    meta = rawMeta.map(m => ({ kind: m.kind, start: m.start, end: m.end }));
    heights = new Array(meta.length).fill(0);
    measuredLineSum = 0; measuredPxSum = 0; measuredIndices = new Set();
    startLines = await fetchLineNumbers(meta.length);
    rebuildOffsets(0);
    scrollTop.value = isDocChange ? 0 : savedScroll;
    await updateVisibleAndLoad();
    // Wait for the first real height measurement before restoring scroll so
    // the browser doesn't clamp to an underestimated totalHeight.
    await new Promise(r => requestAnimationFrame(r));
    await nextTick();
    measureHeights();
    await nextTick();
    let target = isDocChange ? 0 : savedScroll;
    if (!isDocChange && anchorIndex >= 0 && anchorIndex < meta.length) {
      target = Math.max(0, offsets[anchorIndex] + anchorOffset - savedClient / 2);
    }
    setScrollTop(target);
  } catch (e) {
    console.error('loadDocument:', e);
  }
}

watch(() => [props.doc?.id, currentViewMode.value], loadDocument);

// ---------------------------------------------------------------------------
// Scrolling — rendering of already-cached blocks is instant (driven by the
// cheap `scrollTop` ref + binary search); fetching newly-visible block data
// and lazy chunk-parsing are coalesced to at most once per animation frame
// so fast/flung scrolling never queues up redundant work.
// ---------------------------------------------------------------------------

function onScroll() {
  if (ignoreScroll > 0) { ignoreScroll--; return; }
  if (suppressScroll.value || !viewport.value) return;
  scrollTop.value = viewport.value.scrollTop;
  if (gutter.value) gutter.value.scrollTop = scrollTop.value;
  scheduleVisibleUpdate();
}

function scheduleVisibleUpdate() {
  if (visibleUpdateRaf) return;
  visibleUpdateRaf = requestAnimationFrame(() => {
    visibleUpdateRaf = 0;
    updateVisibleAndLoad();
    maybeParseNextChunk();
  });
}

function setScrollTop(value) {
  if (!viewport.value) return;
  const clamped = Math.max(0, value);
  if (viewport.value.scrollTop === clamped) return;
  ignoreScroll++;
  viewport.value.scrollTop = clamped;
  scrollTop.value = clamped;
  if (gutter.value) gutter.value.scrollTop = clamped;
}

function restoreScroll(savedScroll) {
  if (!viewport.value) return;
  const ch = viewport.value.clientHeight || clientHeight.value;
  let target = savedScroll;
  if (anchorAfterEdit >= 0 && offsets[anchorAfterEdit] != null) {
    target = Math.max(0, offsets[anchorAfterEdit] - ch / 4);
  }
  anchorAfterEdit = -1;
  setScrollTop(target);
}

async function updateVisibleAndLoad() {
  if (loadingVisible) return;
  loadingVisible = true;
  try {
    const { start, end } = visibleRange.value;
    const toLoad = [];
    for (let i = start; i <= end; i++) {
      if (i === editingIndex.value) continue;
      if (cache.has(i) || loadingSet.has(i)) continue;
      toLoad.push(i);
    }
    if (toLoad.length) {
      toLoad.forEach(i => loadingSet.add(i));
      try {
        const results = await Promise.all(toLoad.map(loadBlockData));
        for (const data of results) {
          if (data) cacheSet(data.index, data);
        }
      } finally {
        toLoad.forEach(i => loadingSet.delete(i));
      }
      dataVersion.value++;
    }
    nextTick(() => requestAnimationFrame(measureHeights));
  } finally {
    loadingVisible = false;
  }
}

async function loadBlockData(index) {
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
    if (editingIndex.value === idx) continue;
    const h = el.offsetHeight;
    if (h > 0 && h !== heights[idx]) {
      heights[idx] = h;
      recordMeasuredHeight(idx, h);
      firstChanged = Math.min(firstChanged, idx);
    }
  }
  if (firstChanged < Infinity) rebuildOffsets(firstChanged);
  layoutReady.value = true;
}

function observeBlockHeights() {
  if (!viewport.value || !blockResizeObserver) return;
  blockResizeObserver.disconnect();
  for (const el of viewport.value.querySelectorAll('[data-block-index]')) {
    blockResizeObserver.observe(el);
  }
}

function onBlockResize(entries) {
  if (!viewport.value) return;
  let firstChanged = Infinity;
  for (const entry of entries) {
    const el = entry.target;
    const idx = Number(el.dataset.blockIndex);
    if (Number.isNaN(idx) || idx < 0 || idx >= meta.length) continue;
    if (editingIndex.value === idx) continue;
    const h = Math.round(entry.contentRect.height);
    if (h > 0 && h !== heights[idx]) {
      heights[idx] = h;
      recordMeasuredHeight(idx, h);
      firstChanged = Math.min(firstChanged, idx);
    }
  }
  if (firstChanged < Infinity) rebuildOffsets(firstChanged);
}

async function maybeParseNextChunk() {
  if (!hasMoreToParse.value || chunkParsing.value) return;
  const parsedEnd = totalHeight.value - (hasMoreToParse.value ? clientHeight.value * 2 : 0);
  if (scrollTop.value < parsedEnd) return;
  chunkParsing.value = true;
  try {
    const [newOffset, total] = await invoke('parse_next_chunk');
    parsedOffset.value = newOffset;
    totalLen.value = total;
    hasMoreToParse.value = newOffset < total;
    const rawMeta = await invoke('get_syntax_tree_meta');
    const newStartLines = await fetchLineNumbers(rawMeta.length);
    syncBlockState(rawMeta, newStartLines);
    await updateVisibleAndLoad();
  } catch (e) {
    console.error('parse_next_chunk:', e);
  } finally {
    chunkParsing.value = false;
  }
}

// ---------------------------------------------------------------------------
// Click-to-edit
// ---------------------------------------------------------------------------

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
  if (dx > 3 || dy > 3) return; // was a drag-selection, not a click
  const sel = window.getSelection();
  if (sel && !sel.isCollapsed && sel.toString().trim().length > 0) return;
  e.stopPropagation();
  enterEdit(index);
}

async function enterEdit(index) {
  if (editingIndex.value === index) return;
  if (editingIndex.value >= 0) {
    await exitEdit(true);
  }
  if (index < 0 || index >= meta.length) return;

  let data = cache.get(index);
  if (!data) {
    loadingSet.add(index);
    try {
      data = await loadBlockData(index);
      if (data) cacheSet(index, data);
    } finally {
      loadingSet.delete(index);
    }
  }
  if (!data) return;

  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  editingIndex.value = index;
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
  if (editingIndex.value < 0) return;
  const index = editingIndex.value;
  const old = editOriginal.value;
  const changed = editSource.value !== old;

  if (changed || force) {
    try {
      await invoke('replace_block', { blockIndex: index, newSource: editSource.value });
      const rawMeta = await invoke('get_syntax_tree_meta');
      const newStartLines = await fetchLineNumbers(rawMeta.length);
      anchorAfterEdit = index;
      syncBlockState(rawMeta, newStartLines);
      const data = await invoke('get_block_data', { blockIndex: index });
      if (data) cacheSet(index, data);
      dataVersion.value++;
      if (changed) emit('dirty', true);
    } catch (e) {
      console.error('exitEdit replace:', e);
    }
  }

  const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
  editingIndex.value = -1;
  editingHeight.value = 0;
  editSource.value = '';
  editOriginal.value = '';
  textareaEl = null;
  nextTick(() => {
    updateVisibleAndLoad().then(() => {
      nextTick(() => requestAnimationFrame(() => {
        measureHeights();
        restoreScroll(savedScroll);
      }));
      // Note: no delayed re-measure needed here — blockResizeObserver (see
      // onMounted) picks up any further layout settling (tables/images/fonts)
      // as soon as it happens, without a blind fixed-delay guess.
    });
  });
}

// Ctrl+S is intentionally not handled here: App.vue's global keydown handler
// commits the in-progress edit (via the exposed `save()`) and then persists
// the document to disk. Handling it here too would just call exitEdit twice.
function onKeydown(e) {
  if (e.key === 'Escape') {
    e.preventDefault();
    exitEdit(false);
  } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
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
  if (editingIndex.value >= 0) {
    const oldHeight = editingHeight.value;
    editingHeight.value = el.scrollHeight;
    if (editingHeight.value !== oldHeight) {
      rebuildOffsets(editingIndex.value);
    }
  }
}

function posToLineCol(text, offset) {
  const lines = text.substring(0, offset).split('\n');
  const line = lines.length;
  const col = lines[lines.length - 1].length + 1;
  return { line, col };
}

// Reports the ABSOLUTE document line/column (not just the position within
// the block's local source) so the status bar always matches reality.
function updateCursorFromTextarea() {
  const el = textareaEl;
  if (!el) return;
  const idx = editingIndex.value;
  const base = idx >= 0 ? (startLines[idx] || (idx + 1)) : 1;
  const pos = posToLineCol(editSource.value, el.selectionStart);
  emit('cursor', { line: base - 1 + pos.line, col: pos.col });
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

function getActiveTextarea() {
  if (editingIndex.value < 0) return null;
  if (textareaEl && textareaEl.isConnected) return textareaEl;
  return viewport.value?.querySelector('textarea.md-block-textarea') || null;
}

// ---------------------------------------------------------------------------
// Source-view formatting helpers
// ---------------------------------------------------------------------------

// Apply a pure textEditing.js transform to the source-view textarea, syncing
// the result back into sourceText and restoring the selection. Shares the
// exact same transforms as the block-textarea editor (see applyToTextarea
// above) so source view and block-edit view can never drift out of sync in
// their formatting behavior.
function withSourceText(transform) {
  const ta = sourceTextarea.value;
  if (!ta) return;
  const before = sourceText.value;
  const result = transform(before, ta.selectionStart, ta.selectionEnd);
  sourceText.value = result.value;
  nextTick(() => {
    if (!sourceTextarea.value) return;
    sourceTextarea.value.selectionStart = result.start;
    sourceTextarea.value.selectionEnd = result.end;
    sourceTextarea.value.focus();
    if (result.value !== before) emit('dirty', true);
  });
}

function sourceWrapSelection(prefix, suffix) {
  withSourceText((value, start, end) => textEditing.wrapSelection(value, start, end, prefix, suffix));
}

function sourceLinePrefix(prefix) {
  withSourceText((value, start, end) => textEditing.toggleLinePrefix(value, start, end, prefix));
}

function sourceHeading(level) {
  withSourceText((value, start, end) => textEditing.toggleHeading(value, start, end, level));
}

function sourceInsert(insertion) {
  withSourceText((value, start, end) => textEditing.insertText(value, start, end, insertion));
}

function sourceLink() {
  const url = prompt('URL:', 'https://');
  if (!url) return;
  withSourceText((value, start, end) => textEditing.makeLink(value, start, end, url));
}

function sourceImage() {
  const url = prompt('Image URL:', 'https://');
  if (!url) return;
  withSourceText((value, start, end) => textEditing.makeImage(value, start, end, url));
}

function sourceTable() {
  withSourceText((value, start, end) => textEditing.insertText(value, start, end, textEditing.makeTable()));
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
  let data = cache.get(index);
  if (!data) {
    data = await loadBlockData(index);
    if (data) cacheSet(index, data);
  }
  return data?.source || '';
}

async function replaceBlockSource(index, newSource) {
  try {
    const savedScroll = viewport.value ? viewport.value.scrollTop : scrollTop.value;
    await invoke('replace_block', { blockIndex: index, newSource });
    emit('dirty', true);
    const rawMeta = await invoke('get_syntax_tree_meta');
    const newStartLines = await fetchLineNumbers(rawMeta.length);
    anchorAfterEdit = index;
    syncBlockState(rawMeta, newStartLines);
    const data = await invoke('get_block_data', { blockIndex: index });
    if (data) cacheSet(index, data);
    dataVersion.value++;
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

// Apply a pure textEditing.js transform to a live <textarea>, syncing the
// resulting value/selection back into both the DOM element and editSource.
function applyToTextarea(ta, transform) {
  if (!ta) return;
  const result = transform(ta.value, ta.selectionStart, ta.selectionEnd);
  ta.value = result.value;
  ta.selectionStart = result.start;
  ta.selectionEnd = result.end;
  editSource.value = ta.value;
  nextTick(() => autoSize(ta));
  updateCursorFromTextarea();
}

function toggleWrap(ta, prefix, suffix) {
  applyToTextarea(ta, (value, start, end) => textEditing.wrapSelection(value, start, end, prefix, suffix));
}

function toggleLinePrefix(ta, prefix) {
  applyToTextarea(ta, (value, start, end) => textEditing.toggleLinePrefix(value, start, end, prefix));
}

function toggleHeadingInTextarea(ta, level) {
  applyToTextarea(ta, (value, start, end) => textEditing.toggleHeading(value, start, end, level));
}

function insertLinkInTextarea(ta) {
  if (!ta) return;
  const url = prompt('URL:', 'https://');
  if (!url) return;
  applyToTextarea(ta, (value, start, end) => textEditing.makeLink(value, start, end, url));
}

function insertImageInTextarea(ta) {
  if (!ta) return;
  const url = prompt('Image URL:', 'https://');
  if (!url) return;
  applyToTextarea(ta, (value, start, end) => textEditing.makeImage(value, start, end, url));
}

function insertTableInTextarea(ta) {
  if (!ta) return;
  applyToTextarea(ta, (value, start, end) => textEditing.insertText(value, start, end, textEditing.makeTable()));
}

async function insertThematicBreak() {
  if (currentViewMode.value === 'source') {
    withSourceText((value, start) => textEditing.insertAtLineStart(value, start, '---\n'));
    return;
  }
  const ta = getActiveTextarea();
  if (ta) {
    applyToTextarea(ta, (value, start) => textEditing.insertAtLineStart(value, start, '---\n'));
  } else {
    await invoke('insert_text', { position: totalLen.value || 0, text: '---\n' });
    await loadDocument();
    emit('dirty', true);
  }
}

async function insertCodeBlock() {
  if (currentViewMode.value === 'source') {
    withSourceText((value, start, end) => textEditing.wrapCodeBlock(value, start, end));
    return;
  }
  const ta = getActiveTextarea();
  if (ta) {
    applyToTextarea(ta, textEditing.wrapCodeBlock);
  } else {
    await invoke('insert_text', { position: totalLen.value || 0, text: '```\n\n```\n' });
    await loadDocument();
    emit('dirty', true);
  }
}

async function insertTable() {
  if (currentViewMode.value === 'source') { sourceTable(); return; }
  const ta = getActiveTextarea();
  if (ta) {
    insertTableInTextarea(ta);
    return;
  }
  await invoke('insert_text', { position: totalLen.value || 0, text: textEditing.makeTable() });
  await loadDocument();
  emit('dirty', true);
}

async function insertImage() {
  if (currentViewMode.value === 'source') { sourceImage(); return; }
  const ta = getActiveTextarea();
  if (ta) {
    insertImageInTextarea(ta);
    return;
  }
  const url = prompt('Image URL:', 'https://');
  if (!url) return;
  await invoke('insert_text', { position: totalLen.value || 0, text: `![alt text](${url})` });
  await loadDocument();
  emit('dirty', true);
}

async function applyInlineFormat(prefix, suffix) {
  if (currentViewMode.value === 'source') { sourceWrapSelection(prefix, suffix); return; }
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
  if (currentViewMode.value === 'source') { sourceLinePrefix(prefix); return; }
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
  if (currentViewMode.value === 'source') { sourceHeading(level); return; }
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
  if (currentViewMode.value === 'source') { sourceLink(); return; }
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
  if (currentViewMode.value === 'source') { sourceLinePrefix('- [ ] '); return; }
  await applyLineFormat('- [ ] ');
}

function focusFirst() {
  if (!viewport.value) return;
  viewport.value.focus();
}

onMounted(() => {
  if (viewport.value) {
    clientHeight.value = viewport.value.clientHeight;
    scrollTop.value = viewport.value.scrollTop;
    resizeObserver = new ResizeObserver((entries) => {
      const h = entries[0]?.contentRect?.height || viewport.value?.clientHeight || 0;
      if (h > 0 && h !== clientHeight.value) {
        clientHeight.value = h;
        scheduleVisibleUpdate();
      }
    });
    resizeObserver.observe(viewport.value);
    blockResizeObserver = new ResizeObserver(onBlockResize);
    observeBlockHeights();
  }
  loadDocument();
});

onUpdated(() => {
  observeBlockHeights();
});

onUnmounted(() => {
  if (visibleUpdateRaf) cancelAnimationFrame(visibleUpdateRaf);
  if (resizeObserver) { resizeObserver.disconnect(); resizeObserver = null; }
  if (blockResizeObserver) { blockResizeObserver.disconnect(); blockResizeObserver = null; }
});

function save() {
  if (currentViewMode.value === 'source') {
    return saveSource();
  }
  return exitEdit(false);
}

function setViewMode(mode) {
  currentViewMode.value = mode;
}

const isSourceView = computed(() => currentViewMode.value === 'source');

// ---------------------------------------------------------------------------
// Find / Find & Replace
// ---------------------------------------------------------------------------
// Matching and replacing run entirely on the backend (search_document /
// replace_match_in_document / replace_all_in_document, see main.rs) and only
// ever exchange byte offsets — never the document text — over IPC. This is
// what lets Find work instantly on a 10+ MB document without ever loading it
// whole into the frontend: no more forcing a switch into an unvirtualized
// source-view textarea just to have something to search.
//
// Typing is debounced (150ms) so a fast typist doesn't fire an IPC round
// trip per keystroke, and every request carries a monotonically increasing
// id so a slow/late response can never clobber a newer one.
//
// Live typing (searchSetQuery) NEVER moves focus or the document selection —
// it only updates the "N / M" counter (searchStatus). Only an explicit user
// action — Next/Prev/Enter/F3, or a replace — jumps the cursor into the
// document. Calling `.focus()` from the live-typing path was exactly the bug
// where every keystroke yanked focus out of the find input.

const SEARCH_DEBOUNCE_MS = 150;

const searchStatus = ref({ count: 0, index: -1, valid: true, truncated: false });
watch(searchStatus, (v) => emit('searchStatus', v), { deep: true });
let searchQuery = '';
let searchOptions = { caseSensitive: false, regex: false };
let searchMatches = []; // [{start, end}], byte offsets, ascending order
let searchDebounceTimer = 0;
let searchRequestId = 0;

function scrollMatchIntoView(ta, pos) {
  const linesBefore = ta.value.slice(0, pos).split('\n').length;
  const target = Math.max(0, (linesBefore - 5) * LINE_HEIGHT);
  if (ta.scrollTop !== target) ta.scrollTop = target;
}

// Binary search `meta` (sorted ascending by byte start) for the block
// containing `byteOffset`. Mirrors findIndexAtOffset, but over byte offsets
// instead of pixel offsets.
function findBlockIndexForByteOffset(byteOffset) {
  if (!meta.length) return -1;
  let lo = 0, hi = meta.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (meta[mid].start <= byteOffset) lo = mid; else hi = mid - 1;
  }
  return lo;
}

// Is block `index` within the actual on-screen viewport (scrollTop to
// scrollTop + clientHeight)? This is deliberately *not* the same question as
// "is it in visibleRange" — visibleRange also includes RENDER_BUFFER
// (800px) of pre-rendered content above/below the fold, for smooth
// scrolling. A block can be part of visibleBlocks (rendered in the DOM)
// while still being physically off-screen. Using visibleRange as an
// "already visible" check let the viewport silently drift up to ~800px
// (several blocks) away from a highlighted match before ever correcting —
// exactly the "scroll doesn't follow, then suddenly jumps" symptom.
function isBlockOnScreen(index) {
  const top = offsets[index] ?? 0;
  const bottom = offsets[index + 1] ?? top;
  const viewTop = scrollTop.value;
  const viewBottom = scrollTop.value + clientHeight.value;
  return top < viewBottom && bottom > viewTop;
}

// Scroll block `index` into the *actual visible viewport* (not just the
// virtualized render window, see isBlockOnScreen above) if it isn't there
// already.
//
// `offsets[index]` sums the (possibly estimated) height of every block from
// 0 to `index`. For a block deep in a long stretch the user has never
// scrolled through, most of those heights were never measured — they're
// per-block guesses from estimateHeight(), and guessing error *compounds*
// additively over thousands of blocks. A jump based purely on `offsets`
// only gets locally corrected right around wherever it lands, so it can
// converge to a stable-looking value that's still several blocks off from
// the true position, without any way to detect the discrepancy.
//
// The first jump instead uses estimateScrollForBlock(): a single globally-
// calibrated pixels-per-line ratio (learned from every block actually
// measured so far) applied to the target's *exact* line number. That one
// ratio doesn't compound with distance the way per-block guesses do, so it
// lands far closer on the first try. Once we're in the neighborhood and
// its surroundings get measured, `offsets` becomes locally accurate and
// takes over to fine-tune the remaining pixels.
async function ensureBlockVisible(index) {
  if (!viewport.value) return;
  if (isBlockOnScreen(index)) return; // already on-screen, nothing to do

  let lastOffset = null;
  for (let attempt = 0; attempt < 6; attempt++) {
    const current = attempt === 0 ? estimateScrollForBlock(index) : (offsets[index] ?? 0);
    const target = Math.max(0, current - clientHeight.value / 3);
    setScrollTop(target);
    await updateVisibleAndLoad();
    await nextTick();
    // Give the ResizeObserver one animation frame to report real heights for
    // whatever just rendered before reading `offsets` again.
    await new Promise((r) => requestAnimationFrame(r));
    const settled = lastOffset !== null && Math.abs((offsets[index] ?? 0) - lastOffset) < 2;
    if (isBlockOnScreen(index) && settled) return;
    lastOffset = offsets[index] ?? 0;
  }
}

// Jump the visible cursor/selection to a match, without reloading the whole
// document: in source view, select directly (the full text is already
// loaded, by the user's own choice of view mode); otherwise, enter-edit only
// the one block containing the match (existing lazy per-block loading).
async function navigateToMatch(index) {
  if (index < 0 || index >= searchMatches.length) return;
  const m = searchMatches[index];

  if (currentViewMode.value === 'source') {
    const ta = sourceTextarea.value;
    if (!ta) return;
    ta.focus();
    ta.selectionStart = m.start;
    ta.selectionEnd = m.end;
    scrollMatchIntoView(ta, m.start);
    return;
  }

  const blockIndex = findBlockIndexForByteOffset(m.start);
  if (blockIndex < 0) return;
  if (editingIndex.value !== blockIndex) {
    // enterEdit() only produces a real <textarea> for a block that's
    // currently part of visibleBlocks (the virtualized render window) — it
    // does not scroll anything into view itself (the normal click-to-edit
    // path never needs to, since you can only click a block that's already
    // visible). A match can be anywhere in the document, so bring its block
    // into the viewport *first*; otherwise enterEdit sets editingIndex
    // reactively but no textarea ever mounts, and navigation silently does
    // nothing.
    await ensureBlockVisible(blockIndex);
    await enterEdit(blockIndex);
    await nextTick();
    if (!textareaEl && editingIndex.value === blockIndex) {
      // Block offsets above/around this index may still be estimates (never
      // measured) rather than real heights, so the first scroll target can
      // be off enough to miss the render buffer entirely, especially deep
      // into a large document. Retry once, snapped exactly to this block's
      // (now-recomputed) offset — enterEdit already loaded its data, so this
      // just needs Vue to re-render with it inside visibleBlocks.
      setScrollTop(Math.max(0, offsets[blockIndex] ?? 0));
      await updateVisibleAndLoad();
      await nextTick();
    }
  } else {
    // Two consecutive matches landed in the same block that's already open
    // for editing — re-check on-screen visibility anyway. The block itself
    // hasn't moved, but the user may have manually scrolled the outer page
    // away from it since the last match, or (for an unusually tall block)
    // this match may sit in a part of it that isn't currently on-screen.
    await ensureBlockVisible(blockIndex);
  }
  const ta = textareaEl;
  if (!ta) return;
  const blockStart = meta[blockIndex]?.start ?? 0;
  ta.focus();
  ta.selectionStart = Math.max(0, m.start - blockStart);
  ta.selectionEnd = Math.max(0, m.end - blockStart);
  scrollMatchIntoView(ta, ta.selectionStart);
}

/** Actually run the backend search (no debounce). `requestId` lets a stale response be discarded. */
async function performSearch(query, options, requestId) {
  try {
    const result = await invoke('search_document', {
      args: { query, caseSensitive: !!options.caseSensitive, regex: !!options.regex },
    });
    if (requestId !== searchRequestId) return; // a newer query/clear superseded this one
    if (!result.valid) {
      searchMatches = [];
      searchStatus.value = { count: 0, index: -1, valid: false, truncated: false };
      return;
    }
    searchMatches = result.matches;
    const index = searchMatches.length ? 0 : -1;
    searchStatus.value = { count: searchMatches.length, index, valid: true, truncated: !!result.truncated };
  } catch (e) {
    console.error('search_document:', e);
  }
}

/** Update the query/options driving the live match counter (debounced; never touches focus/selection). */
function searchSetQuery(query, options = {}) {
  searchQuery = query || '';
  searchOptions = { caseSensitive: !!options.caseSensitive, regex: !!options.regex };
  if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
  searchRequestId++;
  if (!searchQuery) {
    searchMatches = [];
    searchStatus.value = { count: 0, index: -1, valid: true, truncated: false };
    return;
  }
  const requestId = searchRequestId;
  searchDebounceTimer = setTimeout(() => performSearch(searchQuery, searchOptions, requestId), SEARCH_DEBOUNCE_MS);
}

async function searchStep(delta) {
  if (!searchMatches.length) return;
  const index = (searchStatus.value.index + delta + searchMatches.length) % searchMatches.length;
  searchStatus.value = { ...searchStatus.value, index };
  await navigateToMatch(index);
}

function searchNext() { return searchStep(1); }
function searchPrev() { return searchStep(-1); }

/** Replace the currently-selected match on the backend, then re-search and advance to the next match. */
async function searchReplaceCurrent(replacement) {
  if (searchStatus.value.index < 0 || !searchMatches.length) return;
  if (editingIndex.value >= 0) await exitEdit(false); // commit any in-progress block edit first
  const m = searchMatches[searchStatus.value.index];
  const wasIndex = searchStatus.value.index;
  try {
    await invoke('replace_match_in_document', {
      args: {
        start: m.start,
        end: m.end,
        query: searchQuery,
        caseSensitive: searchOptions.caseSensitive,
        regex: searchOptions.regex,
        replacement: replacement ?? '',
      },
    });
    emit('dirty', true);
    await loadDocument();
    await performSearch(searchQuery, searchOptions, ++searchRequestId);
    if (searchMatches.length) {
      const nextIndex = Math.min(wasIndex, searchMatches.length - 1);
      searchStatus.value = { ...searchStatus.value, index: nextIndex };
      await navigateToMatch(nextIndex);
    }
  } catch (e) {
    console.error('searchReplaceCurrent:', e);
  }
}

/** Replace every match in one backend transaction (single undo step). */
async function searchReplaceAll(replacement) {
  if (!searchQuery) return;
  if (editingIndex.value >= 0) await exitEdit(false);
  try {
    const result = await invoke('replace_all_in_document', {
      args: {
        query: searchQuery,
        caseSensitive: searchOptions.caseSensitive,
        regex: searchOptions.regex,
        replacement: replacement ?? '',
      },
    });
    if (result.count > 0) emit('dirty', true);
    await loadDocument();
    await performSearch(searchQuery, searchOptions, ++searchRequestId);
  } catch (e) {
    console.error('searchReplaceAll:', e);
  }
}

function searchClear() {
  if (searchDebounceTimer) { clearTimeout(searchDebounceTimer); searchDebounceTimer = 0; }
  searchRequestId++;
  searchQuery = '';
  searchMatches = [];
  searchStatus.value = { count: 0, index: -1, valid: true, truncated: false };
}

defineExpose({
  applyInlineFormat,
  applyLineFormat,
  applyHeading,
  applyLink,
  toggleTask,
  insertThematicBreak,
  insertCodeBlock,
  insertTable,
  insertImage,
  searchSetQuery,
  searchNext,
  searchPrev,
  searchReplaceCurrent,
  searchReplaceAll,
  searchClear,
  searchStatus,
  getActiveTextarea,
  editingBlockIndex: editingIndex,
  blocks,
  focusFirst,
  loadDocument,
  save,
  setViewMode,
  isSourceView,
});
</script>
