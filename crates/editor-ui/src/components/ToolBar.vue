<template>
  <div id="toolbar" @mousedown="onToolbarMouseDown">
    <button id="btn-new" title="New document" @click="$emit('new')">New</button>
    <button id="btn-open" title="Open file" @click="$emit('open')">Open</button>
    <button id="btn-save" title="Save" @click="$emit('save')">Save</button>
    <span class="separator"></span>
    <button id="btn-undo" title="Undo" @click="$emit('undo')">Undo</button>
    <button id="btn-redo" title="Redo" @click="$emit('redo')">Redo</button>
    <span class="separator"></span>
    <select id="sel-heading" v-model="headingModel" title="Heading" @change="onHeading">
      <option value="0">Normal text</option>
      <option value="1">Heading 1</option>
      <option value="2">Heading 2</option>
      <option value="3">Heading 3</option>
      <option value="4">Heading 4</option>
      <option value="5">Heading 5</option>
      <option value="6">Heading 6</option>
    </select>
    <span class="separator"></span>
    <button id="btn-bold" title="Bold" style="font-weight:bold" @click="$emit('format-inline', '**', '**')">B</button>
    <button id="btn-italic" title="Italic" style="font-style:italic" @click="$emit('format-inline', '*', '*')">I</button>
    <button id="btn-strike" title="Strikethrough" style="text-decoration:line-through" @click="$emit('format-inline', '~~', '~~')">S</button>
    <button id="btn-code" title="Inline code" style="font-family:monospace" @click="$emit('format-inline', '`', '`')">&lt;/&gt;</button>
    <button id="btn-link" title="Link" @click="$emit('format-link')">Link</button>
    <span class="separator"></span>
    <button id="btn-ul" title="Unordered list" @click="$emit('format-line', '- ')">• List</button>
    <button id="btn-ol" title="Ordered list" @click="$emit('format-line', '1. ')">1. List</button>
    <button id="btn-quote" title="Quote" @click="$emit('format-line', '> ')">❝</button>
    <button id="btn-hr" title="Thematic break" @click="$emit('insert-hr')">―</button>
    <button id="btn-codeblock" title="Code block" @click="$emit('insert-codeblock')">{ }</button>
    <span class="separator"></span>
    <button id="btn-task" title="Task list" @click="$emit('toggle-task')">☐</button>
    <span class="separator"></span>
    <button id="btn-tree-toggle" title="Toggle file tree" :class="{ active: fileTreeVisible }" @click="$emit('toggle-tree')">☰</button>
    <button id="btn-view-toggle" title="Toggle source view" :class="{ active: viewMode === 'source' }" @click="$emit('toggle-view')">MD</button>
    <button id="btn-git" title="Toggle Git panel" :class="{ active: gitVisible }" @click="$emit('toggle-git')">Git</button>
    <span class="separator"></span>
    <span id="file-name">{{ fileName || 'untitled.md' }}</span>
    <span id="dirty-indicator" class="dirty-dot" :class="{ active: isDirty }"></span>
    <span class="spacer"></span>
    <span id="git-branch-info">{{ branchInfo }}</span>
    <span class="separator"></span>
    <button id="btn-settings" title="Settings" @click="$emit('settings')">⚙</button>
  </div>
</template>

<script setup>
import { ref } from 'vue';

defineProps({
  fileName: String,
  isDirty: Boolean,
  branchInfo: String,
  gitVisible: Boolean,
  fileTreeVisible: Boolean,
  viewMode: { type: String, default: 'rendered' },
});

const headingModel = ref('0');

const emit = defineEmits(['new', 'open', 'save', 'undo', 'redo', 'heading', 'format-inline', 'format-line', 'format-link', 'insert-hr', 'insert-codeblock', 'toggle-task', 'toggle-tree', 'toggle-view', 'toggle-git', 'settings']);

function onHeading() {
  const level = parseInt(headingModel.value, 10);
  headingModel.value = '0';
  emit('heading', level);
}

function onToolbarMouseDown(e) {
  // Buttons must not steal DOM focus from a block-edit <textarea>: its
  // blur handler commits the edit and unmounts it, so the click would
  // apply formatting to a textarea that no longer exists. <select> is
  // exempt — preventing its mousedown would block the dropdown (its blur
  // is kept alive via relatedTarget in MarkdownEditor instead).
  if (e.target.tagName !== 'SELECT') e.preventDefault();
}
</script>
