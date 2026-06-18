<script setup lang="ts">
import { ref } from 'vue'

// Stage 1 hello world：验证 SFC 预编译、scoped style、响应式在 cas:// + CSP 下工作。
const ready = ref('Vue 3 + Vite + TypeScript 脚手架已就绪')
const transport = ref('cas://localhost/  ·  CSP: script-src \'self\'')
</script>

<template>
  <main class="scaffold">
    <div class="badge">Stage 1</div>
    <h1>{{ ready }}</h1>
    <p class="transport">{{ transport }}</p>
    <p class="hint">脚手架 + 构建链打通 · 下一步接入设计系统与三套主题</p>
  </main>
</template>

<style scoped>
.scaffold {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  min-height: 100vh;
  gap: 10px;
  font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Segoe UI',
    'Microsoft YaHei UI', system-ui, sans-serif;
  color: #1d1d1f;
  background: #f5f5f7;
  -webkit-font-smoothing: antialiased;
}
.badge {
  font-size: 12px;
  font-weight: 600;
  color: #0a84ff;
  background: #e8f1ff;
  padding: 3px 10px;
  border-radius: 9999px;
}
h1 {
  font-size: 20px;
  font-weight: 600;
  margin: 4px 0 0;
}
.transport {
  font-family: ui-monospace, 'SF Mono', Menlo, monospace;
  font-size: 13px;
  color: #6e6e73;
  margin: 0;
}
.hint {
  font-size: 13px;
  color: #98989d;
  margin: 0;
}
</style>
