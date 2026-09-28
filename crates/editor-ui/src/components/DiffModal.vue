<template>
  <div class="modal-overlay" :class="{ hidden: !open }" @click.self="close">
    <div class="modal-dialog" style="width: 760px">
      <div class="modal-header">
        <span class="modal-title">{{ title }}</span>
        <button class="modal-close" @click="close">×</button>
      </div>
      <div class="modal-body">
        <pre class="diff-view">{{ diff }}</pre>
      </div>
    </div>
  </div>
</template>

<script setup>
const props = defineProps({
  open: Boolean,
  title: { type: String, default: 'Changes' },
  diff: { type: String, default: '' },
});
const emit = defineEmits(['update:open']);

function close() {
  emit('update:open', false);
}
</script>
