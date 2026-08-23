<template>
  <div v-if="show" class="find-bar" @keydown.esc="close">
    <div class="find-row">
      <input
        ref="queryInput"
        v-model="query"
        class="find-input"
        placeholder="Find"
        @keydown.enter.exact.prevent="onEnter"
        @keydown.shift.enter.exact.prevent="emitPrev"
      />
      <span class="find-count" :class="{ invalid: !status.valid, empty: status.valid && status.count === 0 }">
        {{ countLabel }}
      </span>
      <button class="find-btn" title="Previous match (Shift+Enter)" :disabled="!status.count" @click="emitPrev">↑</button>
      <button class="find-btn" title="Next match (Enter)" :disabled="!status.count" @click="emitNext">↓</button>
      <button class="find-btn toggle" :class="{ active: caseSensitive }" title="Match case" @click="caseSensitive = !caseSensitive">Aa</button>
      <button class="find-btn toggle" :class="{ active: useRegex }" title="Use regular expression" @click="useRegex = !useRegex">.*</button>
      <button class="find-btn toggle" :class="{ active: showReplace }" title="Toggle replace" @click="showReplace = !showReplace">⇄</button>
      <button class="find-btn close" title="Close (Esc)" @click="close">×</button>
    </div>
    <div v-if="showReplace" class="find-row">
      <input
        v-model="replacement"
        class="find-input"
        placeholder="Replace"
        @keydown.enter.exact.prevent="emitReplace"
      />
      <button class="find-btn" :disabled="!status.count" @click="emitReplace">Replace</button>
      <button class="find-btn" :disabled="!status.count" @click="emitReplaceAll">Replace All</button>
    </div>
  </div>
</template>

<script setup>
import { ref, computed, watch, nextTick } from 'vue';

const props = defineProps({
  show: Boolean,
  status: { type: Object, default: () => ({ count: 0, index: -1, valid: true }) },
});

const emit = defineEmits(['search', 'next', 'prev', 'replace', 'replace-all', 'close']);

const query = ref('');
const replacement = ref('');
const caseSensitive = ref(false);
const useRegex = ref(false);
const showReplace = ref(false);
const queryInput = ref(null);

function runSearch() {
  emit('search', { query: query.value, caseSensitive: caseSensitive.value, regex: useRegex.value });
}

// Re-run the search whenever the query text or either option changes —
// this is what drives the live "N / M" match counter.
watch([query, caseSensitive, useRegex], runSearch);

const countLabel = computed(() => {
  if (!query.value) return '';
  if (!props.status.valid) return 'Invalid regex';
  if (props.status.count === 0) return 'No results';
  return `${props.status.index + 1} / ${props.status.count}`;
});

function emitNext() { emit('next'); }
function emitPrev() { emit('prev'); }
function emitReplace() { emit('replace', replacement.value); }
function emitReplaceAll() { emit('replace-all', replacement.value); }
function onEnter() { if (query.value) emitNext(); }
function close() { emit('close'); }

// Called imperatively by App.vue when opening the bar (Ctrl+F / Ctrl+H, or
// the Find/Find-and-Replace menu items) so the input is always focused —
// and, for Ctrl+H, the replace row is revealed immediately. Query/options
// intentionally persist across opens/closes (this component stays mounted;
// only its `show` prop toggles) so re-opening Find repeats the last search.
function focus(withReplace = false) {
  if (withReplace) showReplace.value = true;
  nextTick(() => {
    queryInput.value?.focus();
    queryInput.value?.select();
  });
  if (query.value) runSearch();
}

defineExpose({ focus, next: emitNext, prev: emitPrev });
</script>

<style scoped>
.find-bar {
  position: fixed;
  top: 40px;
  right: 16px;
  z-index: 1500;
  background: var(--surface-0, #1e1e2e);
  border: 1px solid var(--surface-1, #313244);
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  border-radius: 6px;
  padding: 6px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  font: 13px system-ui, -apple-system, sans-serif;
}

.find-row {
  display: flex;
  align-items: center;
  gap: 4px;
}

.find-input {
  width: 200px;
  background: var(--surface-1, #313244);
  border: 1px solid var(--surface-2, #45475a);
  color: var(--fg, #cdd6f4);
  border-radius: 4px;
  padding: 4px 6px;
  font: inherit;
}

.find-input:focus {
  outline: 1px solid var(--accent, #89b4fa);
}

.find-count {
  min-width: 72px;
  text-align: center;
  color: var(--fg-muted, #a6adc8);
  font-size: 12px;
}

.find-count.empty {
  color: var(--close, #f38ba8);
}

.find-count.invalid {
  color: var(--close, #f38ba8);
  font-style: italic;
}

.find-btn {
  background: var(--surface-1, #313244);
  border: 1px solid var(--surface-2, #45475a);
  color: var(--fg, #cdd6f4);
  border-radius: 4px;
  padding: 3px 8px;
  cursor: pointer;
  font: inherit;
}

.find-btn:hover:not(:disabled) {
  background: var(--surface-2, #45475a);
}

.find-btn:disabled {
  opacity: 0.4;
  cursor: default;
}

.find-btn.toggle.active {
  background: var(--accent, #89b4fa);
  color: var(--surface-0, #1e1e2e);
  border-color: var(--accent, #89b4fa);
}

.find-btn.close {
  font-weight: bold;
}
</style>
