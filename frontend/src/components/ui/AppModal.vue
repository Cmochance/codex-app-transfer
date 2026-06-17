<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue'
import IconClose from '~icons/lucide/x'

const props = defineProps<{ title?: string; wide?: boolean }>()
const emit = defineEmits<{ close: [] }>()

function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') emit('close')
}
onMounted(() => document.addEventListener('keydown', onKey))
onUnmounted(() => document.removeEventListener('keydown', onKey))
</script>

<template>
  <Teleport to="body">
    <div class="modal-overlay" @click.self="emit('close')">
      <div class="modal" :class="{ 'modal--wide': props.wide }" role="dialog" aria-modal="true">
        <header class="modal__head">
          <h3 class="modal__title">{{ props.title }}</h3>
          <button type="button" class="modal__close" aria-label="关闭" @click="emit('close')">
            <IconClose />
          </button>
        </header>
        <div class="modal__body">
          <slot />
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.modal-overlay {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: var(--space-5);
  background: color-mix(in srgb, #000 38%, transparent);
  backdrop-filter: blur(2px);
  -webkit-backdrop-filter: blur(2px);
}
.modal {
  width: 100%;
  max-width: 440px;
  max-height: 86vh;
  display: flex;
  flex-direction: column;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-lg);
  overflow: hidden;
}
.modal--wide {
  max-width: 640px;
}
.modal__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid var(--border);
}
.modal__title {
  font-size: var(--fs-md);
  font-weight: 600;
  margin: 0;
}
.modal__close {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  border: none;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-secondary);
  transition: background var(--transition), color var(--transition);
}
.modal__close:hover {
  background: var(--surface-hover);
  color: var(--text);
}
.modal__close svg {
  width: 16px;
  height: 16px;
}
.modal__body {
  padding: var(--space-4);
  overflow-y: auto;
}
</style>
