import { ref } from 'vue'

export type Appearance = 'light' | 'dark' | 'inkwash'

const STORAGE_KEY = 'cas:appearance'
const VALID: Appearance[] = ['light', 'dark', 'inkwash']

// 模块级单例: 全 app 共享当前主题
const current = ref<Appearance>('light')

function normalizeLegacy(v?: string | null): Appearance | null {
  if (!v) return null
  if (v === 'dark') return 'dark'
  if (v === 'inkwash') return 'inkwash'
  // 旧 6-palette(default/white/gray/green/orange)统一收敛到 light
  if (['light', 'default', 'white', 'gray', 'green', 'orange'].includes(v)) return 'light'
  return null
}

export function useAppearance() {
  function set(theme: Appearance, persist = true) {
    const t = VALID.includes(theme) ? theme : 'light'
    current.value = t
    document.documentElement.setAttribute('data-theme', t)
    if (persist) {
      try {
        localStorage.setItem(STORAGE_KEY, t)
      } catch {
        /* localStorage 不可用时忽略 */
      }
      // Stage 3 接 settings store 后, 这里同步 PUT /api/settings { theme: t }
    }
  }

  function load(serverTheme?: string) {
    let fromLocal: string | null = null
    try {
      fromLocal = localStorage.getItem(STORAGE_KEY)
    } catch {
      /* ignore */
    }
    const t = normalizeLegacy(serverTheme) ?? normalizeLegacy(fromLocal) ?? 'light'
    set(t, false)
  }

  return { current, set, load }
}
