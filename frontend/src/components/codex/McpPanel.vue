<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import * as codexApi from '@/api/codex'
import type { McpServerSpec, McpPlugin, ManagedHistoryEntry, PluginSkill } from '@/api/codex'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useConfirm } from '@/composables/useConfirm'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppModal from '@/components/ui/AppModal.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import HistoryModal from './HistoryModal.vue'
import ConnectorMarket from './ConnectorMarket.vue'
import IconPlus from '~icons/lucide/plus'
import IconTrash from '~icons/lucide/trash-2'
import IconArchive from '~icons/lucide/archive'
import IconHistory from '~icons/lucide/history'
import IconFileCode from '~icons/lucide/file-code-2'
import IconPencil from '~icons/lucide/pencil'
import IconCheck from '~icons/lucide/circle-check-big'
import IconSparkles from '~icons/lucide/sparkles'

const { show: toast } = useToast()
const { confirm } = useConfirm()

type Subpane = 'servers' | 'plugins' | 'marketplace'
const subpane = ref<Subpane>('servers')
const subpaneOptions: { value: Subpane; label: string }[] = [
  { value: 'servers', label: t('codex.mcp.servers') },
  { value: 'plugins', label: t('codex.mcp.plugins') },
  { value: 'marketplace', label: t('codex.mcp.marketplace') },
]

// ── Servers ──────────────────────────────────────────────────────────────
const servers = ref<McpServerSpec[]>([])
const currentServerName = ref<string | null>(null) // '__new__' 为新增哨兵
const pendingNewName = ref<string | null>(null)
const jsonEditMode = ref(false)
const jsonDraft = ref('')
const jsonError = ref('')
const newServerModal = ref(false)
const newServerName = ref('')
const rawWrap = ref(false)
const rawContent = ref('')
const rawSnapshot = ref('')

function emptyServerSpec(): McpServerSpec {
  return { name: '', transport: 'stdio', command: '', args: [], enabled: true }
}

const currentSpec = computed<McpServerSpec | null>(() => {
  if (currentServerName.value === '__new__') return emptyServerSpec()
  if (currentServerName.value) return servers.value.find((s) => s.name === currentServerName.value) || null
  return null
})
const isNewServer = computed(() => currentServerName.value === '__new__')

// spec → pretty JSON(剔除内部字段 + null/空,保留 transport)
function specToJsonText(spec: McpServerSpec): string {
  const out: Record<string, unknown> = {}
  const skip = new Set(['_isNew', 'name', 'disabledReason', 'transport'])
  if (spec.transport) out.transport = spec.transport
  for (const [k, v] of Object.entries(spec)) {
    if (skip.has(k)) continue
    if (v == null) continue
    if (Array.isArray(v) && v.length === 0) continue
    if (typeof v === 'object' && !Array.isArray(v) && Object.keys(v as object).length === 0) continue
    out[k] = v
  }
  return JSON.stringify(out, null, 2)
}

const jsonText = computed(() => {
  const spec = currentSpec.value
  if (!spec) return ''
  return jsonEditMode.value && jsonDraft.value ? jsonDraft.value : specToJsonText(spec)
})

// 写盘前二次确认文案(MOC-106):stdio 列 cmdline+cwd+env;http 列 url+凭据+header。
function buildSaveConfirm(spec: McpServerSpec): string {
  const name = spec.name || ''
  if (spec.transport === 'stdio') {
    const cmdline = [spec.command || '', ...(Array.isArray(spec.args) ? spec.args : [])].join(' ').trim()
    const extras: string[] = []
    if (spec.cwd) extras.push(`cwd: ${spec.cwd}`)
    if (spec.env && typeof spec.env === 'object') {
      const envLines = Object.keys(spec.env).map((k) => `  ${k}=${spec.env![k]}`)
      if (envLines.length) extras.push('env:\n' + envLines.join('\n'))
    }
    const extra = extras.length ? '\n\n' + extras.join('\n') : ''
    return tFmt('codex.mcp.saveConfirmStdio', { name, cmdline, extra })
  }
  const httpExtras: string[] = []
  if (spec.bearerTokenEnvVar) httpExtras.push(`bearer token env var: ${spec.bearerTokenEnvVar}`)
  if (spec.httpHeaders && typeof spec.httpHeaders === 'object') {
    const lines = Object.keys(spec.httpHeaders).map((k) => `  ${k}: ${spec.httpHeaders![k]}`)
    if (lines.length) httpExtras.push('http headers:\n' + lines.join('\n'))
  }
  if (spec.envHttpHeaders && typeof spec.envHttpHeaders === 'object') {
    const lines = Object.keys(spec.envHttpHeaders).map((k) => `  ${k} ← $${spec.envHttpHeaders![k]}`)
    if (lines.length) httpExtras.push('env http headers:\n' + lines.join('\n'))
  }
  const extra = httpExtras.length ? '\n\n' + httpExtras.join('\n') : ''
  return tFmt('codex.mcp.saveConfirmHttp', { name, url: spec.url || '', extra })
}

async function reloadServers() {
  try {
    const j = await codexApi.getMcpServers()
    servers.value = j.servers || []
    if (currentServerName.value && currentServerName.value !== '__new__' && !servers.value.some((s) => s.name === currentServerName.value)) {
      currentServerName.value = null
    }
  } catch (e) {
    console.error('reloadServers', e)
    servers.value = []
  }
}

function selectServer(name: string) {
  currentServerName.value = name
  jsonEditMode.value = false
  jsonDraft.value = ''
  jsonError.value = ''
}

function editToggle() {
  if (jsonEditMode.value) {
    saveJson()
  } else {
    jsonDraft.value = specToJsonText(currentSpec.value!)
    jsonEditMode.value = true
    jsonError.value = ''
  }
}

// JSON.parse + snake_case/camelCase 双解析 → spec(逐字移植 codexMcpJsonSave)
async function saveJson() {
  jsonError.value = ''
  let parsed: Record<string, unknown>
  try {
    parsed = JSON.parse(jsonDraft.value || '{}')
  } catch (e) {
    jsonError.value = 'JSON 解析失败:' + ((e as Error).message || e)
    return
  }
  if (typeof parsed !== 'object' || Array.isArray(parsed) || parsed === null) {
    jsonError.value = 'JSON 必须是一个 object(花括号 {...})'
    return
  }
  const name = isNewServer.value ? pendingNewName.value : currentServerName.value
  if (!name) {
    jsonError.value = 'server 名缺失'
    return
  }
  const p = parsed as Record<string, unknown>
  let transport = p.transport as string | undefined
  if (!transport) {
    if (typeof p.command === 'string' && p.command.length > 0) transport = 'stdio'
    else if (typeof p.url === 'string' && p.url.length > 0) transport = 'streamable_http'
    else transport = 'stdio'
  }
  if (transport !== 'stdio' && transport !== 'streamable_http') {
    jsonError.value = `transport 仅支持 "stdio" 跟 "streamable_http",收到:${transport}`
    return
  }
  const pick = <T,>(camel: string, snake: string): T | undefined =>
    (p[camel] ?? p[snake]) as T | undefined
  const spec: McpServerSpec = {
    name,
    transport,
    command: (p.command as string) ?? undefined,
    args: Array.isArray(p.args) ? (p.args as string[]) : undefined,
    env: p.env && typeof p.env === 'object' ? (p.env as Record<string, string>) : undefined,
    cwd: (p.cwd as string) ?? undefined,
    url: (p.url as string) ?? undefined,
    bearerTokenEnvVar: pick<string>('bearerTokenEnvVar', 'bearer_token_env_var') ?? undefined,
    httpHeaders: pick<Record<string, string>>('httpHeaders', 'http_headers') ?? undefined,
    envHttpHeaders: pick<Record<string, string>>('envHttpHeaders', 'env_http_headers') ?? undefined,
    enabled: p.enabled !== false,
    required: !!p.required,
    supportsParallelToolCalls: !!pick('supportsParallelToolCalls', 'supports_parallel_tool_calls'),
    experimentalEnvironment: pick('experimentalEnvironment', 'experimental_environment'),
    startupTimeoutSec: pick<number>('startupTimeoutSec', 'startup_timeout_sec'),
    toolTimeoutSec: pick<number>('toolTimeoutSec', 'tool_timeout_sec'),
    defaultToolsApprovalMode: pick<string>('defaultToolsApprovalMode', 'default_tools_approval_mode'),
    enabledTools: (Array.isArray(pick('enabledTools', 'enabled_tools')) ? pick('enabledTools', 'enabled_tools') : undefined) as string[] | undefined,
    disabledTools: (Array.isArray(pick('disabledTools', 'disabled_tools')) ? pick('disabledTools', 'disabled_tools') : undefined) as string[] | undefined,
  }
  if (!(await confirm(buildSaveConfirm(spec)))) return
  try {
    await codexApi.saveMcpServer(spec)
    toast(t('codex.mcp.saveOk'))
    currentServerName.value = name
    pendingNewName.value = null
    jsonEditMode.value = false
    jsonDraft.value = ''
    await reloadServers()
  } catch (e) {
    jsonError.value = (e as Error).message || t('toast.requestFailed')
  }
}

async function deleteServer() {
  if (!currentServerName.value || currentServerName.value === '__new__') return
  if (
    !(await confirm({
      message: `确认删除 server "${currentServerName.value}"?(会同步删 ~/.codex/config.toml 对应节)`,
      danger: true,
    }))
  )
    return
  try {
    await codexApi.deleteMcpServer(currentServerName.value)
    currentServerName.value = null
    await reloadServers()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

function openNewServer() {
  newServerName.value = ''
  newServerModal.value = true
}
function confirmNewServer() {
  const name = newServerName.value.trim()
  if (!name) {
    toast('名字不能为空', 'error')
    return
  }
  if (!/^[A-Za-z0-9_.\-]+$/.test(name)) {
    toast('名字仅允许字母数字 / 短横 / 下划线 / 点', 'error')
    return
  }
  if (servers.value.some((s) => s.name === name)) {
    toast(`server "${name}" 已存在`, 'error')
    return
  }
  pendingNewName.value = name
  currentServerName.value = '__new__'
  jsonEditMode.value = true
  jsonDraft.value = JSON.stringify({ transport: 'stdio', command: 'npx', args: [], enabled: true }, null, 2)
  newServerModal.value = false
}

async function backupServers() {
  if (!(await confirm(t('codex.backupConfirm')))) return
  try {
    await codexApi.backupMcpServers()
    toast(tFmt('codex.docBackupOk', { doc: t('codex.mcp.servers') }))
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

const showHistory = ref(false)
const historyEntries = ref<ManagedHistoryEntry[]>([])
async function openServersHistory() {
  try {
    const j = await codexApi.getMcpServersHistory()
    historyEntries.value = (j.history || []).slice().reverse()
    showHistory.value = true
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function onHistoryRestore(index: number) {
  if (!(await confirm({ message: tFmt('codex.docRestoreConfirm', { doc: t('codex.mcp.servers') }), danger: true })))
    return
  try {
    await codexApi.restoreMcpServers(index)
    toast(tFmt('codex.docRestoreOk', { doc: t('codex.mcp.servers') }))
    showHistory.value = false
    await reloadServers()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

async function rawToggle() {
  if (rawWrap.value) {
    rawWrap.value = false
    return
  }
  try {
    const j = await codexApi.getMcpConfigRaw()
    rawSnapshot.value = j.content || ''
    rawContent.value = rawSnapshot.value
    rawWrap.value = true
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function rawApply() {
  if (!(await confirm({ message: t('codex.mcp.rawApplyConfirm'), danger: true }))) return
  try {
    await codexApi.saveMcpConfigRaw(rawContent.value)
    toast(t('codex.mcp.saveOk'))
    rawWrap.value = false
    await reloadServers()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
function rawCancel() {
  rawContent.value = rawSnapshot.value
  rawWrap.value = false
}

// ── Plugins ──────────────────────────────────────────────────────────────
const plugins = ref<McpPlugin[]>([])
async function reloadPlugins() {
  try {
    const j = await codexApi.getMcpPlugins()
    plugins.value = j.plugins || []
  } catch (e) {
    console.error('reloadPlugins', e)
    plugins.value = []
  }
}
async function togglePlugin(p: McpPlugin) {
  try {
    await codexApi.toggleMcpPlugin(p.key, !p.enabled)
    await reloadPlugins()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function uninstallPlugin(p: McpPlugin) {
  if (
    !(await confirm({
      message: `确认卸载 plugin "${p.key}"?会同步删除 ~/.codex/plugins/cache/ 下整个目录`,
      danger: true,
    }))
  )
    return
  try {
    await codexApi.uninstallMcpPlugin(p.key)
    toast(t('codex.mcp.uninstallOk'))
    await reloadPlugins()
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

// ── plugin 图标 + skills 弹窗 ───────────────────────────────────────────────
const pluginFailedIcons = ref<Set<string>>(new Set())
function onPluginIconError(key: string) {
  pluginFailedIcons.value = new Set(pluginFailedIcons.value).add(key)
}

const skillsModal = ref<McpPlugin | null>(null)
const skillNames = ref<string[]>([])
const selectedSkill = ref('')
const skillDoc = ref<PluginSkill | null>(null)
const skillLoading = ref(false)
const skillOptions = computed(() => skillNames.value.map((n) => ({ value: n, label: n })))

function openSkills(p: McpPlugin) {
  skillsModal.value = p
  skillNames.value = p.skillNames || []
  selectedSkill.value = skillNames.value[0] || ''
  loadSkill()
}
async function loadSkill() {
  if (!skillsModal.value || !selectedSkill.value) return
  skillLoading.value = true
  skillDoc.value = null
  try {
    const j = await codexApi.getPluginSkill(skillsModal.value.key, selectedSkill.value)
    skillDoc.value = j.skill || null
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  } finally {
    skillLoading.value = false
  }
}
watch(selectedSkill, () => loadSkill())

// ── subpane lazy load ──────────────────────────────────────────────────────
async function loadSubpane(sub: Subpane) {
  if (sub === 'servers') await reloadServers()
  else if (sub === 'plugins') await reloadPlugins()
  // marketplace:ConnectorMarket 组件自身 onMounted 拉取连接器官方源,无需在此预载
}
onMounted(() => loadSubpane(subpane.value))
watch(subpane, (sub) => loadSubpane(sub))
</script>

<template>
  <div class="mcp">
    <div class="mcp__subnav">
      <SegmentedControl v-model="subpane" :options="subpaneOptions" />
    </div>

    <!-- ════ Servers ════ -->
    <div v-if="subpane === 'servers'" class="mcp__servers">
      <div class="mcp__split">
        <!-- list -->
        <div class="mcp__list">
          <div class="mcp__list-scroll">
          <div v-if="!servers.length" class="mcp__empty">{{ t('codex.mcp.serversEmpty') }}</div>
          <button
            v-for="s in servers"
            :key="s.name"
            type="button"
            class="mcp__list-item"
            :class="{ active: s.name === currentServerName, disabled: s.enabled === false }"
            @click="selectServer(s.name)"
          >
            <span class="mcp__tchip" :class="s.transport === 'stdio' ? 'stdio' : 'http'">{{
              s.transport === 'stdio' ? 'stdio' : 'http'
            }}</span>
            <span class="mcp__list-name">{{ s.name }}</span>
            <span v-if="s.enabled === false" class="mcp__off">off</span>
          </button>
          </div>
          <AppButton class="mcp__new" size="sm" :icon="IconPlus" :label="t('codex.mcp.serverNew')" @click="openNewServer" />
        </div>

        <!-- form -->
        <div class="mcp__form">
          <div v-if="!currentSpec" class="mcp__empty">{{ t('codex.mcp.formEmpty') }}</div>
          <template v-else>
            <div class="mcp__form-head">
              <span class="mcp__form-name">{{ currentSpec.name || '(新)' }}</span>
              <button
                v-if="!isNewServer"
                type="button"
                class="mcp__icon-btn danger"
                :title="t('common.delete')"
                @click="deleteServer"
              >
                <IconTrash />
              </button>
            </div>
            <textarea
              v-if="jsonEditMode"
              v-model="jsonDraft"
              class="mcp__json-edit"
              spellcheck="false"
            ></textarea>
            <pre v-else class="mcp__json-pre">{{ jsonText }}</pre>
            <div v-if="jsonError" class="mcp__json-error">{{ jsonError }}</div>
            <div class="mcp__form-actions">
              <AppButton
                size="sm"
                :variant="jsonEditMode ? 'primary' : 'secondary'"
                :icon="jsonEditMode ? IconCheck : IconPencil"
                :label="jsonEditMode ? (isNewServer ? t('codex.mcp.confirmCreate') : t('common.save')) : t('codex.agentsEdit')"
                @click="editToggle"
              />
              <AppButton size="sm" :icon="IconArchive" :label="t('codex.agentsBackup')" @click="backupServers" />
              <AppButton size="sm" :icon="IconHistory" :label="t('codex.history')" @click="openServersHistory" />
              <AppButton size="sm" :icon="IconFileCode" :label="t('codex.mcp.rawToml')" @click="rawToggle" />
            </div>
          </template>
        </div>
      </div>

      <!-- raw config.toml editor -->
      <div v-if="rawWrap" class="mcp__raw">
        <p class="mcp__warn">{{ t('codex.mcp.rawWarn') }}</p>
        <textarea v-model="rawContent" class="mcp__json-edit" spellcheck="false"></textarea>
        <div class="mcp__form-actions">
          <AppButton variant="ghost" size="sm" :label="t('common.cancel')" @click="rawCancel" />
          <AppButton variant="primary" size="sm" :label="t('codex.apply')" @click="rawApply" />
        </div>
      </div>
    </div>

    <!-- ════ Plugins ════ -->
    <div v-else-if="subpane === 'plugins'" class="mcp__plugins">
      <div v-if="!plugins.length" class="mcp__empty">{{ t('codex.mcp.pluginsEmpty') }}</div>
      <div v-for="p in plugins" :key="p.key" class="mcp__plugin">
        <img
          v-if="!pluginFailedIcons.has(p.key)"
          class="mcp__plugin-icon"
          :src="codexApi.pluginIconUrl(p.key)"
          :alt="p.name"
          @error="onPluginIconError(p.key)"
        />
        <div v-else class="mcp__plugin-icon mcp__plugin-icon--ph" />
        <div class="mcp__plugin-body">
          <span class="mcp__plugin-name">{{ p.name }}</span>
          <span class="mcp__plugin-ver">@{{ p.marketplace }} · v{{ p.version }}</span>
        </div>
        <div class="mcp__plugin-actions">
          <AppButton
            v-if="p.skillNames?.length"
            size="sm"
            :icon="IconSparkles"
            :label="`Skills · ${p.skillNames.length}`"
            @click="openSkills(p)"
          />
          <span class="mcp__plugin-state" :class="p.enabled ? 'on' : 'off'">
            {{ p.enabled ? t('codex.mcp.pluginOn') : t('codex.mcp.pluginOff') }}
          </span>
          <AppButton
            size="sm"
            :label="p.enabled ? t('codex.mcp.pluginDisable') : t('codex.mcp.pluginEnable')"
            @click="togglePlugin(p)"
          />
          <AppButton size="sm" variant="danger" :icon="IconTrash" @click="uninstallPlugin(p)" />
        </div>
      </div>
    </div>

    <!-- ════ Marketplace(连接器官方源镜像)════ -->
    <ConnectorMarket v-else />

    <!-- history (servers) -->
    <HistoryModal
      v-if="showHistory"
      :entries="historyEntries"
      current-content=""
      label-prefix="config.toml"
      @close="showHistory = false"
      @restore="onHistoryRestore"
    />

    <!-- new server name modal -->
    <AppModal v-if="newServerModal" :title="t('codex.mcp.serverNew')" @close="newServerModal = false">
      <p class="mcp__add-desc">{{ t('codex.mcp.formName') }}</p>
      <AppInput v-model="newServerName" placeholder="vercel" />
      <div class="mcp__add-actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="newServerModal = false" />
        <AppButton variant="primary" :label="t('codex.mcp.sourceAddConfirm')" @click="confirmNewServer" />
      </div>
    </AppModal>

    <!-- plugin skills 弹窗 -->
    <AppModal
      v-if="skillsModal"
      :title="`${skillsModal.name} · Skills`"
      @close="skillsModal = null"
    >
      <AppSelect
        v-if="skillNames.length > 1"
        v-model="selectedSkill"
        :options="skillOptions"
        class="mcp__skill-select"
      />
      <div v-if="skillLoading" class="mcp__skill-loading">{{ t('market.loading') }}</div>
      <div v-else-if="skillDoc" class="mcp__skill-doc">
        <div class="mcp__skill-name">{{ skillDoc.name }}</div>
        <div v-if="skillDoc.description" class="mcp__skill-desc">{{ skillDoc.description }}</div>
        <pre class="mcp__skill-content">{{ skillDoc.content }}</pre>
      </div>
    </AppModal>

  </div>
</template>

<style scoped>
.mcp {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.mcp__subnav {
  display: flex;
  justify-content: center;
}
.mcp__warn {
  margin: 0;
  padding: var(--space-2) var(--space-3);
  background: var(--warning-soft);
  border-radius: var(--radius);
  color: var(--text-secondary);
  font-size: var(--fs-xs);
  line-height: 1.5;
}
.mcp__empty {
  padding: var(--space-5) var(--space-3);
  text-align: center;
  color: var(--text-muted);
  font-size: var(--fs-sm);
}

/* servers split */
.mcp__split {
  display: grid;
  grid-template-columns: 200px 1fr;
  gap: var(--space-3);
  height: calc(100vh - 280px);
  min-height: 320px;
}
.mcp__list {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-height: 0;
}
/* 列表填满左列、框内滚;「新增」按钮常驻底部,避免下方留空白 */
.mcp__list-scroll {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
.mcp__list-item {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2);
  border: 1px solid transparent;
  border-radius: var(--radius);
  background: var(--surface);
  text-align: left;
}
.mcp__list-item:hover {
  background: var(--surface-hover);
}
.mcp__list-item.active {
  border-color: var(--accent);
  background: var(--accent-soft);
}
.mcp__list-item.disabled {
  opacity: 0.55;
}
.mcp__list-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--fs-sm);
}
.mcp__new {
  margin-top: var(--space-1);
}
.mcp__tchip {
  flex-shrink: 0;
  padding: 1px 6px;
  border-radius: var(--radius-sm);
  font-size: 10px;
  font-weight: 600;
  text-transform: uppercase;
}
.mcp__tchip.stdio {
  background: var(--success-soft);
  color: var(--success);
}
.mcp__tchip.http {
  background: var(--accent-soft);
  color: var(--accent-text);
}
.mcp__off {
  font-size: 10px;
  color: var(--text-muted);
}

/* form */
.mcp__form {
  min-width: 0;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.mcp__form-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.mcp__form-name {
  font-weight: 600;
  font-size: var(--fs-md);
}
.mcp__icon-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  border: none;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-secondary);
}
.mcp__icon-btn.danger:hover {
  background: var(--danger-soft);
  color: var(--danger);
}
.mcp__icon-btn svg {
  width: 15px;
  height: 15px;
}
/* 配置框固定高度(填满表单中段)+ 框内滚:不同 server 配置长短不再改变布局/撑页 */
.mcp__json-pre,
.mcp__json-edit {
  margin: 0;
  flex: 1;
  min-height: 0;
  padding: var(--space-3);
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  line-height: 1.6;
  overflow-y: auto;
  white-space: pre-wrap;
  word-break: break-all;
}
.mcp__json-edit {
  width: 100%;
  background: var(--surface);
  border-color: var(--border-strong);
  color: var(--text);
  resize: none;
}
.mcp__json-edit:focus {
  outline: none;
  border-color: var(--accent);
}
.mcp__json-error {
  padding: var(--space-2) var(--space-3);
  background: var(--danger-soft);
  border-radius: var(--radius-sm);
  color: var(--danger);
  font-size: var(--fs-xs);
  white-space: pre-wrap;
}
.mcp__form-actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.mcp__raw {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

/* plugins */
.mcp__plugins {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.mcp__plugin {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-3);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.mcp__plugin-icon {
  flex-shrink: 0;
  width: 32px;
  height: 32px;
  border-radius: var(--radius-sm);
  object-fit: cover;
  background: var(--surface-2);
}
.mcp__plugin-icon--ph {
  background: var(--surface-2);
}
.mcp__plugin-body {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.mcp__plugin-name {
  font-weight: 550;
  font-size: var(--fs-sm);
}
.mcp__plugin-ver {
  font-size: var(--fs-xs);
  color: var(--text-muted);
}
.mcp__plugin-actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-shrink: 0;
}
.mcp__plugin-state {
  font-size: var(--fs-sm);
  font-weight: 600;
}
.mcp__plugin-state.on {
  color: var(--success);
}
.mcp__plugin-state.off {
  color: var(--text-muted);
}

/* modals */
.mcp__add-desc {
  margin: 0 0 var(--space-3);
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  line-height: 1.5;
}
.mcp__add-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
.mcp__skill-select {
  margin-bottom: var(--space-3);
}
.mcp__skill-loading {
  padding: var(--space-4);
  text-align: center;
  color: var(--text-muted);
}
.mcp__skill-name {
  font-size: var(--fs-md);
  font-weight: 600;
}
.mcp__skill-desc {
  margin-top: 2px;
  font-size: var(--fs-sm);
  color: var(--text-muted);
}
.mcp__skill-content {
  margin-top: var(--space-3);
  max-height: 50vh;
  overflow: auto;
  padding: var(--space-3);
  background: var(--surface-2);
  border-radius: var(--radius-sm);
  font-family: var(--font-mono);
  font-size: var(--fs-xs);
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-word;
}
</style>
