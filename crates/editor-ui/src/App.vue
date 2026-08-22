<template>
  <div id="app" data-theme="mocha" @contextmenu.prevent="onAppContextMenu">
    <MenuBar
      @new="newDoc"
      @open="openFileDialog"
      @save="saveFile"
      @save-as="saveFile"
      @close-window="closeWindow"
      @about="about"
      @undo="undo"
      @redo="redo"
      @cut="cut"
      @copy="copy"
      @paste="paste"
      @select-all="selectAll"
      @find="findText"
      @find-replace="findReplace"
      @view-rendered="viewMode = 'rendered'"
      @view-source="viewMode = 'source'"
      @toggle-tree="toggleTree"
      @toggle-git="toggleGit"
      @heading="applyHeading"
      @bold="() => applyInlineFormat('**')"
      @italic="() => applyInlineFormat('*')"
      @strikethrough="() => applyInlineFormat('~~')"
      @code="() => applyInlineFormat('`')"
      @link="applyLink"
      @unordered-list="() => applyLineFormat('- ')"
      @ordered-list="() => applyLineFormat('1. ')"
      @task-list="() => applyLineFormat('- [ ] ')"
      @quote="() => applyLineFormat('> ')"
      @hr="insertHr"
      @code-block="insertCodeBlock"
      @settings="settingsOpen = true"
    />
    <ContextMenu
      :show="ctxShow"
      :x="ctxX"
      :y="ctxY"
      @close="ctxShow = false"
      @undo="undo"
      @redo="redo"
      @cut="cut"
      @copy="copy"
      @paste="paste"
      @select-all="selectAll"
      @bold="() => applyInlineFormat('**')"
      @italic="() => applyInlineFormat('*')"
      @strikethrough="() => applyInlineFormat('~~')"
      @code="() => applyInlineFormat('`')"
      @link="applyLink"
      @heading="applyHeading"
      @unordered-list="() => applyLineFormat('- ')"
      @ordered-list="() => applyLineFormat('1. ')"
      @task-list="() => applyLineFormat('- [ ] ')"
      @quote="() => applyLineFormat('> ')"
      @hr="insertHr"
      @code-block="insertCodeBlock"
      @view-rendered="viewMode = 'rendered'"
      @view-source="viewMode = 'source'"
      @toggle-tree="toggleTree"
      @toggle-git="toggleGit"
      @settings="settingsOpen = true"
    />
    <TabBar
      :tabs="tabs"
      :active-tab-id="activeTabId"
      @new="newDoc"
      @switch="switchTab"
      @close="closeTab"
    />
    <ToolBar
      :file-name="activeFileName"
      :is-dirty="activeIsDirty"
      :branch-info="branchInfo"
      :git-visible="gitVisible"
      :file-tree-visible="fileTreeVisible"
      :view-mode="viewMode"
      @new="newDoc"
      @open="openFileDialog"
      @save="saveFile"
      @undo="undo"
      @redo="redo"
      @heading="applyHeading"
      @format-inline="applyInlineFormat"
      @format-line="applyLineFormat"
      @format-link="applyLink"
      @insert-hr="insertHr"
      @insert-codeblock="insertCodeBlock"
      @toggle-task="toggleTask"
      @toggle-tree="toggleTree"
      @toggle-view="toggleView"
      @toggle-git="toggleGit"
      @settings="settingsOpen = true"
    />
    <div id="main-area">
      <FileTree
        :visible="fileTreeVisible"
        :root="fileTreeRoot"
        :width="fileTreeWidth"
        @close="fileTreeVisible = false"
        @open="openFile"
        @update:root="fileTreeRoot = $event"
      />
      <div
        id="file-tree-divider"
        :class="{ hidden: !fileTreeVisible }"
        @mousedown="startTreeResize"
      ></div>

      <MarkdownEditor
        v-if="activeDoc"
        ref="editorRef"
        :doc="activeDoc"
        :view-mode="viewMode"
        @dirty="onDirty"
        @cursor="cursorPos = $event"
        @block-count="blockCount = $event"
      />
      <div v-else id="editor-container" class="welcome">
        <div class="md-block-placeholder" style="padding:24px;color:var(--fg-muted);text-align:center">
          <h1>WoMD</h1>
          <p>Open a file to start editing.</p>
          <button @click="openFileDialog">Open test_large.md</button>
        </div>
      </div>

      <div
        id="git-divider"
        :class="{ hidden: !gitVisible }"
        @mousedown="startGitResize"
      ></div>
      <GitPanel
        :visible="gitVisible"
        :active-file="activeFilePath"
        :width="gitWidth"
      />
    </div>

    <SettingsModal v-model:open="settingsOpen" />

    <StatusBar
      :cursor-pos="cursorText"
      :block-count="blockCount"
      :autosave="autosave"
      :encoding="encoding"
      :line-ending="lineEnding"
    />
  </div>
</template>

<script setup>
import { ref, computed, watch, onMounted, onUnmounted, nextTick } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import MarkdownEditor from './components/MarkdownEditor.vue';
import ToolBar from './components/ToolBar.vue';
import StatusBar from './components/StatusBar.vue';
import TabBar from './components/TabBar.vue';
import GitPanel from './components/GitPanel.vue';
import FileTree from './components/FileTree.vue';
import SettingsModal from './components/SettingsModal.vue';
import MenuBar from './components/MenuBar.vue';
import ContextMenu from './components/ContextMenu.vue';

const tabs = ref([]);
const activeTabId = ref(0);
const fileTreeVisible = ref(false);
const fileTreeRoot = ref('');
const fileTreeWidth = ref(260);
const gitVisible = ref(false);
const gitWidth = ref(380);
const settingsOpen = ref(false);
const viewMode = ref('rendered');
const branchInfo = ref('');
const cursorPos = ref({ line: 1, col: 1 });
const blockCount = ref(0);
const autosave = ref(true);
const encoding = ref('UTF-8');
const lineEnding = ref('LF');
const editorRef = ref(null);
const ctxShow = ref(false);
const ctxX = ref(0);
const ctxY = ref(0);

let autosaveTimer = null;
let resizeHandler = null;

const activeTab = computed(() => tabs.value.find(t => t.id === activeTabId.value) || null);
const activeDoc = computed(() => {
  const t = activeTab.value;
  if (!t) return null;
  return { id: t.id, name: t.name, path: t.path };
});
const activeFileName = computed(() => activeTab.value?.name || 'untitled.md');
const activeFilePath = computed(() => activeTab.value?.path || '');
const activeIsDirty = computed(() => activeTab.value?.dirty || false);
const cursorText = computed(() => `Ln ${cursorPos.value.line}, Col ${cursorPos.value.col}`);

function parentDir(p) {
  if (!p) return '';
  const sep = p.includes('/') ? '/' : '\\';
  const parts = p.split(sep);
  parts.pop();
  return parts.join(sep);
}

async function refreshTabs() {
  try {
    const list = await invoke('get_tabs');
    tabs.value = list.tabs.map(t => ({
      id: t.id,
      name: t.file_name,
      path: t.file_path,
      dirty: t.is_dirty,
    }));
    activeTabId.value = list.active;
    updateBranchInfo();
    if (activeFilePath.value && !fileTreeRoot.value) {
      fileTreeRoot.value = parentDir(activeFilePath.value);
    }
  } catch (e) {
    console.error('refreshTabs:', e);
  }
}

async function updateBranchInfo() {
  try {
    const status = await invoke('get_git_status');
    branchInfo.value = status.branch ? `branch: ${status.branch}` : '';
  } catch (e) {
    branchInfo.value = '';
  }
}

function onDirty() {
  const t = activeTab.value;
  if (t) t.dirty = true;
}

async function newDoc() {
  try {
    await invoke('new_document');
    await refreshTabs();
  } catch (e) {
    console.error('newDoc:', e);
  }
}

async function openFileDialog() {
  try {
    const selected = await open({ multiple: false });
    if (!selected) return;
    const path = Array.isArray(selected) ? selected[0] : selected;
    await openFile(path);
  } catch (e) {
    console.error('openFileDialog:', e);
  }
}

async function openFile(path) {
  if (!path) return;
  try {
    await invoke('open_document', { path });
    await refreshTabs();
  } catch (e) {
    console.error('openFile:', e);
    alert('Failed to open: ' + e);
  }
}

async function saveFile() {
  if (!activeDoc.value) return;
  try {
    if (editorRef.value?.save) {
      await editorRef.value.save();
    }
    await invoke('save_document', { path: activeFilePath.value || null });
    const t = activeTab.value;
    if (t) t.dirty = false;
    await refreshTabs();
  } catch (e) {
    console.error('saveFile:', e);
  }
}

async function undo() {
  try {
    await invoke('undo');
    await editorRef.value?.loadDocument?.();
    await refreshTabs();
  } catch (e) {
    console.error('undo:', e);
  }
}

async function redo() {
  try {
    await invoke('redo');
    await editorRef.value?.loadDocument?.();
    await refreshTabs();
  } catch (e) {
    console.error('redo:', e);
  }
}

async function switchTab(tabId) {
  try {
    await invoke('switch_tab', { tabId });
    await refreshTabs();
  } catch (e) {
    console.error('switchTab:', e);
  }
}

async function closeTab(tabId) {
  try {
    await invoke('close_tab', { tabId });
    await refreshTabs();
  } catch (e) {
    console.error('closeTab:', e);
  }
}

async function applyHeading(level) {
  await editorRef.value?.applyHeading?.(level);
}
async function applyInlineFormat(prefix, suffix) {
  await editorRef.value?.applyInlineFormat?.(prefix, suffix || prefix);
}
async function applyLineFormat(prefix) {
  await editorRef.value?.applyLineFormat?.(prefix);
}
async function applyLink() {
  await editorRef.value?.applyLink?.();
}
function insertHr() {
  editorRef.value?.insertThematicBreak?.();
}
function insertCodeBlock() {
  editorRef.value?.insertCodeBlock?.();
}
function toggleTask() {
  editorRef.value?.toggleTask?.();
}
function toggleTree() {
  fileTreeVisible.value = !fileTreeVisible.value;
}
function toggleGit() {
  gitVisible.value = !gitVisible.value;
}
function toggleView() {
  viewMode.value = viewMode.value === 'rendered' ? 'source' : 'rendered';
}

function onAppContextMenu(e) {
  ctxX.value = e.clientX;
  ctxY.value = e.clientY;
  ctxShow.value = true;
}

function cut() { document.execCommand('cut'); }
function copy() { document.execCommand('copy'); }
function paste() { document.execCommand('paste'); }
function selectAll() { document.execCommand('selectAll'); }
function findText() { window.find?.('', false, false, true, false, true, false); }
function findReplace() { alert('Find and Replace is not yet implemented.'); }
function closeWindow() { window.close?.(); }
function about() { alert('WoMD — Markdown Editor\nVersion 0.1.0'); }

function detectLineEnding(text) {
  if (text.includes('\r\n')) return 'CRLF';
  if (text.includes('\r')) return 'CR';
  return 'LF';
}

async function updateLineEnding() {
  if (!activeDoc.value) {
    lineEnding.value = 'LF';
    return;
  }
  try {
    const [, total] = await invoke('get_parsed_offset');
    if (total > 100_000) {
      lineEnding.value = 'LF';
      return;
    }
    const text = await invoke('get_document_text');
    lineEnding.value = detectLineEnding(text.substring(0, 4096));
  } catch (e) {
    lineEnding.value = 'LF';
  }
}

watch(activeFilePath, () => {
  fileTreeRoot.value = parentDir(activeFilePath.value);
  updateLineEnding();
});

// Resizable dividers
function startTreeResize(e) {
  e.preventDefault();
  const startX = e.clientX;
  const startWidth = fileTreeWidth.value;
  function onMove(ev) {
    fileTreeWidth.value = Math.max(180, Math.min(500, startWidth + ev.clientX - startX));
  }
  function onUp() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function startGitResize(e) {
  e.preventDefault();
  const startX = e.clientX;
  const startWidth = gitWidth.value;
  function onMove(ev) {
    // Git panel is on the right; dragging the divider left makes it wider.
    gitWidth.value = Math.max(250, Math.min(800, startWidth + startX - ev.clientX));
  }
  function onUp() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

// Autosave
function scheduleAutosave() {
  if (!autosave.value) return;
  if (autosaveTimer) clearTimeout(autosaveTimer);
  autosaveTimer = setTimeout(async () => {
    if (activeIsDirty.value && activeFilePath.value) {
      try { await saveFile(); } catch (e) { console.error('autosave:', e); }
    }
  }, 30000);
}

watch(activeIsDirty, (dirty) => { if (dirty) scheduleAutosave(); });

onMounted(async () => {
  await refreshTabs();
  if (!tabs.value.length) {
    const defaultPath = 'C:/Users/gorod/RustroverProjects/womd/test_large.md';
    try { await openFile(defaultPath); } catch (e) { console.error('default open:', e); }
  }
});

onUnmounted(() => {
  if (autosaveTimer) clearTimeout(autosaveTimer);
  if (resizeHandler) window.removeEventListener('resize', resizeHandler);
});
</script>
