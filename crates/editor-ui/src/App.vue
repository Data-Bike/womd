<template>
  <div id="app" data-theme="mocha">
    <ToolBar
      :file-name="fileName"
      :is-dirty="isDirty"
      @open="openFile"
      @save="saveFile"
    />
    <div id="main-area">
      <MarkdownEditor v-if="doc" :doc="doc" @dirty="isDirty = $event" @save="saveFile" />
      <div v-else class="welcome">
        <h1>WoMD</h1>
        <p>Vue rewrite in progress. Open test_large.md to test.</p>
        <button @click="openFile">Open test_large.md</button>
      </div>
    </div>
    <StatusBar :file-name="fileName" :is-dirty="isDirty" />
  </div>
</template>

<script setup>
import { ref, onMounted } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import MarkdownEditor from './components/MarkdownEditor.vue';
import ToolBar from './components/ToolBar.vue';
import StatusBar from './components/StatusBar.vue';

const doc = ref(null);
const fileName = ref('');
const isDirty = ref(false);

async function openFile() {
  // For now, hardcode the test file relative to the repo root.
  const path = 'C:/Users/gorod/RustroverProjects/womd/test_large.md';
  try {
    const info = await invoke('open_document', { path });
    doc.value = { id: info.tab_id, name: info.file_name, path };
    fileName.value = doc.value.name;
    isDirty.value = false;
  } catch (e) {
    console.error('openFile:', e);
  }
}

async function saveFile() {
  if (!doc.value) return;
  try {
    await invoke('save_document', { path: doc.value.path });
    isDirty.value = false;
  } catch (e) {
    console.error('saveFile:', e);
  }
}

onMounted(openFile);
</script>
