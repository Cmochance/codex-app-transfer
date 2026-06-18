import { defineStore } from 'pinia'
import { ref } from 'vue'
import * as settingsApi from '@/api/settings'
import type { Settings } from '@/api/settings'

export const useSettingsStore = defineStore('settings', () => {
  const settings = ref<Settings>({})
  const loaded = ref(false)

  async function load() {
    settings.value = await settingsApi.getSettings()
    loaded.value = true
    return settings.value
  }

  // PUT partial(浅合并)→ 后端返合并后 settings;返回可选 webFetchSyncWarning 供 UI toast。
  // 乐观更新(开关即时响应)+ 失败回滚(防 UI 与服务端不一致)。
  async function save(partial: Settings): Promise<string | undefined> {
    const prev = { ...settings.value }
    settings.value = { ...settings.value, ...partial }
    try {
      const { settings: merged, webFetchSyncWarning } = await settingsApi.saveSettings(partial)
      settings.value = merged
      return webFetchSyncWarning
    } catch (e) {
      settings.value = prev
      throw e
    }
  }

  // 带默认值的 typed getter(旧 app.js renderSettings 的 `!== false` / `=== true` 默认语义)
  function bool(key: string, def: boolean): boolean {
    const v = settings.value[key]
    return typeof v === 'boolean' ? v : def
  }
  function str(key: string, def = ''): string {
    const v = settings.value[key]
    return typeof v === 'string' ? v : def
  }
  function num(key: string, def = 0): number {
    const v = settings.value[key]
    return typeof v === 'number' ? v : def
  }

  return { settings, loaded, load, save, bool, str, num }
})
