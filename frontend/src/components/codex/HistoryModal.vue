<script setup lang="ts">
import { computed, ref } from 'vue'
import type { ManagedHistoryEntry } from '@/api/codex'
import { t } from '@/i18n'
import AppModal from '@/components/ui/AppModal.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppSelect from '@/components/ui/AppSelect.vue'

// 通用历史快照 modal:picker + LCS diff(快照 vs 当前内容)+ restore。
// entries 已 reversed(最新在前);labelPrefix 由 parent 按 resource 算好。
const props = defineProps<{
  entries: ManagedHistoryEntry[]
  currentContent: string
  labelPrefix: string
}>()
const emit = defineEmits<{ close: []; restore: [index: number] }>()

const selectedIdx = ref(0)

function fmtLabel(entry: ManagedHistoryEntry): string {
  const ts = new Date((entry.timestamp || 0) * 1000).toLocaleString()
  return props.labelPrefix ? `${props.labelPrefix} · ${ts}` : ts
}

// 自定义下拉(AppSelect)选项:value=快照下标,label=带前缀的时间戳。替代原生 <select>
// (原生弹窗会遮挡 modal 内容)。
const historyOptions = computed(() =>
  props.entries.map((e, i) => ({ value: i, label: fmtLabel(e) })),
)

// LCS-based 行级 diff(逐字移植 codexLineDiff,O(m*n))→ [{type:ctx|add|del, text}]
function lineDiff(oldText: string, newText: string): { type: string; text: string }[] {
  const oldLines = oldText.split('\n')
  const newLines = newText.split('\n')
  const m = oldLines.length
  const n = newLines.length
  const dp: number[][] = Array.from({ length: m + 1 }, () => new Array(n + 1).fill(0))
  for (let i = 0; i < m; i++)
    for (let j = 0; j < n; j++)
      dp[i + 1][j + 1] =
        oldLines[i] === newLines[j] ? dp[i][j] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1])
  const result: { type: string; text: string }[] = []
  let i = m
  let j = n
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && oldLines[i - 1] === newLines[j - 1]) {
      result.unshift({ type: 'ctx', text: oldLines[i - 1] })
      i--
      j--
    } else if (j > 0 && (i === 0 || dp[i][j - 1] >= dp[i - 1][j])) {
      result.unshift({ type: 'add', text: newLines[j - 1] })
      j--
    } else {
      result.unshift({ type: 'del', text: oldLines[i - 1] })
      i--
    }
  }
  return result
}

const diff = computed(() => {
  const entry = props.entries[selectedIdx.value]
  if (!entry) return []
  const newContent = entry.appliedContent || entry.managedContent || ''
  return lineDiff(props.currentContent || '', newContent)
})

function onRestore() {
  const entry = props.entries[selectedIdx.value]
  if (!entry) return
  emit('restore', entry.index)
}
</script>

<template>
  <AppModal wide :title="t('codex.historyTitle')" @close="emit('close')">
    <div v-if="!entries.length" class="hist-empty">{{ t('codex.historyEmpty') }}</div>
    <template v-else>
      <div class="hist-picker">
        <AppSelect v-model="selectedIdx" :options="historyOptions" />
      </div>

      <div v-if="!diff.length" class="hist-empty">{{ t('codex.historyDiffEmpty') }}</div>
      <pre v-else class="hist-diff"><span
        v-for="(d, i) in diff"
        :key="i"
        class="hist-diff__line"
        :class="`hist-diff__line--${d.type}`"
      >{{ d.text || ' ' }}</span></pre>

      <div class="hist-actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="emit('close')" />
        <AppButton variant="primary" :label="t('codex.historyApply')" @click="onRestore" />
      </div>
    </template>
  </AppModal>
</template>

<style scoped>
.hist-empty {
  padding: var(--space-5) var(--space-2);
  text-align: center;
  color: var(--text-muted);
  font-size: var(--fs-sm);
}
.hist-picker {
  margin-bottom: var(--space-3);
}
.hist-diff {
  max-height: 46vh;
  overflow: auto;
  margin: 0;
  padding: var(--space-2) 0;
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  line-height: 1.5;
}
.hist-diff__line {
  display: block;
  padding: 0 var(--space-3);
  white-space: pre-wrap;
  word-break: break-all;
}
.hist-diff__line--add {
  background: var(--success-soft);
  color: var(--success);
}
.hist-diff__line--del {
  background: var(--danger-soft);
  color: var(--danger);
}
.hist-diff__line--ctx {
  color: var(--text-secondary);
}
.hist-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
</style>
