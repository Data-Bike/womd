<template>
  <div id="tab-bar">
    <div id="tab-list">
      <div
        v-for="tab in tabs"
        :key="tab.id"
        class="tab"
        :class="{ active: activeTabId === tab.id }"
        @click="$emit('switch', tab.id)"
      >
        <span class="tab-name">{{ tab.name }}</span>
        <span v-if="tab.dirty" class="tab-dirty"></span>
        <span class="tab-close" @click.stop="$emit('close', tab.id)">×</span>
      </div>
    </div>
    <button id="tab-new" title="New tab" @click="$emit('new')">+</button>
  </div>
</template>

<script setup>
defineProps({
  tabs: Array,
  activeTabId: [Number, String],
});
defineEmits(['new', 'switch', 'close']);
</script>
