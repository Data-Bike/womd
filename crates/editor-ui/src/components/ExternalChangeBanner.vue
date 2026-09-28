<template>
  <div class="external-banner">
    <span class="external-banner-icon">⚠</span>
    <span class="external-banner-text">
      <template v-if="conflict.kind === 'conflict'">
        <strong>{{ conflict.fileName }}</strong> was changed on disk{{
          conflict.fromMcp ? ' by an AI agent (MCP)' : ''
        }}<template v-if="conflict.versionSha">
          and saved as version <code class="version-sha">{{ shortSha }}</code><template
            v-if="conflict.versionMessage"
          >
            — “{{ conflict.versionMessage }}”</template
          ></template
        >. Your unsaved edits are still here — choose which version to keep.
      </template>
      <template v-else-if="conflict.kind === 'removed'">
        <strong>{{ conflict.fileName }}</strong> was deleted on disk. Your edits are
        still here — saving recreates the file.
      </template>
      <template v-else-if="conflict.kind === 'error'">
        <strong>{{ conflict.fileName }}</strong> changed on disk but could not be
        reloaded yet (the file may be temporarily locked). Retrying automatically…
      </template>
      <template v-else>
        <strong>{{ conflict.fileName }}</strong> was updated on disk{{
          conflict.fromMcp ? ' by an AI agent (MCP)' : ''
        }}<template v-if="conflict.versionSha">
          — version <code class="version-sha">{{ shortSha }}</code></template
        >.
      </template>
    </span>
    <span class="external-banner-actions">
      <button v-if="conflict.versionSha" @click="$emit('view-diff')">View changes</button>
      <template v-if="conflict.kind === 'conflict'">
        <button class="primary" @click="$emit('reload')">Load saved version</button>
        <button @click="$emit('dismiss')">Keep my edits</button>
      </template>
      <button v-else @click="$emit('dismiss')">Dismiss</button>
    </span>
  </div>
</template>

<script setup>
import { computed } from 'vue';

const props = defineProps({
  // { fileName, kind: 'conflict'|'reloaded'|'removed', versionSha, versionMessage, fromMcp }
  conflict: { type: Object, required: true },
});
defineEmits(['reload', 'view-diff', 'dismiss']);

const shortSha = computed(() => props.conflict.versionSha?.slice(0, 8) || '');
</script>
