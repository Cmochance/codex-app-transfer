import { reactive } from 'vue'
import zh from './zh'
import en from './en'

export type Locale = 'zh' | 'en'

const dicts: Record<Locale, Record<string, string>> = { zh, en }

// reactive locale: 切换语言后所有用 t() 的模板自动重渲染
export const i18nState = reactive<{ locale: Locale }>({ locale: 'zh' })

// 三级 fallback(移植 i18n.js): 当前语言 → 中文 → 键本身
export function t(key: string): string {
  return dicts[i18nState.locale][key] ?? dicts.zh[key] ?? key
}

export function setLocale(l: Locale) {
  i18nState.locale = dicts[l] ? l : 'zh'
  document.documentElement.lang = i18nState.locale === 'zh' ? 'zh-CN' : 'en'
  try {
    localStorage.setItem('cas:lang', i18nState.locale)
  } catch {
    /* ignore */
  }
  // Stage 3 接 settings store 后同步 PUT /api/settings { language }
}

export function cachedLocale(): Locale {
  try {
    const v = localStorage.getItem('cas:lang')
    if (v === 'zh' || v === 'en') return v
  } catch {
    /* ignore */
  }
  const nav = (navigator.language || 'zh').toLowerCase()
  return nav.startsWith('en') ? 'en' : 'zh'
}
