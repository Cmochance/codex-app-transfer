<script setup lang="ts">
import TopTabBar from './TopTabBar.vue'
import ToastHost from '@/components/ui/ToastHost.vue'
</script>

<template>
  <div class="app-shell">
    <!-- macOS overlay 标题栏:红绿灯浮在左上,应用名居中,整条可拖拽窗口 -->
    <div class="titlebar" data-tauri-drag-region>
      <span class="titlebar__title">Codex App Transfer</span>
    </div>
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
.titlebar {
  flex-shrink: 0;
  height: 38px;
  display: flex;
  align-items: center;
  justify-content: center;
}
.titlebar__title {
  font-size: var(--fs-md);
  font-weight: 600;
  color: var(--text);
  letter-spacing: -0.01em;
  pointer-events: none;
}
.app-shell__content {
  flex: 1;
  overflow-y: auto;
}
/* 内容填满窗口宽度, 卡片左右仅留 20px 边(窗口宽度由 tauri.conf 控成较窄) */
.app-shell__inner {
  max-width: 1400px;
  margin: 0 auto;
  padding: var(--space-5) 20px var(--space-8);
}
</style>
