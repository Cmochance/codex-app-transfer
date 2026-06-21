import { api } from './http'
import type { Provider, Preset, ProviderPayload } from './types'

type IconSpec = { logo?: string; icon?: string }

// ── 逐字移植 api.js ICON_MAP ──
// 子串匹配 + insertion 顺序敏感: 特定 OAuth/品牌规则必须放在通用规则前(见各行注释)。
const ICON_MAP: Record<string, IconSpec> = {
  deepseek: { logo: 'assets/providers/deepseek.ico' },
  kimi: { logo: 'assets/providers/kimi.ico' },
  moonshot: { logo: 'assets/providers/kimi.ico' },
  xiaomi: { logo: 'assets/providers/xiaomi-mimo.png' },
  mimo: { logo: 'assets/providers/xiaomi-mimo.png' },
  qiniu: { logo: 'assets/providers/qiniu.ico' },
  qnaigc: { logo: 'assets/providers/qiniu.ico' },
  zhipu: { logo: 'assets/providers/zhipu.png' },
  bigmodel: { logo: 'assets/providers/zhipu.png' },
  glm: { logo: 'assets/providers/zhipu.png' },
  // [MOC-252] zai-login 的 id/baseUrl 不含 zhipu/bigmodel/glm, 显式兜底复用智谱 logo
  'zai-login': { logo: 'assets/providers/zhipu.png' },
  siliconflow: { icon: 'bi-diagram-3-fill' },
  bailian: { logo: 'assets/providers/aliyun.ico' },
  dashscope: { logo: 'assets/providers/aliyun.ico' },
  aliyun: { logo: 'assets/providers/aliyun.ico' },
  minimax: { logo: 'assets/providers/minimax.ico' },
  minimaxi: { logo: 'assets/providers/minimax.ico' },
  // gemini-cli 必须在通用 'gemini' 前(insertion 顺序), 才能优先命中专属 spark 图标
  'gemini-cli': { logo: 'assets/providers/gemini.svg' },
  'antigravity-oauth': { logo: 'assets/providers/antigravity.png' },
  google: { logo: 'assets/providers/google-ai-studio.png' },
  gemini: { logo: 'assets/providers/google-ai-studio.png' },
  aistudio: { logo: 'assets/providers/google-ai-studio.png' },
  generativelanguage: { logo: 'assets/providers/google-ai-studio.png' },
  'grok-web': { logo: 'assets/providers/grok.svg' },
  anyrouter: { logo: 'assets/providers/anyrouter.png' },
}

// 逐字移植 computeIcon: 拼 id+name+baseUrl+apiFormat → normalize(_/空格→-)→ 子串匹配 ICON_MAP
export function computeIcon(p: Partial<Provider>): IconSpec {
  const raw = `${p.id || ''} ${p.name || ''} ${p.baseUrl || ''} ${p.apiFormat || ''}`.toLowerCase()
  const lookup = raw.replace(/[_\s]+/g, '-')
  for (const [key, val] of Object.entries(ICON_MAP)) {
    if (lookup.includes(key)) return val
  }
  return { icon: 'bi-plug-fill' }
}

// 逐字移植 mapProvider: 后端 public_provider → 前端 Provider(显式挑字段, 不列即丢)
export function mapProvider(provider: Record<string, any>, activeId: string | null): Provider {
  const models = provider.models || {}
  return {
    id: provider.id,
    name: provider.name,
    baseUrl: provider.baseUrl,
    apiFormat: ['openai', 'openai_chat'].includes(provider.apiFormat)
      ? 'openai_chat'
      : provider.apiFormat || 'openai_chat',
    authScheme: provider.authScheme || 'bearer',
    hasApiKey: !!provider.hasApiKey,
    hasMimoCookie: !!provider.hasMimoCookie,
    extraHeaders: provider.extraHeaders || {},
    modelCapabilities: provider.modelCapabilities || {},
    requestOptions: provider.requestOptions || {},
    default: provider.id === activeId,
    isBuiltin: !!provider.isBuiltin,
    reviewModelSlot: provider.reviewModelSlot || '',
    mappings: {
      default: models.default || '',
      gpt_5_5: models.gpt_5_5 || '',
      gpt_5_4: models.gpt_5_4 || '',
      gpt_5_4_mini: models.gpt_5_4_mini || '',
      gpt_5_3_codex: models.gpt_5_3_codex || '',
      gpt_5_2: models.gpt_5_2 || '',
    },
    ...computeIcon(provider),
  }
}

// 逐字移植 providerBody: apiFormat passthrough 已知协议(含 OAuth/grok 别名), 让后端 normalize 唯一负责。
// **不可简化**: 漏 passthrough 某协议会 fallback openai_chat → 走错端点 404(各 if 注释是实战踩坑)。
export function providerBody(payload: ProviderPayload, includeModels = true): Record<string, unknown> {
  const body: Record<string, unknown> = {
    name: payload.name,
    baseUrl: payload.baseUrl,
    authScheme: payload.authScheme || 'bearer',
    apiFormat: (() => {
      const v = (payload.apiFormat || '').toLowerCase().replace(/-/g, '_')
      if (['responses', 'openai_responses'].includes(v)) return 'responses'
      if (['anthropic_messages', 'anthropic', 'claude', 'messages', 'claude_messages'].includes(v))
        return 'anthropic_messages'
      if (['gemini_native', 'google_ai_studio', 'gemini'].includes(v)) return 'gemini_native'
      if (['gemini_cli_oauth', 'gemini_oauth', 'google_oauth_cloud_code'].includes(v))
        return 'gemini_cli_oauth'
      if (['antigravity_oauth', 'google_oauth_antigravity'].includes(v)) return 'antigravity_oauth'
      if (['grok_web', 'grok', 'grok_com'].includes(v)) return 'grok_web'
      return 'openai_chat'
    })(),
    extraHeaders: payload.extraHeaders || {},
    modelCapabilities: payload.modelCapabilities || {},
    requestOptions: payload.requestOptions || {},
  }
  if (payload.apiKey) body.apiKey = payload.apiKey
  if (includeModels) body.models = payload.models || {}
  if (payload.reviewModelSlot !== undefined && payload.reviewModelSlot !== null)
    body.reviewModelSlot = payload.reviewModelSlot
  if (payload.grokWeb) body.grokWeb = payload.grokWeb
  return body
}

// ── 端点(路径/请求 shape 保持与后端契约一致) ──
export async function getProviders(): Promise<Provider[]> {
  // 后端 list_providers 返回的 key 是 `activeId`(非 activeProviderId);读错即导致
  // 没有 provider 被标记 default → 「已启用」徽章/置顶不生效。
  const data = await api<{ providers?: Record<string, any>[]; activeId?: string | null }>(
    'GET',
    '/api/providers',
  )
  const activeId = data.activeId ?? null
  return (data.providers || []).map((p) => mapProvider(p, activeId))
}

export async function getPresets(): Promise<Preset[]> {
  const data = await api<{ presets?: Record<string, any>[] }>('GET', '/api/presets')
  return (data.presets || []).map((p) => ({ ...p, ...computeIcon(p) }) as Preset)
}

export const addProvider = (payload: ProviderPayload) =>
  api('POST', '/api/providers', providerBody(payload))
export const updateProvider = (id: string, payload: ProviderPayload) =>
  api('PUT', `/api/providers/${id}`, providerBody(payload))
export const deleteProvider = (id: string) => api('DELETE', `/api/providers/${id}`)
export const reorderProviders = (providerIds: string[]) =>
  api('PUT', '/api/providers/reorder', { providerIds })
export const setDefaultProvider = (id: string) => api('PUT', `/api/providers/${id}/default`)
export const activateProvider = (id: string) => api('POST', `/api/providers/${id}/activate`)
export const getProviderSecret = (id: string) =>
  api<{ apiKey?: string }>('GET', `/api/providers/${id}/secret`)

// 小米账号登录(内嵌窗口抓套餐 session cookie,供 Codex 显示 MiMo 套餐额度)。
// 长阻塞:窗口登录完成/关闭才返回;captured=true 表示已抓到 session。
export const mimoLogin = (id: string) =>
  api<{ captured?: boolean }>('POST', `/api/providers/${id}/mimo-login`)

// 获取上游可用模型:已存在 provider 走 id(用落盘 key);草稿(新增/编辑未存)走 payload。
// 响应除 models 列表外还带 suggested(后端 suggest_model_mappings 自动建议的「槽位→模型 id」映射,
// 目前主要给 default 槽),供前端 fetchModels 一键预填空槽位(取代已删的 autofill 专用端点)。
type ModelsAvailableResp = {
  models?: unknown[]
  suggested?: Record<string, string>
  // [CAT-256] 后端为 openai_chat provider 剔除掉走 messages(anthropic)端点的模型 id(如 OpenCode Go 的 minimax/qwen 系)
  filteredMessagesModels?: string[]
}
export const fetchProviderModels = (id: string) =>
  api<ModelsAvailableResp>('GET', `/api/providers/${id}/models/available`)
export const fetchProviderModelsDraft = (payload: ProviderPayload) =>
  api<ModelsAvailableResp>('POST', '/api/providers/models/available', providerBody(payload, false))

// 测试表单当前值的连接(后端用传入 payload 发探测请求,不依赖落盘配置)。
// 新增/编辑均可测;返回 { ok, latencyMs, message };ok=true = endpoint 可达(含 401/403),
// ok=false = endpoint 不存在或网络不通;message 带诊断详情(延迟/状态码)。
// **必须带 models**: 只支持 POST 探测的上游(Kimi/GLM 等 HEAD/GET→404)会发 chat
// 探测体,后端 provider_test_model 按 default 槽选模型;剥掉 models 会回落硬编码
// claude-sonnet-4-6,对该模型返 404 的上游把可用 provider 误报为「endpoint unavailable」。
export const testProvider = (payload: ProviderPayload) =>
  api<{ ok?: boolean; latencyMs?: number; message?: string }>(
    'POST',
    '/api/providers/test',
    providerBody(payload),
  )
