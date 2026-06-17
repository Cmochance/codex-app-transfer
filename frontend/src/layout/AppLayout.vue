<script setup lang="ts">
import TopTabBar from './TopTabBar.vue'
import ToastHost from '@/components/ui/ToastHost.vue'
</script>

<template>
  <div class="app-shell">
    <TopTabBar />
    <main class="app-shell__content">
      <div class="app-shell__inner">
        <RouterView />
      </div>
    </main>
    <ToastHost />
  </div>
</template>

<style scoped>
.app-shell {
  display: flex;
  flex-direction: column;
  height: 100vh;
  overflow: hidden;
  background: var(--bg);
}
.app-shell__content {
  flex: 1;
  overflow-y: auto;
}
/* FineTune 风: 内容居中 + 慷慨留白 */
.app-shell__inner {
  max-width: 720px;
  margin: 0 auto;
  padding: var(--space-6) var(--space-5) var(--space-8);
}
</style>
