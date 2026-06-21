<script setup lang="ts">
// [MOC-272] 调色盘编辑器 — 改某主题的精选关键色(accent / ink / baseColor / surface + scrim 蒙版深浅)。
// 「复用预制主题调色盘」一键填入某预制的配色;「还原默认」清除该主题的全部覆盖。保存 emit override,
// 父组件落盘(settings.themePaletteOverrides)+ 重注入。颜色用 <input type=color>(恒 #rrggbb)。
import { ref } from 'vue'
import { i18nState, t } from '@/i18n'
import type { PaletteOverride, ThemeEntry } from '@/api/desktop'
import AppModal from '@/components/ui/AppModal.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

const props = defineProps<{ entry: ThemeEntry; presets: ThemeEntry[] }>()
const emit = defineEmits<{ save: [override: PaletteOverride]; reset: []; close: [] }>()

const p = props.entry.palette
// accent "" = 跟随原生蓝 → 默认不启用;启用时给个起始色。
const accentEnabled = ref(!!p.accent)
const accent = ref(p.accent || '#0a84ff')
const ink = ref(p.ink || '#f1ece4')
const baseColor = ref(p.baseColor || '#0e0e10')
const surface = ref(p.surface || '#141418')
// scrim null = 跟随基底 → 默认不启用;启用时给默认起点。
const scrimEnabled = ref(p.scrim !== null && p.scrim !== undefined)
const scrim = ref(p.scrim ?? 50)

function name(e: ThemeEntry): string {
  return i18nState.locale === 'en' ? e.displayNameEn : e.displayNameZh
}

// 复用预制:选一个预制主题,把它的精选调色板填进编辑器(用户可再微调)。
const reuseId = ref('')
function onReuse() {
  const src = props.presets.find((e) => e.id === reuseId.value)
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
  const ov: PaletteOverride = {
    ink: ink.value,
    baseColor: baseColor.value,
    surface: surface.value,
  }
  if (accentEnabled.value) ov.accent = accent.value
  if (scrimEnabled.value) ov.scrim = scrim.value
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
