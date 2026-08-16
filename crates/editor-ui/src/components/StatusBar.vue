<template>
  <div id="status-bar">
    <span v-if="fileName">{{ fileName }}{{ isDirty ? ' *' : '' }}</span>
    <span v-else class="muted">No file</span>
  </div>
</template>

<script setup>
defineProps({ fileName: String, isDirty: Boolean });
</script>
