<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import * as codexApi from '@/api/codex'
import type { ConversationMeta, ConversationDetail, ConversationItem, ExportOptions } from '@/api/codex'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { renderMiniMd, truncateString } from '@/lib/miniMd'
import AppButton from '@/components/ui/AppButton.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import AppDropdown from '@/components/ui/AppDropdown.vue'
import IconRefresh from '~icons/lucide/refresh-cw'
import IconDownload from '~icons/lucide/download'
import IconTrash from '~icons/lucide/trash-2'
import IconSettings from '~icons/lucide/sliders-horizontal'

const { show: toast } = useToast()

const sessions = ref<ConversationMeta[]>([])
const selected = reactive(new Set<string>())
const activeId = ref<string | null>(null)
const detail = ref<ConversationDetail | null>(null)
const detailState = ref('')
const loading = ref(false)

const search = ref('')
const kindFilter = ref('all')
const cwdFilter = ref('all')
const format = ref('markdown')

const exportOptions = reactive<ExportOptions>({
  includeReasoning: false,
  includeToolCalls: true,
  toolOutputMaxChars: 2048,
  includeSystemPrompts: false,
  redactSecrets: true,
})

// ── 默认导出目录(localStorage 持久化)──────────────────────────────────
const DEFAULT_DIR_KEY = 'cas.conv.defaultExportDir'
const defaultDir = ref('')
function loadDefaultDir(): string {
  try {
    return localStorage.getItem(DEFAULT_DIR_KEY) || ''
  } catch {
    return ''
  }
}
function saveDefaultDir(dir: string) {
  try {
    if (dir) localStorage.setItem(DEFAULT_DIR_KEY, dir)
    else localStorage.removeItem(DEFAULT_DIR_KEY)
  } catch {
    /* ignore */
  }
  defaultDir.value = dir
}

interface TauriDialog {
  open(opts: Record<string, unknown>): Promise<string | string[] | null>
  save(opts: Record<string, unknown>): Promise<string | null>
}
function tauriDialog(): TauriDialog | null {
  const w = window as unknown as { __TAURI__?: { dialog?: TauriDialog } }
  return w.__TAURI__?.dialog ?? null
}

async function pickDefaultDir() {
  const dialog = tauriDialog()
  if (!dialog?.open) {
    toast('Tauri dialog API 不可用', 'error')
    return
  }
  try {
    const picked = await dialog.open({
      title: t('codex.conv.defaultDirPickTitle'),
      directory: true,
      multiple: false,
      defaultPath: loadDefaultDir() || undefined,
    })
    if (!picked) return
    const dir = Array.isArray(picked) ? picked[0] : picked
    saveDefaultDir(dir)
    toast(tFmt('codex.conv.defaultDirSet', { path: dir }))
  } catch (e) {
    toast(`${t('codex.conv.defaultDirPickFailed')}: ${(e as Error).message || e}`, 'error')
  }
}
function clearDefaultDir() {
  saveDefaultDir('')
  toast(t('codex.conv.defaultDirCleared'))
}

// ── 加载 + 过滤 ──────────────────────────────────────────────────────────
async function load() {
  loading.value = true
  try {
    const j = await codexApi.getConversations()
    sessions.value = j.sessions || []
  } catch (e) {
    sessions.value = []
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  } finally {
    loading.value = false
  }
}

const cwdOptions = computed(() => {
  const counts = new Map<string, number>()
  for (const s of sessions.value) {
    if (!s.cwd) continue
    counts.set(s.cwd, (counts.get(s.cwd) || 0) + 1)
  }
  const sorted = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
  return sorted.map(([cwd, count]) => ({ value: cwd, label: `${cwd.split('/').pop() || cwd} (${count})` }))
})
// 自定义下拉选项(替代原生 <select>)
const kindOptions = computed(() => [
  { value: 'all', label: t('codex.conv.kindAll') },
  { value: 'active', label: t('codex.conv.kindActive') },
  { value: 'archived', label: t('codex.conv.kindArchived') },
])
const cwdSelectOptions = computed(() => [
  { value: 'all', label: t('codex.conv.cwdAll') },
  ...cwdOptions.value,
])
const formatOptions = computed(() => [
  { value: 'markdown', label: 'Markdown (.md)' },
  { value: 'json', label: 'JSON (.json)' },
  { value: 'jsonl', label: `${t('codex.conv.formatJsonl')} (.jsonl)` },
])

const filtered = computed(() => {
  const q = search.value.toLowerCase().trim()
  return sessions.value.filter((s) => {
    if (kindFilter.value !== 'all' && s.kind !== kindFilter.value) return false
    if (cwdFilter.value !== 'all' && s.cwd !== cwdFilter.value) return false
    if (!q) return true
    return [s.title || '', s.id, s.cwd, s.originator, s.modelProvider].join(' ').toLowerCase().includes(q)
  })
})

function fallbackTitle(s: { cwd?: string; id?: string }): string {
  const cwdBase = (s.cwd || '').split('/').pop() || ''
  const shortId = (s.id || '').slice(0, 8)
  return cwdBase ? `${cwdBase} (${shortId})` : `Session ${shortId}`
}
function titleOf(s: ConversationMeta): string {
  return s.title || fallbackTitle(s)
}
function fmtDate(s?: string): string {
  return s ? new Date(s).toLocaleString() : ''
}

// ── 多选 ─────────────────────────────────────────────────────────────────
function toggleSelect(id: string) {
  if (selected.has(id)) selected.delete(id)
  else selected.add(id)
}
const allSelected = computed(() => filtered.value.length > 0 && filtered.value.every((s) => selected.has(s.id)))
function toggleSelectAll() {
  const target = !allSelected.value
  for (const s of filtered.value) {
    if (target) selected.add(s.id)
    else selected.delete(s.id)
  }
}

// ── 详情 ─────────────────────────────────────────────────────────────────
async function openDetail(id: string) {
  activeId.value = id
  detail.value = null
  detailState.value = t('codex.conv.loading')
  try {
    const j = await codexApi.getConversation(id)
    detail.value = j || null
    detailState.value = ''
  } catch (e) {
    detailState.value = (e as Error).message || String(e)
  }
}

// 8 种 item 类型 → 渲染描述符(逐字移植 codexConversationsItemDetailHtml)
interface RenderedItem {
  kind: 'message' | 'collapse-md' | 'collapse-tool' | 'compacted' | 'skip'
  role?: string
  summary?: string
  html?: string
  text?: string
}
function renderItem(item: ConversationItem): RenderedItem {
  const type = (item.type || '').toLowerCase()
  if (type === 'user') return { kind: 'message', role: t('codex.conv.roleUser'), html: renderMiniMd(item.text || '') }
  if (type === 'assistant') return { kind: 'message', role: t('codex.conv.roleAssistant'), html: renderMiniMd(item.text || '') }
  if (type === 'reasoning') return { kind: 'collapse-md', summary: t('codex.conv.reasoning'), html: renderMiniMd(item.text || '') }
  if (type === 'toolcall') return { kind: 'collapse-tool', summary: `🔧 ${item.name || ''}`, text: String(item.arguments ?? '') }
  if (type === 'tooloutput') return { kind: 'collapse-tool', summary: '↳ output', text: truncateString(item.output || '', 4000) }
  if (type === 'compacted') return { kind: 'compacted', summary: t('codex.conv.compacted'), html: renderMiniMd(item.summary || '') }
  if (type === 'system') return { kind: 'collapse-md', summary: `[${item.role || 'system'}]`, html: renderMiniMd(item.text || '') }
  return { kind: 'skip' }
}
function turnItems(items?: ConversationItem[]): RenderedItem[] {
  return (items || []).map(renderItem).filter((r) => r.kind !== 'skip')
}

// ── 导出 ─────────────────────────────────────────────────────────────────
function downloadBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

async function exportSelected() {
  if (selected.size === 0) return
  const ids = [...selected]
  const isMulti = ids.length > 1
  const tsTag = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)
  const ext = format.value === 'markdown' ? 'md' : format.value === 'jsonl' ? 'jsonl' : 'json'
  let defaultName: string
  if (isMulti) {
    defaultName = `codex-conversations-${tsTag}.zip`
  } else {
    const meta = sessions.value.find((s) => s.id === ids[0])
    const baseName = meta?.path?.split('/').pop()?.replace(/\.jsonl$/, '') || `session-${ids[0].slice(0, 8)}`
    defaultName = `${baseName}.${ext}`
  }

  // 优先用默认导出目录;留空才弹 Tauri save dialog
  let targetPath: string | undefined
  const dir = loadDefaultDir()
  if (dir) {
    const sep = dir.endsWith('/') || dir.endsWith('\\') ? '' : '/'
    targetPath = `${dir}${sep}${defaultName}`
  } else {
    const dialog = tauriDialog()
    if (!dialog?.save) {
      toast(`${t('codex.conv.exportFailed')}: Tauri dialog API 不可用`, 'error')
      return
    }
    try {
      const saved = await dialog.save({
        title: isMulti ? t('codex.conv.saveDialogMulti') : t('codex.conv.saveDialogSingle'),
        defaultPath: defaultName,
        filters: [{ name: (isMulti ? 'zip' : ext).toUpperCase(), extensions: [isMulti ? 'zip' : ext] }],
      })
      if (!saved) return
      targetPath = saved
    } catch (e) {
      toast(`${t('codex.conv.exportFailed')}: ${(e as Error).message || e}`, 'error')
      return
    }
  }

  try {
    const res = await codexApi.exportConversations({
      sessionIds: ids,
      format: format.value,
      options: { ...exportOptions },
      targetPath,
    })
    if (res.kind === 'json') {
      const path = (res.data as { path?: string }).path || targetPath
      toast(tFmt('codex.conv.toastExportedTo', { count: ids.length, path: path || '' }))
    } else {
      downloadBlob(res.blob, res.filename)
      toast(tFmt('codex.conv.toastExported', { count: ids.length }))
    }
  } catch (e) {
    toast(`${t('codex.conv.exportFailed')}: ${(e as Error).message || e}`, 'error')
  }
}

// ── 删除 ─────────────────────────────────────────────────────────────────
async function deleteSelected() {
  if (selected.size === 0) return
  const ids = [...selected]
  if (!window.confirm(tFmt('codex.conv.confirmDelete', { count: ids.length }))) return
  try {
    const data = await codexApi.deleteConversations(ids)
    const moved = (data.deleted || []).length
    const failedItems = data.failed || []
    selected.clear()
    if (failedItems.length > 0) {
      toast(tFmt('codex.conv.toastDeletedPartial', { moved, failed: failedItems.length }))
      const sample = failedItems
        .slice(0, 3)
        .map((f) => `  - ${f.sessionId}: ${f.reason}`)
        .join('\n')
      const more = failedItems.length > 3 ? `\n  ... +${failedItems.length - 3} more` : ''
      window.alert(`${t('codex.conv.deleteFailureDetail')}:\n${sample}${more}`)
    } else {
      toast(tFmt('codex.conv.toastDeleted', { count: moved }))
    }
    await load()
  } catch (e) {
    toast(`${t('codex.conv.deleteFailed')}: ${(e as Error).message || e}`, 'error')
  }
}

const exportLabel = computed(() =>
  selected.size > 0 ? tFmt('codex.conv.exportSelectedN', { count: selected.size }) : t('codex.conv.exportSelected'),
)
const deleteLabel = computed(() =>
  selected.size > 0 ? tFmt('codex.conv.deleteSelectedN', { count: selected.size }) : t('codex.conv.deleteSelected'),
)

onMounted(() => {
  defaultDir.value = loadDefaultDir()
  load()
})
</script>

<template>
  <div class="conv">
    <!-- 工具栏 -->
    <div class="conv__toolbar">
      <AppInput v-model="search" :placeholder="t('codex.conv.searchPlaceholder')" class="conv__search" />
      <AppSelect v-model="kindFilter" :options="kindOptions" class="conv__sel conv__sel--kind" />
      <AppSelect v-model="cwdFilter" :options="cwdSelectOptions" align="right" class="conv__sel conv__sel--cwd" />
      <AppButton size="sm" :icon="IconRefresh" :label="t('codex.conv.refresh')" @click="load" />
    </div>

    <div class="conv__actions">
      <label class="conv__selectall">
        <input type="checkbox" :checked="allSelected" @change="toggleSelectAll" />
        <span>{{ t('codex.conv.selectAll') }}</span>
      </label>
      <span class="conv__spacer" />
      <AppSelect v-model="format" :options="formatOptions" align="right" class="conv__sel conv__sel--fmt" />
      <!-- 「选项」内联下拉(替代弹窗;开关直接绑 exportOptions,改即生效)-->
      <AppDropdown align="right" panel-width="320px" class="conv__opts">
        <template #trigger>
          <AppButton size="sm" :icon="IconSettings" :label="t('codex.conv.options')" />
        </template>
        <div class="conv__opts-panel">
          <SettingsRow :title="t('codex.conv.optIncludeReasoning')">
            <AppSwitch v-model="exportOptions.includeReasoning" />
          </SettingsRow>
          <SettingsRow :title="t('codex.conv.optIncludeToolCalls')">
            <AppSwitch v-model="exportOptions.includeToolCalls" />
          </SettingsRow>
          <SettingsRow :title="t('codex.conv.optIncludeSystem')">
            <AppSwitch v-model="exportOptions.includeSystemPrompts" />
          </SettingsRow>
          <SettingsRow :title="t('codex.conv.optRedact')">
            <AppSwitch v-model="exportOptions.redactSecrets" />
          </SettingsRow>
          <SettingsRow :title="t('codex.conv.optToolMax')">
            <input v-model.number="exportOptions.toolOutputMaxChars" type="number" min="100" max="200000" step="256" class="conv__num" />
          </SettingsRow>
        </div>
      </AppDropdown>
      <AppButton class="conv__act-btn" size="sm" :icon="IconDownload" :label="exportLabel" :disabled="!selected.size" @click="exportSelected" />
      <AppButton class="conv__act-btn" size="sm" variant="danger" :icon="IconTrash" :label="deleteLabel" :disabled="!selected.size" @click="deleteSelected" />
    </div>

    <p v-if="!loading" class="conv__summary">{{ tFmt('codex.conv.summary', { count: sessions.length }) }}</p>

    <!-- 列表 -->
    <div class="conv__list">
      <div v-if="loading" class="conv__state">{{ t('codex.conv.loading') }}</div>
      <div v-else-if="!filtered.length" class="conv__state">{{ t('codex.conv.noResults') }}</div>
      <div
        v-for="s in filtered"
        :key="s.id"
        class="conv__item"
        :class="{ active: activeId === s.id }"
        @click="openDetail(s.id)"
      >
        <input
          type="checkbox"
          class="conv__check"
          :checked="selected.has(s.id)"
          @click.stop
          @change="toggleSelect(s.id)"
        />
        <div class="conv__item-body">
          <div class="conv__item-top">
            <span class="conv__item-title" :title="titleOf(s)">{{ titleOf(s) }}</span>
            <span class="conv__kind" :class="s.kind === 'active' ? 'active' : 'archived'">{{
              s.kind === 'active' ? 'active' : 'archived'
            }}</span>
          </div>
          <div class="conv__item-meta">
            <span>{{ fmtDate(s.createdAt) }}</span>
            <span v-if="s.cwd">· {{ s.cwd.split('/').pop() }}</span>
            <span>· {{ s.turnCount }} {{ t('codex.conv.turns') }}</span>
            <span v-if="s.modelProvider">· {{ s.modelProvider }}</span>
          </div>
        </div>
      </div>
    </div>

    <!-- 详情 -->
    <div v-if="activeId" class="conv__detail">
      <div v-if="detailState" class="conv__state">{{ detailState }}</div>
      <template v-else-if="detail">
        <h3 class="conv__detail-title">{{ detail.meta?.title || fallbackTitle(detail.meta || {}) }}</h3>
        <div class="conv__detail-meta">
          <div>ID: <code>{{ detail.meta?.id || '' }}</code></div>
          <div>{{ detail.meta?.cwd || '' }} · {{ detail.meta?.originator || '' }} · {{ detail.meta?.modelProvider || '' }}</div>
        </div>
        <div v-for="(turn, ti) in detail.turns || []" :key="ti" class="conv__turn">
          <div class="conv__turn-head">Turn {{ ti + 1 }}</div>
          <template v-for="(r, ri) in turnItems(turn.items)" :key="ri">
            <div v-if="r.kind === 'message'" class="conv__msg">
              <div class="conv__role">{{ r.role }}</div>
              <div class="conv__text codex-md" v-html="r.html"></div>
            </div>
            <details v-else-if="r.kind === 'collapse-md'" class="conv__collapse">
              <summary>{{ r.summary }}</summary>
              <div class="conv__text codex-md" v-html="r.html"></div>
            </details>
            <details v-else-if="r.kind === 'collapse-tool'" class="conv__collapse">
              <summary>{{ r.summary }}</summary>
              <div class="conv__text conv__tool">{{ r.text }}</div>
            </details>
            <div v-else-if="r.kind === 'compacted'" class="conv__compacted">
              📦 {{ r.summary }}: <span class="codex-md" v-html="r.html"></span>
            </div>
          </template>
        </div>
      </template>
    </div>

    <!-- 默认导出目录 -->
    <div class="conv__defaultdir">
      <SettingsRow title="默认导出文件夹" :description="defaultDir || t('codex.conv.defaultDirPlaceholder')">
        <div class="conv__dir-actions">
          <AppButton size="sm" :label="t('codex.conv.defaultDirPickTitle')" @click="pickDefaultDir" />
          <AppButton v-if="defaultDir" size="sm" variant="ghost" label="清除" @click="clearDefaultDir" />
        </div>
      </SettingsRow>
    </div>

  </div>
</template>

<style scoped>
.conv {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  flex: 1;
  min-height: 0;
}
.conv__warn {
  margin: 0;
  padding: var(--space-2) var(--space-3);
  background: var(--warning-soft);
  border-radius: var(--radius);
  color: var(--text-secondary);
  font-size: var(--fs-xs);
  line-height: 1.5;
}
.conv__toolbar,
.conv__actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}
.conv__search {
  flex: 1;
  min-width: 160px;
}
.conv__search :deep(.app-input) {
  width: 100%;
}
.conv__sel--kind {
  width: 104px;
}
.conv__sel--cwd {
  width: 168px;
}
.conv__sel--fmt {
  width: 152px;
}
/* 导出/删除按钮固定宽度:label 带选中数变化时不再改变宽度(修原重叠 bug) */
.conv__act-btn {
  min-width: 122px;
  flex-shrink: 0;
  justify-content: center;
}
/* 「选项」内联面板的开关行 */
.conv__opts-panel :deep(.settings-row) {
  padding: var(--space-3);
}
.conv__opts-panel :deep(.settings-row + .settings-row) {
  border-top: 1px solid var(--border);
}
.conv__selectall {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--fs-sm);
  color: var(--text-secondary);
}
.conv__spacer {
  flex: 1;
}
.conv__summary {
  margin: 0;
  font-size: var(--fs-xs);
  color: var(--text-muted);
}

/* list */
/* 列表填满剩余空间 + 框内滚;「默认导出文件夹」常驻底部,底部间隙恒定 = 内边距 */
.conv__list {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
  flex: 1;
  min-height: 160px;
  overflow-y: auto;
}
.conv__state {
  padding: var(--space-5);
  text-align: center;
  color: var(--text-muted);
  font-size: var(--fs-sm);
}
.conv__item {
  display: flex;
  align-items: flex-start;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  background: var(--surface);
  cursor: pointer;
}
.conv__item + .conv__item {
  border-top: 1px solid var(--border);
}
.conv__item:hover {
  background: var(--surface-hover);
}
.conv__item.active {
  background: var(--accent-soft);
}
.conv__check {
  margin-top: 3px;
}
.conv__item-body {
  flex: 1;
  min-width: 0;
}
.conv__item-top {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.conv__item-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--fs-sm);
  font-weight: 550;
}
.conv__kind {
  flex-shrink: 0;
  padding: 1px 7px;
  border-radius: var(--radius-full);
  font-size: 10px;
  font-weight: 600;
}
.conv__kind.active {
  background: var(--success-soft);
  color: var(--success);
}
.conv__kind.archived {
  background: var(--surface-2);
  color: var(--text-muted);
}
.conv__item-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-top: 2px;
  font-size: var(--fs-xs);
  color: var(--text-muted);
}

/* detail */
.conv__detail {
  flex-shrink: 0;
  max-height: 42vh;
  overflow-y: auto;
  padding: var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
}
.conv__detail-title {
  margin: 0 0 var(--space-2);
  font-size: var(--fs-md);
  font-weight: 600;
}
.conv__detail-meta {
  margin-bottom: var(--space-3);
  font-size: var(--fs-xs);
  color: var(--text-muted);
}
.conv__detail-meta code {
  font-family: var(--font-mono);
}
.conv__turn {
  margin-top: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}
.conv__turn-head {
  font-size: var(--fs-xs);
  font-weight: 600;
  color: var(--text-muted);
  margin-bottom: var(--space-2);
}
.conv__msg {
  margin: var(--space-2) 0;
}
.conv__role {
  font-size: var(--fs-xs);
  font-weight: 600;
  color: var(--accent);
  margin-bottom: 2px;
}
.conv__collapse {
  margin: var(--space-2) 0;
}
.conv__collapse summary {
  cursor: pointer;
  font-size: var(--fs-sm);
  color: var(--text-secondary);
}
.conv__text {
  font-size: var(--fs-sm);
  line-height: 1.6;
}
.conv__tool {
  margin-top: var(--space-2);
  padding: var(--space-2) var(--space-3);
  background: var(--surface-2);
  border-radius: var(--radius-sm);
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  white-space: pre-wrap;
  word-break: break-all;
}
.conv__compacted {
  margin: var(--space-2) 0;
  padding: var(--space-2) var(--space-3);
  background: var(--surface-2);
  border-radius: var(--radius-sm);
  font-size: var(--fs-sm);
}

/* default dir + modal */
.conv__defaultdir {
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.conv__dir-actions {
  display: flex;
  gap: var(--space-2);
}
.conv__num {
  width: 100px;
  height: 30px;
  padding: 0 var(--space-2);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-sm);
}

/* 渲染 markdown(v-html)*/
.codex-md :deep(p) {
  margin: var(--space-1) 0;
  line-height: 1.6;
}
.codex-md :deep(pre.codex-md-code) {
  margin: var(--space-2) 0;
  padding: var(--space-2);
  overflow-x: auto;
  background: var(--surface-2);
  border-radius: var(--radius-sm);
}
.codex-md :deep(code) {
  padding: 1px 4px;
  border-radius: var(--radius-sm);
  background: var(--surface-2);
  font-family: var(--font-mono);
  font-size: 0.92em;
}
.codex-md :deep(a) {
  color: var(--accent);
}
.codex-md :deep(ul),
.codex-md :deep(ol) {
  margin: var(--space-1) 0;
  padding-left: var(--space-5);
}
</style>
