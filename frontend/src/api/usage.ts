import { api } from './http'

// GET /api/usage/summary 返回的报表 shape(移植旧 app.js renderUsage* 用到的字段)。
// 后端 codex-app-transfer-usage-tracker 扫 ~/.codex/sessions/ rollout JSONL,
// 解析层 vendor 自 ryoppippi/ccusage(MIT)。
export interface UsageRow {
  group: string
  displayName?: string
  upstreamModel?: string
  models?: string[]
  inputTokens?: number
  cachedInputTokens?: number
  outputTokens?: number
  reasoningOutputTokens?: number
  totalTokens?: number
  turnCount?: number
  lastActivity?: string
}

export interface UsageReport {
  totalInputTokens?: number
  totalOutputTokens?: number
  totalTokens?: number
  totalConversations?: number
  daily?: UsageRow[]
  byModel?: UsageRow[]
  byConversation?: UsageRow[]
  unknownTimestampEvents?: number
}

// 单轮缓存命中分布的一个桶(≤10 桶,后端已分好)。
export interface CacheBucket {
  inputTokens?: number
  cachedInputTokens?: number
  outputTokens?: number
  turnEnd?: number
}

export type UsageView = 'conversation' | 'daily' | 'model'

// 浏览器 tz → 后端按用户时区聚合;forceRefresh 带 nocache=1 绕过后端 60s TTL cache。
export async function getUsageSummary(forceRefresh = false): Promise<UsageReport> {
  const tz = encodeURIComponent(Intl.DateTimeFormat().resolvedOptions().timeZone || '')
  const nocache = forceRefresh ? '&nocache=1' : ''
  return api<UsageReport>('GET', `/api/usage/summary?tz=${tz}${nocache}`)
}

export async function getCacheSeries(session: string): Promise<CacheBucket[]> {
  return api<CacheBucket[]>(
    'GET',
    `/api/usage/conversation/cache-series?session=${encodeURIComponent(session)}`,
  )
}
