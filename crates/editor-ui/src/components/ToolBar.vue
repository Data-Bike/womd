<template>
  <div id="toolbar">
    <button @click="$emit('open')">Open test_large.md</button>
    <button @click="$emit('save')">Save</button>
    <span class="file-name">{{ fileName || 'No file' }}</span>
    <span v-if="isDirty" class="dirty">*</span>
  </div>
</template>

<script setup>
defineProps({ fileName: String, isDirty: Boolean });
defineEmits(['open', 'save']);
</script>
