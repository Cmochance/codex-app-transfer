<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import * as codexApi from '@/api/codex'
import type { ManagedResource, ManagedPathEntry, ManagedHistoryEntry } from '@/api/codex'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useConfirm } from '@/composables/useConfirm'
import { renderMiniMd } from '@/lib/miniMd'
import AppButton from '@/components/ui/AppButton.vue'
import AppModal from '@/components/ui/AppModal.vue'
import AppInput from '@/components/ui/AppInput.vue'
import HistoryModal from './HistoryModal.vue'
import IconChevron from '~icons/lucide/chevron-down'
import IconPlus from '~icons/lucide/plus'
import IconX from '~icons/lucide/x'
import IconPencil from '~icons/lucide/pencil'
import IconArchive from '~icons/lucide/archive'
import IconHistory from '~icons/lucide/history'
import IconFolderOpen from '~icons/lucide/folder-open'

// agents/memories/skills 参数化面板(逐字移植旧 app.js 三套近重复)。
// 每实例自管一个 resource 的状态 — 彻底消除旧 codexDocActiveResource 全局态 bug 源。
const props = defineProps<{ resource: ManagedResource }>()
const { show: toast } = useToast()
const { confirm } = useConfirm()

interface FeatureConfig {
  pathManagement: boolean // 添加/删除/浏览 路径(skills 无)
  reveal: boolean // 在文件管理器打开(skills 独有)
  browseDir: boolean // browse 选目录(memories)还是文件(agents)
  docName: string // apply 二次确认显示的文档名
}
const FEATURES: Record<ManagedResource, FeatureConfig> = {
  agents: { pathManagement: true, reveal: false, browseDir: false, docName: 'AGENTS.md' },
  memories: { pathManagement: true, reveal: false, browseDir: true, docName: 'MEMORY.md' },
  skills: { pathManagement: false, reveal: true, browseDir: false, docName: 'SKILL.md' },
}
const EMPTY_KEY: Record<ManagedResource, string> = {
  agents: 'codex.agentsPathEmpty',
  memories: 'codex.memoriesPathEmpty',
  skills: 'codex.skillsEmpty',
}
const features = computed(() => FEATURES[props.resource])
const emptyText = computed(() => t(EMPTY_KEY[props.resource]))

// ── 状态(本地,KeepAlive 跨子 tab 切换保活)─────────────────────────────
const entries = ref<ManagedPathEntry[]>([])
const currentHash = ref<string | null>(null)
const pickerOpen = ref(false)
const rawContent = ref('')
const mode = ref<'preview' | 'edit'>('preview')
const editDraft = ref('')

const currentEntry = computed(() => entries.value.find((e) => e.hash === currentHash.value) || null)
const renderedMd = computed(() => renderMiniMd(rawContent.value))
// 删除按钮:有选中且非全局(全局文件后端也删不掉,统一隐藏)
const showRemoveBtn = computed(
  () => features.value.pathManagement && !!currentEntry.value && currentEntry.value.category !== 'global',
)

// chip 渲染:三 resource 各异(category / 文件名 / name)
interface Chip {
  text: string
  kind: 'global' | 'project-root' | 'subdir'
}
function chipsFor(entry: ManagedPathEntry): Chip[] {
  if (props.resource === 'skills') {
    return [{ text: entry.name || '?', kind: 'project-root' }]
  }
  if (props.resource === 'memories') {
    const filename = (entry.path || '').split('/').pop() || ''
    if (filename === 'MEMORY.md') return [{ text: t('codex.memoriesPath.index'), kind: 'global' }]
    if (filename === 'memory_summary.md') return [{ text: t('codex.memoriesPath.summary'), kind: 'project-root' }]
    return [{ text: filename, kind: 'project-root' }]
  }
  // agents
  if (entry.category === 'global') return [{ text: t('codex.agentsPath.global'), kind: 'global' }]
  if (entry.category === 'project-root') return [{ text: entry.projectName || '?', kind: 'project-root' }]
  return [
    { text: entry.projectName || '?', kind: 'project-root' },
    { text: entry.subdirPath || '?', kind: 'subdir' },
  ]
}

// ── 数据加载 ─────────────────────────────────────────────────────────────
async function reloadPaths() {
  try {
    const j = await codexApi.getManagedPaths(props.resource)
    entries.value = j.entries || []
    if (!currentHash.value || !entries.value.some((e) => e.hash === currentHash.value)) {
      currentHash.value = entries.value[0]?.hash || null
    }
  } catch (e) {
    console.error('reloadPaths', e)
    entries.value = []
  }
}

async function rawLoad() {
  mode.value = 'preview'
  if (!currentHash.value) {
    rawContent.value = ''
    editDraft.value = ''
    return
  }
  try {
    const j = await codexApi.getManagedRaw(props.resource, currentHash.value)
    rawContent.value = j.content || ''
    editDraft.value = rawContent.value
  } catch (e) {
    rawContent.value = ''
    toast(`读取失败: ${(e as Error).message || e}`, 'error')
  }
}

async function selectHash(hash: string) {
  if (!hash) return
  currentHash.value = hash
  pickerOpen.value = false
  await rawLoad()
}

function togglePicker() {
  if (!entries.value.length) return
  pickerOpen.value = !pickerOpen.value
}

// ── 编辑 / 备份 ──────────────────────────────────────────────────────────
function onEditStart() {
  if (!currentHash.value) {
    toast(emptyText.value)
    return
  }
  editDraft.value = rawContent.value
  mode.value = 'edit'
}
function onCancel() {
  mode.value = 'preview'
}
async function onApply() {
  if (!currentHash.value) return
  if (!(await confirm({ message: tFmt('codex.docApplyConfirm', { doc: features.value.docName }), danger: true })))
    return
  try {
    await codexApi.saveManagedRaw(props.resource, currentHash.value, editDraft.value)
    toast(tFmt('codex.docApplyOk', { doc: features.value.docName }))
    await rawLoad()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function onBackup() {
  if (!currentHash.value) {
    toast(emptyText.value)
    return
  }
  if (!(await confirm(t('codex.backupConfirm')))) return
  try {
    await codexApi.backupManaged(props.resource, currentHash.value)
    toast(tFmt('codex.docBackupOk', { doc: features.value.docName }))
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function onReveal() {
  if (!currentHash.value) {
    toast(emptyText.value)
    return
  }
  try {
    await codexApi.revealManaged(props.resource, currentHash.value)
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

// ── 路径增删(agents/memories)──────────────────────────────────────────
const pathModalOpen = ref(false)
const pathInput = ref('')
const pathModalTitleKey = computed(() =>
  props.resource === 'memories' ? 'codex.memoriesPathAddTitle' : 'codex.agentsPathAddTitle',
)
const pathModalPromptKey = computed(() =>
  props.resource === 'memories' ? 'codex.memoriesPathAddPrompt' : 'codex.agentsPathAddPrompt',
)

function openPathAdd() {
  pathInput.value = ''
  pathModalOpen.value = true
}

interface TauriDialog {
  open(opts: Record<string, unknown>): Promise<string | string[] | null>
}
function tauriDialog(): TauriDialog | null {
  const w = window as unknown as { __TAURI__?: { dialog?: TauriDialog } }
  return w.__TAURI__?.dialog ?? null
}

async function onBrowse() {
  const dialog = tauriDialog()
  if (!dialog || typeof dialog.open !== 'function') {
    toast('Tauri dialog API 不可用 — 请直接粘贴绝对路径', 'error')
    return
  }
  const raw = pathInput.value.trim()
  const defaultPath = raw && raw.startsWith('/') ? raw : undefined
  const dir = features.value.browseDir
  try {
    const selected = await dialog.open({
      title: t(pathModalTitleKey.value),
      multiple: false,
      directory: dir,
      defaultPath,
      filters: dir
        ? undefined
        : [
            { name: 'AGENTS.md', extensions: ['md', 'MD'] },
            { name: 'All files', extensions: ['*'] },
          ],
    })
    if (typeof selected === 'string' && selected) pathInput.value = selected
  } catch (e) {
    toast((e as Error).message || 'dialog open failed', 'error')
  }
}

async function confirmPathAdd() {
  const path = pathInput.value.trim()
  if (!path) {
    toast(t('codex.agentsPathAddEmpty'), 'error')
    return
  }
  try {
    const j = await codexApi.addManagedPath(props.resource, path)
    currentHash.value = j.entry?.hash || null
    pathModalOpen.value = false
    await reloadPaths()
    await rawLoad()
    toast(t('codex.agentsPathAddOk'))
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

async function onPathRemove() {
  if (!currentHash.value) return
  if (!(await confirm({ message: t('codex.agentsPathRemoveConfirm'), danger: true }))) return
  try {
    await codexApi.removeManagedPath(props.resource, currentHash.value)
    currentHash.value = null
    await reloadPaths()
    await rawLoad()
    toast(t('codex.agentsPathRemoveOk'))
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

// ── 历史快照 ─────────────────────────────────────────────────────────────
const showHistory = ref(false)
const historyEntries = ref<ManagedHistoryEntry[]>([])

const historyPrefix = computed(() => {
  const cur = currentEntry.value
  if (!cur) return ''
  if (props.resource === 'skills') return cur.name || '?'
  if (props.resource === 'memories') return (cur.path || '').split('/').pop() || ''
  if (cur.category === 'global') return t('codex.agentsPath.global')
  if (cur.category === 'project-root') return cur.projectName || '?'
  return `${cur.projectName || '?'} / ${cur.subdirPath || '?'}`
})

async function openHistory() {
  if (!currentHash.value) {
    toast(emptyText.value)
    return
  }
  try {
    const j = await codexApi.getManagedHistory(props.resource, currentHash.value)
    historyEntries.value = (j.history || []).slice().reverse() // 最新在前
    showHistory.value = true
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

async function onHistoryRestore(index: number) {
  if (!(await confirm({ message: tFmt('codex.docRestoreConfirm', { doc: features.value.docName }), danger: true })))
    return
  try {
    await codexApi.restoreManagedRaw(props.resource, currentHash.value, index)
    toast(tFmt('codex.docRestoreOk', { doc: features.value.docName }))
    showHistory.value = false
    await rawLoad()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

// ── 生命周期 ─────────────────────────────────────────────────────────────
async function init() {
  await reloadPaths()
  await rawLoad()
}
onMounted(init)
// resource 不会变(每实例固定),但 key 切换时 KeepAlive 复用 → 防御性 watch
watch(
  () => props.resource,
  () => init(),
)
</script>

<template>
  <div class="mmp">
    <div class="mmp__toolbar">
      <!-- path picker -->
      <div class="mmp__picker" :class="{ open: pickerOpen }">
        <button type="button" class="mmp__toggle" @click="togglePicker">
          <template v-if="currentEntry">
            <span
              v-for="(chip, i) in chipsFor(currentEntry)"
              :key="i"
              class="chip"
              :class="`chip--${chip.kind}`"
              >{{ chip.text }}</span
            >
            <span class="mmp__path" :title="currentEntry.path">{{ currentEntry.path }}</span>
          </template>
          <span v-else class="mmp__empty">{{ emptyText }}</span>
          <IconChevron class="mmp__caret" />
        </button>
        <ul v-show="pickerOpen && entries.length" class="mmp__menu">
          <li
            v-for="e in entries"
            :key="e.hash"
            class="mmp__item"
            :class="{ selected: e.hash === currentHash }"
            @click="selectHash(e.hash)"
          >
            <span v-for="(chip, i) in chipsFor(e)" :key="i" class="chip" :class="`chip--${chip.kind}`">{{
              chip.text
            }}</span>
            <span class="mmp__path" :title="e.path">{{ e.path }}</span>
          </li>
        </ul>
      </div>
      <!-- 路径增删 -->
      <div v-if="features.pathManagement" class="mmp__path-actions">
        <AppButton size="sm" :icon="IconPlus" :label="t('codex.agentsPathAdd')" @click="openPathAdd" />
        <AppButton v-if="showRemoveBtn" size="sm" variant="danger" :icon="IconX" @click="onPathRemove" />
      </div>
    </div>

    <!-- preview / edit -->
    <div class="mmp__doc">
      <div v-show="mode === 'preview'" class="mmp__preview codex-md">
        <div v-if="rawContent" v-html="renderedMd"></div>
        <div v-else class="mmp__doc-empty">{{ emptyText }}</div>
      </div>
      <textarea
        v-show="mode === 'edit'"
        v-model="editDraft"
        class="mmp__edit"
        spellcheck="false"
      ></textarea>
    </div>

    <!-- 操作按钮 -->
    <div class="mmp__actions">
      <template v-if="mode === 'preview'">
        <AppButton size="sm" :icon="IconPencil" :label="t('codex.agentsEdit')" @click="onEditStart" />
        <AppButton size="sm" :icon="IconArchive" :label="t('codex.agentsBackup')" @click="onBackup" />
        <AppButton size="sm" :icon="IconHistory" :label="t('codex.history')" @click="openHistory" />
        <AppButton
          v-if="features.reveal"
          size="sm"
          :icon="IconFolderOpen"
          :label="t('codex.skillsReveal')"
          @click="onReveal"
        />
      </template>
      <template v-else>
        <AppButton variant="ghost" :label="t('codex.agentsCancel')" @click="onCancel" />
        <AppButton variant="primary" :label="t('codex.apply')" @click="onApply" />
      </template>
    </div>

    <!-- 历史快照 modal -->
    <HistoryModal
      v-if="showHistory"
      :entries="historyEntries"
      :current-content="rawContent"
      :label-prefix="historyPrefix"
      @close="showHistory = false"
      @restore="onHistoryRestore"
    />

    <!-- 添加路径 modal -->
    <AppModal v-if="pathModalOpen" :title="t(pathModalTitleKey)" @close="pathModalOpen = false">
      <p class="mmp__add-desc">{{ t(pathModalPromptKey) }}</p>
      <div class="mmp__add-row">
        <AppInput
          v-model="pathInput"
          :placeholder="features.browseDir ? '/path/to/project-root' : '/path/to/AGENTS.md'"
        />
        <AppButton size="sm" :label="t('codex.agentsPathBrowse')" @click="onBrowse" />
      </div>
      <div class="mmp__add-actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="pathModalOpen = false" />
        <AppButton variant="primary" :label="t('codex.agentsPathAddOkBtn')" @click="confirmPathAdd" />
      </div>
    </AppModal>
  </div>
</template>

<style scoped>
.mmp {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.mmp__toolbar {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}

/* path picker */
.mmp__picker {
  position: relative;
  flex: 1;
  min-width: 0;
}
.mmp__toggle {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  width: 100%;
  height: 32px;
  padding: 0 var(--space-3);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-sm);
  text-align: left;
}
.mmp__toggle:hover {
  background: var(--surface-hover);
}
.mmp__path {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-secondary);
}
.mmp__empty {
  flex: 1;
  color: var(--text-muted);
}
.mmp__caret {
  flex-shrink: 0;
  width: 16px;
  height: 16px;
  color: var(--text-muted);
  transition: transform var(--transition);
}
.mmp__picker.open .mmp__caret {
  transform: rotate(180deg);
}
.mmp__menu {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  right: 0;
  z-index: 20;
  margin: 0;
  padding: var(--space-1);
  list-style: none;
  max-height: 280px;
  overflow-y: auto;
  background: var(--surface);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  box-shadow: var(--shadow-lg);
}
.mmp__item {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.mmp__item:hover {
  background: var(--surface-hover);
}
.mmp__item.selected {
  background: var(--accent-soft);
}

/* chips */
.chip {
  flex-shrink: 0;
  padding: 1px 8px;
  border-radius: var(--radius-full);
  font-size: var(--fs-xs);
  font-weight: 600;
  white-space: nowrap;
}
.chip--global {
  background: var(--accent-soft);
  color: var(--accent-text);
}
.chip--project-root {
  background: var(--success-soft);
  color: var(--success);
}
.chip--subdir {
  background: var(--warning-soft);
  color: var(--warning);
}

.mmp__path-actions {
  display: flex;
  gap: var(--space-2);
}

/* doc area */
.mmp__doc {
  min-height: 240px;
}
.mmp__preview {
  min-height: 240px;
  max-height: calc(100vh - 300px);
  overflow-y: auto;
  padding: var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  font-size: var(--fs-sm);
}
.mmp__doc-empty {
  color: var(--text-muted);
  text-align: center;
  padding: var(--space-6) 0;
}
.mmp__edit {
  width: 100%;
  min-height: 240px;
  height: calc(100vh - 300px);
  padding: var(--space-4);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-lg);
  background: var(--surface);
  color: var(--text);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  line-height: 1.6;
  resize: vertical;
}
.mmp__edit:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}

.mmp__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}

/* add-path modal */
.mmp__add-desc {
  margin: 0 0 var(--space-3);
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  line-height: 1.5;
}
.mmp__add-row {
  display: flex;
  gap: var(--space-2);
  align-items: center;
}
.mmp__add-row :deep(.app-input) {
  flex: 1;
  width: auto;
}
.mmp__add-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}

/* 渲染 markdown(v-html 内容,需 :deep)*/
.codex-md :deep(h1),
.codex-md :deep(h2),
.codex-md :deep(h3),
.codex-md :deep(h4) {
  margin: var(--space-3) 0 var(--space-2);
  font-weight: 600;
  line-height: 1.3;
}
.codex-md :deep(h1) {
  font-size: var(--fs-lg);
}
.codex-md :deep(h2) {
  font-size: var(--fs-md);
}
.codex-md :deep(p) {
  margin: var(--space-2) 0;
  line-height: 1.6;
}
.codex-md :deep(ul),
.codex-md :deep(ol) {
  margin: var(--space-2) 0;
  padding-left: var(--space-5);
}
.codex-md :deep(li) {
  margin: 2px 0;
}
.codex-md :deep(code) {
  padding: 1px 5px;
  border-radius: var(--radius-sm);
  background: var(--surface-2);
  font-family: var(--font-mono);
  font-size: 0.92em;
}
.codex-md :deep(pre.codex-md-code) {
  margin: var(--space-2) 0;
  padding: var(--space-3);
  overflow-x: auto;
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.codex-md :deep(pre.codex-md-code code) {
  padding: 0;
  background: transparent;
}
.codex-md :deep(blockquote) {
  margin: var(--space-2) 0;
  padding-left: var(--space-3);
  border-left: 3px solid var(--border-strong);
  color: var(--text-secondary);
}
.codex-md :deep(a) {
  color: var(--accent);
}
.codex-md :deep(hr) {
  margin: var(--space-3) 0;
  border: none;
  border-top: 1px solid var(--border);
}
</style>
