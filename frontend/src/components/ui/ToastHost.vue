<script setup lang="ts">
import { useToast } from '@/composables/useToast'

const { toasts, dismiss } = useToast()
</script>

<template>
  <Teleport to="body">
    <div class="toast-host">
      <TransitionGroup name="toast">
        <div
          v-for="toast in toasts"
          :key="toast.id"
          class="toast"
          :class="`toast--${toast.type}`"
          @click="dismiss(toast.id)"
        >
          {{ toast.message }}
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
.toast-host {
  position: fixed;
  left: 50%;
  bottom: var(--space-6);
  transform: translateX(-50%);
  z-index: 2000;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: var(--space-2);
  pointer-events: none;
}
.toast {
  max-width: 460px;
  padding: var(--space-2) var(--space-4);
  border-radius: var(--radius);
  background: var(--surface);
  border: 1px solid var(--border-strong);
  box-shadow: var(--shadow-lg);
  color: var(--text);
  font-size: var(--fs-sm);
  pointer-events: auto;
  cursor: default;
}
.toast--error {
  border-color: var(--danger);
  color: var(--danger);
}
.toast-enter-active,
.toast-leave-active {
  transition: opacity var(--transition), transform var(--transition);
}
.toast-enter-from,
.toast-leave-to {
  opacity: 0;
  transform: translateY(8px);
}
</style>
