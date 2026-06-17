<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useUsageStore } from '@/stores/usage'
import * as usageApi from '@/api/usage'
import type { UsageRow, UsageView, CacheBucket } from '@/api/usage'
import { t } from '@/i18n'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppModal from '@/components/ui/AppModal.vue'
import IconRefresh from '~icons/lucide/refresh-cw'
import IconArrowDown from '~icons/lucide/arrow-down-circle'
import IconArrowUp from '~icons/lucide/arrow-up-circle'
import IconLayers from '~icons/lucide/layers'
import IconChat from '~icons/lucide/messages-square'

const store = useUsageStore()

onMounted(() => store.load())

function refresh() {
  store.load(true)
}

// ── 格式化(逐字移植旧 app.js)────────────────────────────────────────────
function fmtNum(n: number | undefined | null): string {
  if (n === null || n === undefined) return '—'
  return Number(n).toLocaleString()
}
// 后端已按用户 tz format 成 `YYYY-MM-DD HH:MM`;这里防御性裁切前 16 字符。
function fmtLastActivity(s?: string): string {
  if (!s) return '—'
  const m = s.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})/)
  return m ? `${m[1]} ${m[2]}` : s
}
// 整体命中率 = cachedInput / input;input=0 → null(显示 —)
function cacheHitPct(row: UsageRow): number | null {
  const input = row.inputTokens || 0
  if (input <= 0) return null
  return Math.round(((row.cachedInputTokens || 0) / input) * 100)
}

// ── KPI ────────────────────────────────────────────────────────────────
const kpis = computed(() => {
  const r = store.report
  return [
    { label: t('usage.kpi.totalInput'), value: fmtNum(r?.totalInputTokens), icon: IconArrowDown },
    { label: t('usage.kpi.totalOutput'), value: fmtNum(r?.totalOutputTokens), icon: IconArrowUp },
    { label: t('usage.kpi.totalTokens'), value: fmtNum(r?.totalTokens), icon: IconLayers },
    { label: t('usage.kpi.conversations'), value: fmtNum(r?.totalConversations), icon: IconChat },
  ]
})

// ── 视图 + 表格 ──────────────────────────────────────────────────────────
const viewOptions: { value: UsageView; label: string }[] = [
  { value: 'conversation', label: t('usage.viewConversation') },
  { value: 'daily', label: t('usage.viewDaily') },
  { value: 'model', label: t('usage.viewModel') },
]

const firstColLabel = computed(() => {
  if (store.activeView === 'daily') return t('usage.col.date')
  if (store.activeView === 'model') return t('usage.col.model')
  return t('usage.col.conversation')
})
// By Model 视图首列已是 model name,第二个 models 列必然重复 → 跳过(Devin #280 fix)。
const showModelsCol = computed(() => store.activeView !== 'model')

const rows = computed<UsageRow[]>(() => {
  const r = store.report
  if (!r) return []
  let list: UsageRow[]
  if (store.activeView === 'daily') list = r.daily || []
  else if (store.activeView === 'model') list = r.byModel || []
  else list = r.byConversation || []
  // daily 按日期降序;model/conversation 按 total tokens 降序
  return list.slice().sort((a, b) => {
    if (store.activeView === 'daily') return (b.group || '').localeCompare(a.group || '')
    return (b.totalTokens || 0) - (a.totalTokens || 0)
  })
})

// 「按对话」首列显示 Codex 对话名前 5 字,全名 + rollout 路径放 title;无名回退日期。
function firstCol(row: UsageRow): { label: string; title: string } {
  if (store.activeView !== 'conversation') return { label: row.group || '—', title: '' }
  const name = (row.displayName || '').trim()
  let label: string
  if (name) label = name.length > 5 ? `${name.slice(0, 5)}…` : name
  else {
    const m = (row.group || '').match(/^\d{4}\/(\d{2})\/(\d{2})\//)
    label = m ? `${m[1]}/${m[2]}` : '—'
  }
  const title = name ? `${name}\n${row.group || ''}` : row.group || ''
  return { label, title }
}
// 按对话视图优先显示真实上游模型(proxy 记录);无则回退 rollout 客户端模型名。
function modelText(row: UsageRow): string {
  if (store.activeView === 'conversation' && row.upstreamModel) return row.upstreamModel
  return (row.models || []).join(', ') || '—'
}

// ── 缓存命中分布 modal ───────────────────────────────────────────────────
const modalSession = ref<string | null>(null)
const cacheBuckets = ref<CacheBucket[]>([])
const cacheLoading = ref(false)
const cacheError = ref('')

async function openCache(row: UsageRow) {
  const pct = cacheHitPct(row)
  if (store.activeView !== 'conversation' || pct == null || !row.group) return
  modalSession.value = row.group
  cacheBuckets.value = []
  cacheError.value = ''
  cacheLoading.value = true
  try {
    cacheBuckets.value = await usageApi.getCacheSeries(row.group)
  } catch (e) {
    cacheError.value = (e as Error).message || t('usage.loadError')
  } finally {
    cacheLoading.value = false
  }
}

// 整体命中率汇总(modal 副标题)+ 每柱的高度/命中比(≤10 桶后端已分好)。
const cacheSummary = computed(() => {
  const buckets = cacheBuckets.value
  if (!buckets.length) return ''
  let totCached = 0
  let totInput = 0
  let totOutput = 0
  for (const b of buckets) {
    totCached += b.cachedInputTokens || 0
    totInput += b.inputTokens || 0
    totOutput += b.outputTokens || 0
  }
  const overall = totInput > 0 ? Math.round((100 * totCached) / totInput) : 0
  return (
    `${t('usage.cacheModal.overall')}: ${overall}%  ·  ${fmtNum(totCached)} / ${fmtNum(totInput)}` +
    `  ·  ${t('usage.cacheModal.output')} ${fmtNum(totOutput)}`
  )
})

const cacheBars = computed(() => {
  const buckets = cacheBuckets.value
  if (!buckets.length) return []
  let maxInput = 0
  for (const b of buckets) maxInput = Math.max(maxInput, b.inputTokens || 0)
  const totalTurns = buckets[buckets.length - 1].turnEnd || 1
  return buckets.map((b) => {
    const input = b.inputTokens || 0
    const cached = b.cachedInputTokens || 0
    const output = b.outputTokens || 0
    const pct = input > 0 ? Math.round((100 * cached) / input) : 0
    // 柱高 = 该桶总输入 / 全局最大输入(体现 token 量);柱内命中部分 = cached/input。
    const barH = maxInput > 0 ? Math.round((100 * input) / maxInput) : 0
    const posPct = Math.round((100 * (b.turnEnd || 0)) / totalTurns)
    const title =
      `${t('usage.cacheModal.hitInput')}: ${fmtNum(cached)}\n` +
      `${t('usage.cacheModal.totalInput')}: ${fmtNum(input)}\n` +
      `${t('usage.cacheModal.output')}: ${fmtNum(output)}`
    return { pct, barH, posPct, title }
  })
})
</script>

<template>
  <div>
    <div class="page-head">
      <div>
        <h1 class="page-title">{{ t('usage.title') }}</h1>
        <p class="page-sub">{{ t('usage.subtitle') }}</p>
      </div>
      <AppButton
        variant="ghost"
        size="sm"
        :icon="IconRefresh"
        :label="t('usage.refresh')"
        :disabled="store.loading"
        @click="refresh"
      />
    </div>

    <!-- 错误条:fetch 失败不写空 report 误显 "0 用量",显带 retry 的错误 -->
    <div v-if="store.error" class="usage-error">
      <span>{{ t('usage.loadError') }}: {{ store.error }}</span>
      <AppButton variant="ghost" size="sm" :label="t('usage.refresh')" @click="refresh" />
    </div>

    <template v-else>
      <div class="kpis">
        <article v-for="(kpi, i) in kpis" :key="i" class="kpi">
          <component :is="kpi.icon" class="kpi__icon" />
          <div class="kpi__body">
            <span class="kpi__label">{{ kpi.label }}</span>
            <strong class="kpi__value">{{ kpi.value }}</strong>
          </div>
        </article>
      </div>

      <div class="usage-toolbar">
        <SegmentedControl
          :model-value="store.activeView"
          :options="viewOptions"
          @update:model-value="store.setView($event as UsageView)"
        />
      </div>

      <div class="usage-card">
        <div v-if="store.loading && !store.report" class="usage-state">{{ t('usage.loading') }}</div>
        <div v-else-if="!rows.length" class="usage-state">{{ t('usage.empty') }}</div>
        <div v-else class="usage-table-wrap">
          <table class="usage-table">
            <thead>
              <tr>
                <th>{{ firstColLabel }}</th>
                <th v-if="showModelsCol">{{ t('usage.col.model') }}</th>
                <th class="num">{{ t('usage.col.cacheHit') }}</th>
                <th class="num">{{ t('usage.col.input') }}</th>
                <th class="num">{{ t('usage.col.output') }}</th>
                <th class="num">{{ t('usage.col.reasoning') }}</th>
                <th class="num">{{ t('usage.col.total') }}</th>
                <th class="num">{{ t('usage.col.turns') }}</th>
                <th>{{ t('usage.col.lastActivity') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in rows" :key="i">
                <td :title="firstCol(row).title">{{ firstCol(row).label }}</td>
                <td v-if="showModelsCol" class="model">{{ modelText(row) }}</td>
                <td class="num">
                  <button
                    v-if="store.activeView === 'conversation' && cacheHitPct(row) != null && row.group"
                    type="button"
                    class="cache-hit-btn"
                    :title="t('usage.cacheModal.title')"
                    @click="openCache(row)"
                  >
                    {{ cacheHitPct(row) }}%
                  </button>
                  <span v-else>{{ cacheHitPct(row) == null ? '—' : `${cacheHitPct(row)}%` }}</span>
                </td>
                <td class="num">{{ fmtNum(row.inputTokens) }}</td>
                <td class="num">{{ fmtNum(row.outputTokens) }}</td>
                <td class="num">{{ fmtNum(row.reasoningOutputTokens) }}</td>
                <td class="num"><strong>{{ fmtNum(row.totalTokens) }}</strong></td>
                <td class="num">{{ fmtNum(row.turnCount) }}</td>
                <td class="last">{{ fmtLastActivity(row.lastActivity) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </template>

    <!-- 缓存命中逐轮分布 -->
    <AppModal
      v-if="modalSession"
      wide
      :title="t('usage.cacheModal.title')"
      @close="modalSession = null"
    >
      <!-- 加载态 cacheSummary 为空 → 自然回退显示对话名(与旧 app.js 一致, 不显重复 loading 文案) -->
      <p class="cache-summary">{{ cacheSummary || modalSession }}</p>
      <div v-if="cacheError" class="usage-state">{{ cacheError }}</div>
      <div v-else-if="cacheLoading" class="usage-state">{{ t('usage.cacheModal.loading') }}</div>
      <div v-else-if="!cacheBars.length" class="usage-state">{{ t('usage.cacheModal.empty') }}</div>
      <div v-else class="ucbars">
        <div v-for="(bar, i) in cacheBars" :key="i" class="ucbar" :title="bar.title">
          <div class="ucbar-track">
            <div class="ucbar-total" :style="{ height: bar.barH + '%' }">
              <div class="ucbar-hit" :style="{ height: bar.pct + '%' }"></div>
            </div>
          </div>
          <div class="ucbar-pct">{{ bar.pct }}%</div>
          <div class="ucbar-x">{{ bar.posPct }}%</div>
        </div>
      </div>
    </AppModal>
  </div>
</template>

<style scoped>
.page-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-4);
  margin-bottom: var(--space-5);
}
.page-title {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0 0 4px;
}
.page-sub {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  margin: 0;
  max-width: 520px;
}

/* KPI 卡片 */
.kpis {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: var(--space-3);
  margin-bottom: var(--space-5);
}
.kpi {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
}
.kpi__icon {
  flex-shrink: 0;
  width: 22px;
  height: 22px;
  color: var(--accent);
}
.kpi__body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.kpi__label {
  font-size: var(--fs-xs);
  color: var(--text-muted);
  white-space: nowrap;
}
.kpi__value {
  font-size: var(--fs-lg);
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}

.usage-toolbar {
  display: flex;
  justify-content: flex-end;
  margin-bottom: var(--space-3);
}

/* 表格卡片 */
.usage-card {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.usage-state {
  padding: var(--space-6) var(--space-4);
  text-align: center;
  color: var(--text-muted);
  font-size: var(--fs-sm);
}
.usage-table-wrap {
  overflow-x: auto;
}
.usage-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-sm);
}
.usage-table th,
.usage-table td {
  padding: var(--space-2) var(--space-3);
  text-align: left;
  white-space: nowrap;
}
.usage-table thead th {
  font-size: var(--fs-xs);
  font-weight: 600;
  color: var(--text-muted);
  border-bottom: 1px solid var(--border);
  background: var(--surface-2);
}
.usage-table tbody tr + tr td {
  border-top: 1px solid var(--border);
}
.usage-table .num {
  text-align: right;
  font-variant-numeric: tabular-nums;
  font-family: var(--font-mono);
}
.usage-table .model,
.usage-table .last {
  color: var(--text-secondary);
  max-width: 180px;
  overflow: hidden;
  text-overflow: ellipsis;
}
.cache-hit-btn {
  border: none;
  background: transparent;
  color: var(--accent);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  cursor: pointer;
  padding: 0;
  text-decoration: underline;
  text-underline-offset: 2px;
}

.usage-error {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  background: var(--danger-soft);
  border: 1px solid var(--danger);
  border-radius: var(--radius-lg);
  color: var(--danger);
  font-size: var(--fs-sm);
}

/* 缓存命中柱状图 */
.cache-summary {
  margin: 0 0 var(--space-4);
  font-size: var(--fs-sm);
  color: var(--text-secondary);
}
.ucbars {
  display: flex;
  align-items: flex-end;
  gap: var(--space-2);
  height: 180px;
  padding-top: var(--space-2);
}
.ucbar {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  height: 100%;
}
.ucbar-track {
  flex: 1;
  width: 100%;
  display: flex;
  align-items: flex-end;
  justify-content: center;
}
.ucbar-total {
  width: 70%;
  min-height: 2px;
  display: flex;
  align-items: flex-end;
  justify-content: center;
  background: var(--accent-soft);
  border-radius: var(--radius-sm) var(--radius-sm) 0 0;
  overflow: hidden;
}
.ucbar-hit {
  width: 100%;
  background: var(--accent);
}
.ucbar-pct {
  font-size: var(--fs-xs);
  font-variant-numeric: tabular-nums;
  color: var(--text-secondary);
}
.ucbar-x {
  font-size: var(--fs-xs);
  color: var(--text-muted);
}

@media (max-width: 560px) {
  .kpis {
    grid-template-columns: repeat(2, 1fr);
  }
}
</style>
