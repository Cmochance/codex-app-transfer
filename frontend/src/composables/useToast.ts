import { reactive } from 'vue'

// 全局 toast 队列(单例,移植旧 app.js showToast)。3.2s 自动消失,点击即关。
export interface Toast {
  id: number
  message: string
  type: 'info' | 'error'
}

const toasts = reactive<Toast[]>([])
let seq = 0

function dismiss(id: number) {
  const i = toasts.findIndex((x) => x.id === id)
  if (i >= 0) toasts.splice(i, 1)
}

function show(message: string, type: 'info' | 'error' = 'info') {
  if (!message) return
  const id = ++seq
  toasts.push({ id, message, type })
  window.setTimeout(() => dismiss(id), 3200)
}

export function useToast() {
  return { toasts, show, dismiss }
}
