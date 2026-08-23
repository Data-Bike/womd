<template>
  <div class="modal-overlay" :class="{ hidden: !open }" @click.self="close">
    <div class="modal-dialog" style="width: 420px;">
      <div class="modal-header">
        <span class="modal-title">About WoMD</span>
        <button class="modal-close" @click="close">×</button>
      </div>
      <div class="modal-body" style="text-align: center; padding: 24px;">
        <h1 style="font-size: 24px; margin: 0 0 8px; color: var(--fg);">WoMD</h1>
        <p style="color: var(--fg-muted); margin: 0 0 16px;">Word-like Markdown Editor</p>
        <p style="margin: 6px 0;"><strong>Version:</strong> 0.1.0</p>
        <p style="margin: 6px 0;"><strong>Developer:</strong> Positive Science</p>
        <p style="color: var(--fg-muted); font-size: 12px; margin-top: 16px;">
          A source-preserving, Git-friendly Markdown editor with a Rust core.
        </p>
      </div>
    </div>
  </div>
</template>

<script setup>
const props = defineProps({ open: Boolean });
const emit = defineEmits(['update:open']);

function close() {
  emit('update:open', false);
}
</script>
