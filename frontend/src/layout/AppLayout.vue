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
/* content 不滚、固定高(flex:1 of 100vh shell)+ flex 列,把固定高传给 inner */
.app-shell__content {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
/* inner 拿到固定高(flex:1 of content)→「单主框」页面(用量表/会话列表/文档预览)
   能 flex:1 框内滚、底部间隙恒定 = padding-bottom;长页(设置/提供商)则由 inner 自身
   overflow-y:auto 整页滚。关键:必须是 flex:1 固定高,不能用 min-height:100%(那只是下限,
   内容一多就跟着长高、约束失效 → 表格/列表撑出窗口)。 */
.app-shell__inner {
  max-width: 1400px;
  margin: 0 auto;
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow-y: auto;
  padding: var(--space-5) 20px var(--space-8);
}
</style>
