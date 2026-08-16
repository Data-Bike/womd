<template>
  <div id="git-panel">
    <h3>Git</h3>
    <pre v-if="statusText" class="git-status">{{ statusText }}</pre>
    <p v-else class="muted">No git repository</p>
  </div>
</template>

<script setup>
import { computed } from 'vue';

const props = defineProps({
  gitData: Object,
});

const statusText = computed(() => {
  const d = props.gitData;
  if (!d || !d.status) return '';
  const parts = [];
  if (d.status.branch) parts.push(`branch: ${d.status.branch}`);
  if (d.status.ahead) parts.push(`ahead ${d.status.ahead}`);
  if (d.status.behind) parts.push(`behind ${d.status.behind}`);
  if (d.status.is_clean) parts.push('clean');
  return parts.join(' | ');
});
</script>
