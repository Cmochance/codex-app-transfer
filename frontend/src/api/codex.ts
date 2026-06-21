// Codex 资产半区 typed API 层 — 收编旧 app.js 散落 fetch(逐字保留路径/body/响应字段)。
// 后端 err() 返回 {success:false, error, message}(同值),故统一走 api() wrapper(读 message)。
import { api } from './http'

// ───────────────────────────────────────────────────────────────────────────
// Managed markdown(agents / memories / skills 三套对称,apiBase 参数化)
// ───────────────────────────────────────────────────────────────────────────
export type ManagedResource = 'agents' | 'memories' | 'skills'

export const MANAGED_API_BASE: Record<ManagedResource, string> = {
  agents: '/api/codex/agents-md',
  memories: '/api/codex/memories-md',
  skills: '/api/codex/skills-md',
}

// /paths 的 entries[]:agents 用 category/projectName/subdirPath;skills 用 name。
export interface ManagedPathEntry {
  hash: string
  path: string
  category?: string
  projectName?: string
  subdirPath?: string
  name?: string
}

// /history 的 history[]:index + unix 秒 timestamp + 内容快照。
export interface ManagedHistoryEntry {
  index: number
  timestamp: number
  appliedContent?: string
  managedContent?: string
}

function hashQ(hash?: string | null): string {
  return hash ? `?hash=${encodeURIComponent(hash)}` : ''
}

export function getManagedPaths(resource: ManagedResource) {
  return api<{ entries?: ManagedPathEntry[] }>('GET', `${MANAGED_API_BASE[resource]}/paths`)
}
export function addManagedPath(resource: ManagedResource, path: string) {
  return api<{ entry?: ManagedPathEntry }>('POST', `${MANAGED_API_BASE[resource]}/paths/add`, { path })
}
export function removeManagedPath(resource: ManagedResource, hash: string) {
  return api('POST', `${MANAGED_API_BASE[resource]}/paths/remove`, { hash })
}
export function getManagedRaw(resource: ManagedResource, hash?: string | null) {
  return api<{ content?: string }>('GET', `${MANAGED_API_BASE[resource]}/raw${hashQ(hash)}`)
}
export function saveManagedRaw(resource: ManagedResource, hash: string | null | undefined, content: string) {
  return api('POST', `${MANAGED_API_BASE[resource]}/raw${hashQ(hash)}`, { content })
}
export function backupManaged(resource: ManagedResource, hash?: string | null) {
  return api('POST', `${MANAGED_API_BASE[resource]}/backup${hashQ(hash)}`)
}
export function getManagedHistory(resource: ManagedResource, hash?: string | null) {
  return api<{ history?: ManagedHistoryEntry[] }>('GET', `${MANAGED_API_BASE[resource]}/history${hashQ(hash)}`)
}
export function restoreManagedRaw(resource: ManagedResource, hash: string | null | undefined, index: number) {
  return api('POST', `${MANAGED_API_BASE[resource]}/restore-raw${hashQ(hash)}`, { index })
}
// skills 独有:在文件管理器打开 SKILL.md 所在目录。
export function revealManaged(resource: ManagedResource, hash?: string | null) {
  return api('POST', `${MANAGED_API_BASE[resource]}/reveal${hashQ(hash)}`)
}

// ───────────────────────────────────────────────────────────────────────────
// MCP — servers / plugins
// ───────────────────────────────────────────────────────────────────────────
export interface McpServerSpec {
  name: string
  transport?: 'stdio' | 'streamable_http'
  command?: string
  args?: string[]
  env?: Record<string, string>
  cwd?: string
  url?: string
  bearerTokenEnvVar?: string
  httpHeaders?: Record<string, string>
  envHttpHeaders?: Record<string, string>
  enabled?: boolean
  required?: boolean
  supportsParallelToolCalls?: boolean
  experimentalEnvironment?: boolean
  startupTimeoutSec?: number
  toolTimeoutSec?: number
  defaultToolsApprovalMode?: string
  enabledTools?: string[]
  disabledTools?: string[]
  disabledReason?: string
  [k: string]: unknown
}

export interface McpPlugin {
  key: string
  name: string
  marketplace?: string
  version?: string
  enabled?: boolean
  skillNames?: string[]
  installDir?: string
}

export interface PluginSkill {
  name: string
  description: string
  content: string
}
// GET /api/codex/mcp/plugins/skill — 某 plugin 某 skill 的 SKILL.md(name/description/正文)。
export const getPluginSkill = (key: string, name: string) =>
  api<{ skill?: PluginSkill }>(
    'GET',
    `/api/codex/mcp/plugins/skill?key=${encodeURIComponent(key)}&name=${encodeURIComponent(name)}`,
  )
// 已安装 plugin 的图标(assets/app-icon.png),直接作为 <img src>。
export const pluginIconUrl = (key: string) =>
  `/api/codex/mcp/plugins/icon?key=${encodeURIComponent(key)}`

// servers
export const getMcpServers = () =>
  api<{ servers?: McpServerSpec[] }>('GET', '/api/codex/mcp/servers')
export const saveMcpServer = (spec: McpServerSpec) =>
  api('POST', '/api/codex/mcp/servers', spec)
export const deleteMcpServer = (name: string) =>
  api('POST', '/api/codex/mcp/servers/delete', { name })
export const backupMcpServers = () => api('POST', '/api/codex/mcp/servers/backup')
export const getMcpServersHistory = () =>
  api<{ history?: ManagedHistoryEntry[] }>('GET', '/api/codex/mcp/servers/history')
export const restoreMcpServers = (index: number) =>
  api('POST', '/api/codex/mcp/servers/restore', { index })
export const getMcpConfigRaw = () =>
  api<{ content?: string }>('GET', '/api/codex/mcp/config/raw')
export const saveMcpConfigRaw = (content: string) =>
  api('POST', '/api/codex/mcp/config/raw', { content })

// plugins
export const getMcpPlugins = () =>
  api<{ plugins?: McpPlugin[] }>('GET', '/api/codex/mcp/plugins')
export const toggleMcpPlugin = (key: string, enabled: boolean) =>
  api('POST', '/api/codex/mcp/plugins/toggle', { key, enabled })
export const uninstallMcpPlugin = (key: string) =>
  api('POST', '/api/codex/mcp/plugins/uninstall', { key })
export const installMcpPlugin = (body: {
  name: string
  marketplace?: string
  version?: string
  tarballUrl?: string
}) => api('POST', '/api/codex/mcp/plugins/install', body)

// ───────────────────────────────────────────────────────────────────────────
// Conversations
// ───────────────────────────────────────────────────────────────────────────
export interface ConversationMeta {
  id: string
  title?: string
  kind?: string
  createdAt?: string
  cwd?: string
  turnCount?: number
  modelProvider?: string
  originator?: string
  path?: string
}

export interface ConversationItem {
  type?: string
  role?: string
  text?: string
  name?: string
  arguments?: unknown
  output?: string
  summary?: string
}
export interface ConversationDetail {
  meta?: { id?: string; title?: string; cwd?: string; originator?: string; modelProvider?: string }
  turns?: { items?: ConversationItem[] }[]
}

export interface ExportOptions {
  includeReasoning: boolean
  includeToolCalls: boolean
  toolOutputMaxChars: number
  includeSystemPrompts: boolean
  redactSecrets: boolean
}

export const getConversations = () =>
  api<{ sessions?: ConversationMeta[] }>('GET', '/api/conversations/list')
// 后端 detail 返回**裸** NormalizedSession({meta,turns,warnings} 顶层,无 session 包裹)
export const getConversation = (id: string) =>
  api<ConversationDetail>('GET', `/api/conversations/${encodeURIComponent(id)}`)

// 清空会话历史(两者都清):全部 rollout 移回收站(可恢复)+ 清 proxy L2 续轮缓存。
export interface ClearAllConversationsResult {
  success: boolean
  sessionsTrashed: number
  sessionsFailed: number
  failed?: { sessionId: string; reason: string }[]
  cacheRowsRemoved: number
}
// 不走 api():后端在「部分/全部 rollout 移回收站失败」时返 HTTP 200 + {success:false, sessionsFailed>0},
// api() 遇 success===false 即抛、会吞掉 trashed/failed 计数,UI 无法逐条提示。用 raw fetch 读完整
// payload,只在真正的传输/网关错误(非 2xx 且带 message,或非 JSON)时抛。
export async function clearAllConversations(): Promise<ClearAllConversationsResult> {
  const resp = await fetch('/api/conversations/clear-all', {
    method: 'POST',
    headers: { 'X-CAS-Request': '1', 'Content-Type': 'application/json' },
  })
  let data: ClearAllConversationsResult & { message?: string }
  try {
    data = await resp.json()
  } catch (parseErr) {
    throw new Error(
      `Request failed: POST /api/conversations/clear-all — HTTP ${resp.status} ${resp.statusText || ''} ` +
        `(非 JSON 响应: ${String(parseErr)})`,
    )
  }
  if (!resp.ok) throw new Error(data.message || `Request failed: HTTP ${resp.status}`)
  return data
}
// 后端「全部失败」时返 HTTP 200 + {success:false, deleted:[], failed:[...]}(conversations.rs)。
// 不能走 api():其遇 success===false 即抛,会吞掉 failed 明细,UI 退回泛化报错而非逐条提示。
// 用 raw fetch 读完整 payload,只在真正的传输/网关错误(非 2xx / 非 JSON)时抛。
export async function deleteConversations(
  sessionIds: string[],
): Promise<{ deleted?: string[]; failed?: { sessionId: string; reason: string }[] }> {
  const resp = await fetch('/api/conversations/delete', {
    method: 'POST',
    headers: { 'X-CAS-Request': '1', 'Content-Type': 'application/json' },
    body: JSON.stringify({ sessionIds }),
  })
  let data: {
    deleted?: string[]
    failed?: { sessionId: string; reason: string }[]
    message?: string
  }
  try {
    data = await resp.json()
  } catch (parseErr) {
    throw new Error(
      `Request failed: POST /api/conversations/delete — HTTP ${resp.status} ${resp.statusText || ''} ` +
        `(非 JSON 响应: ${String(parseErr)})`,
    )
  }
  if (!resp.ok) throw new Error(data.message || `Request failed: HTTP ${resp.status}`)
  return data
}

// 导出双响应:targetPath 已落盘 → JSON {success}; 无 targetPath → 二进制流(浏览器下载)。
// 用 raw fetch(非 api())以便按 Content-Type 分流读 blob。
export async function exportConversations(body: {
  sessionIds: string[]
  format: string
  options: ExportOptions
  targetPath?: string
}): Promise<{ kind: 'json'; data: unknown } | { kind: 'blob'; blob: Blob; filename: string }> {
  const resp = await fetch('/api/conversations/export', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-CAS-Request': '1' },
    body: JSON.stringify(body),
  })
  const ct = resp.headers.get('Content-Type') || ''
  if (ct.includes('application/json')) {
    const data = await resp.json()
    if (!resp.ok || (data as { success?: boolean }).success === false) {
      throw new Error((data as { message?: string }).message || '导出失败')
    }
    return { kind: 'json', data }
  }
  if (!resp.ok) throw new Error(`导出失败: HTTP ${resp.status}`)
  const blob = await resp.blob()
  // 从 Content-Disposition 抽文件名,回退默认。
  const cd = resp.headers.get('Content-Disposition') || ''
  const m = cd.match(/filename\*?=(?:UTF-8'')?"?([^";]+)"?/i)
  const filename = m ? decodeURIComponent(m[1]) : `conversations.${body.format}`
  return { kind: 'blob', blob, filename }
}
