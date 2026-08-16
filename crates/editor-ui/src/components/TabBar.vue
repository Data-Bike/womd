<template>
  <div id="tab-bar">
    <div id="tab-list">
      <div
        v-for="(tab, i) in tabs"
        :key="tab.id"
        class="tab"
        :class="{ active: i === active }"
        @click="$emit('switch', i)"
      >
        {{ tab.name }}{{ tab.dirty ? ' •' : '' }}
      </div>
    </div>
    <button id="tab-new" @click="$emit('new')" title="New tab">+</button>
  </div>
</template>

<script setup>
defineProps({
  tabs: Array,
  active: Number,
});
defineEmits(['new', 'switch']);
</script>
