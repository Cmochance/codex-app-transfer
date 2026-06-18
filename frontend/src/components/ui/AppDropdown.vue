<script setup lang="ts">
// 通用锚定弹层:面板上沿紧贴 trigger 下沿,固定 max-height 可滚,点外/Esc 关闭。
// 用于自定义 select(AppSelect)与「选项」内联面板,替代原生 <select> 弹窗 / modal。
import { onMounted, onUnmounted, ref } from 'vue'

withDefaults(defineProps<{ align?: 'left' | 'right'; panelWidth?: string }>(), { align: 'left' })

const open = ref(false)
const root = ref<HTMLElement>()
function toggle() {
  open.value = !open.value
}
function close() {
  open.value = false
}
function onDocPointer(e: PointerEvent) {
  if (open.value && root.value && !root.value.contains(e.target as Node)) close()
}
function onKey(e: KeyboardEvent) {
  if (open.value && e.key === 'Escape') close()
}
onMounted(() => {
  document.addEventListener('pointerdown', onDocPointer)
  document.addEventListener('keydown', onKey)
})
onUnmounted(() => {
  document.removeEventListener('pointerdown', onDocPointer)
  document.removeEventListener('keydown', onKey)
})
defineExpose({ close })
</script>

<template>
  <div ref="root" class="dropdown">
    <div class="dropdown__trigger" @click="toggle">
      <slot name="trigger" :open="open" />
    </div>
    <div
      v-if="open"
      class="dropdown__panel"
      :class="{ 'dropdown__panel--right': align === 'right' }"
      :style="panelWidth ? { width: panelWidth } : undefined"
    >
      <slot :close="close" />
    </div>
  </div>
</template>

<style scoped>
.dropdown {
  position: relative;
  display: inline-flex;
}
.dropdown__trigger {
  display: inline-flex;
  width: 100%;
}
/* 上沿紧贴 trigger 下沿(top:100%)+ 固定 max-height 可滚 */
.dropdown__panel {
  position: absolute;
  top: 100%;
  left: 0;
  z-index: 100;
  min-width: 100%;
  max-height: 300px;
  overflow-y: auto;
  padding: var(--space-1);
  background: var(--surface);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  box-shadow: var(--shadow-md);
}
.dropdown__panel--right {
  left: auto;
  right: 0;
}
</style>
