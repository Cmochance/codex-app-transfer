import { ref } from 'vue'

// 统一的自建确认弹窗(替代原生 window.confirm —— 浏览器默认弹窗会脱离 app 风格 / 在 Tauri webview 里
// 样式不可控)。用法:`const { confirm } = useConfirm(); if (!(await confirm(msg))) return`。
// 模块级单例 state,由全局挂载的 <ConfirmDialog /> 渲染并 resolve。
export interface ConfirmOptions {
  message: string
  title?: string
  confirmLabel?: string
  cancelLabel?: string
  /** true → 确认按钮用 danger 样式(删除 / 覆盖类) */
  danger?: boolean
}
interface ConfirmState extends ConfirmOptions {
  open: boolean
  resolve: ((ok: boolean) => void) | null
}

const state = ref<ConfirmState>({ message: '', open: false, resolve: null })

export function useConfirm() {
  function confirm(opts: ConfirmOptions | string): Promise<boolean> {
    const o: ConfirmOptions = typeof opts === 'string' ? { message: opts } : opts
    return new Promise<boolean>((resolve) => {
      // 若已有未决确认,先拒掉旧的(避免悬挂 promise)。
      state.value.resolve?.(false)
      state.value = { ...o, open: true, resolve }
    })
  }
  function respond(ok: boolean) {
    const r = state.value.resolve
    state.value = { message: '', open: false, resolve: null }
    r?.(ok)
  }
  return { state, confirm, respond }
}
