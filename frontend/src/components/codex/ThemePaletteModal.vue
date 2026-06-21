<script setup lang="ts">
// [MOC-272] 调色盘编辑器 — 改某主题的精选关键色(accent / ink / baseColor / surface + scrim 蒙版深浅)。
// 「复用预制主题调色盘」一键填入某预制配色;「还原默认」清除该主题全部覆盖。
// 关键:**只持久化用户真正改过的字段**(diff vs 初始 + 合并已有 override),避免「开+保存」就把未动的
// 玻璃层/accent 层次冻结、覆盖掉基底(/code-review #1/#2/#3)。颜色用 <input type=color>(恒 #rrggbb)。
import { ref } from 'vue'
import { i18nState, t } from '@/i18n'
import type { PaletteOverride, ThemeEntry } from '@/api/desktop'
import AppModal from '@/components/ui/AppModal.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

const props = defineProps<{
  entry: ThemeEntry
  presets: ThemeEntry[]
  /** 该主题已持久化的 raw override(用于 merge,保留用户未触碰的既有覆盖)。 */
  override?: PaletteOverride
}>()
const emit = defineEmits<{ save: [override: PaletteOverride]; reset: []; close: [] }>()

const p = props.entry.palette
// 初始快照(= 有效值;diff 基准)。accent "" / scrim null = 「不覆盖」语义。
const initAccentOn = !!p.accent
const initAccent = p.accent || '#0a84ff'
const initScrimOn = p.scrim !== null && p.scrim !== undefined
const initScrim = p.scrim ?? 50

const accentEnabled = ref(initAccentOn)
const accent = ref(initAccent)
const ink = ref(p.ink || '#f1ece4')
const baseColor = ref(p.baseColor || '#0e0e10')
const surface = ref(p.surface || '#141418')
const scrimEnabled = ref(initScrimOn)
const scrim = ref(initScrim)

function name(e: ThemeEntry): string {
  return i18nState.locale === 'en' ? e.displayNameEn : e.displayNameZh
}

// 复用预制:选一个预制主题把它的精选调色板填进编辑器(用户可再微调)。
// 应用后清空选择,让用户能再次选同一项重填(/code-review #7:@change 同值不触发)。
const reuseId = ref('')
function onReuse() {
  const src = props.presets.find((e) => e.id === reuseId.value)
  reuseId.value = ''
  if (!src) return
  const sp = src.palette
  accentEnabled.value = !!sp.accent
  if (sp.accent) accent.value = sp.accent
  ink.value = sp.ink
  baseColor.value = sp.baseColor
  surface.value = sp.surface
  scrimEnabled.value = sp.scrim !== null && sp.scrim !== undefined
  if (sp.scrim != null) scrim.value = sp.scrim
}

function onSave() {
  // 合并到已有 override,只动用户真正改过的字段(未动字段保持原样 → 不冻结基底/既有覆盖)。
  const ov: PaletteOverride = { ...(props.override ?? {}) }
  const accentChanged = accentEnabled.value !== initAccentOn || (accentEnabled.value && accent.value !== initAccent)
  if (accentChanged) {
    if (accentEnabled.value) ov.accent = accent.value
    else delete ov.accent
  }
  if (ink.value !== p.ink) ov.ink = ink.value
  if (baseColor.value !== p.baseColor) ov.baseColor = baseColor.value
  if (surface.value !== p.surface) ov.surface = surface.value
  const scrimChanged = scrimEnabled.value !== initScrimOn || (scrimEnabled.value && scrim.value !== initScrim)
  if (scrimChanged) {
    if (scrimEnabled.value) ov.scrim = scrim.value
    else delete ov.scrim
  }
  emit('save', ov)
}
</script>

<template>
  <AppModal :title="`${t('theme.paletteTitle')} · ${name(props.entry)}`" @close="emit('close')">
    <div class="pal">
      <label class="pal-row">
        <span class="pal-label">{{ t('theme.paletteReuse') }}</span>
        <select v-model="reuseId" class="pal-select" @change="onReuse">
          <option value="">{{ t('theme.paletteReusePick') }}</option>
          <option v-for="e in props.presets" :key="e.id" :value="e.id">{{ name(e) }}</option>
        </select>
      </label>

      <div class="pal-row">
        <span class="pal-label">{{ t('theme.paletteAccent') }}</span>
        <div class="pal-ctl">
          <AppSwitch v-model="accentEnabled" />
          <input v-if="accentEnabled" v-model="accent" type="color" class="pal-color" />
          <span v-else class="pal-hint">{{ t('theme.paletteAccentNative') }}</span>
        </div>
      </div>

      <label class="pal-row">
        <span class="pal-label">{{ t('theme.paletteInk') }}</span>
        <input v-model="ink" type="color" class="pal-color" />
      </label>
      <label class="pal-row">
        <span class="pal-label">{{ t('theme.paletteBase') }}</span>
        <input v-model="baseColor" type="color" class="pal-color" />
      </label>
      <label class="pal-row">
        <span class="pal-label">{{ t('theme.paletteSurface') }}</span>
        <input v-model="surface" type="color" class="pal-color" />
      </label>

      <div class="pal-row">
        <span class="pal-label">{{ t('theme.paletteScrim') }}</span>
        <div class="pal-ctl">
          <AppSwitch v-model="scrimEnabled" />
          <input
            v-if="scrimEnabled"
            v-model.number="scrim"
            type="range"
            min="0"
            max="100"
            class="pal-range"
          />
          <span v-if="scrimEnabled" class="pal-val">{{ scrim }}</span>
          <span v-else class="pal-hint">{{ t('theme.paletteScrimBase') }}</span>
        </div>
      </div>

      <div class="pal-actions">
        <AppButton variant="ghost" size="sm" :label="t('theme.paletteReset')" @click="emit('reset')" />
        <div class="pal-actions__right">
          <AppButton variant="ghost" size="sm" :label="t('common.cancel')" @click="emit('close')" />
          <AppButton variant="primary" size="sm" :label="t('theme.paletteSave')" @click="onSave" />
        </div>
      </div>
    </div>
  </AppModal>
</template>

<style scoped>
.pal {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.pal-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}
.pal-label {
  font-size: var(--fs-sm);
  color: var(--text-secondary);
}
.pal-ctl {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.pal-color {
  width: 44px;
  height: 26px;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: transparent;
  cursor: pointer;
}
.pal-select {
  flex: 1;
  max-width: 200px;
  font-size: var(--fs-sm);
  padding: 4px 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--surface-2);
  color: var(--text);
}
.pal-range {
  width: 140px;
}
.pal-val {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  min-width: 26px;
  text-align: right;
}
.pal-hint {
  font-size: var(--fs-sm);
  color: var(--text-muted);
}
.pal-actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-2);
  margin-top: var(--space-2);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}
.pal-actions__right {
  display: flex;
  gap: var(--space-2);
}
</style>
