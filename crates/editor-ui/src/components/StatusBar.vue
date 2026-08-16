<template>
  <div id="status-bar">
    <span id="cursor-pos">{{ cursorPos }}</span>
    <span class="spacer"></span>
    <span id="block-count">{{ blockCount }} blocks</span>
    <span class="separator"></span>
    <span id="autosave-status">{{ autosave ? 'Autosave: On' : 'Autosave: Off' }}</span>
    <span class="separator"></span>
    <span id="encoding">{{ encoding }}</span>
    <span class="separator"></span>
    <span id="line-ending">{{ lineEnding }}</span>
  </div>
</template>

<script setup>
defineProps({
  cursorPos: { type: String, default: 'Ln 1, Col 1' },
  blockCount: { type: Number, default: 0 },
  autosave: { type: Boolean, default: true },
  encoding: { type: String, default: 'UTF-8' },
  lineEnding: { type: String, default: 'LF' },
});
</script>
