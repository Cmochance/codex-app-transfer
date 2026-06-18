// Provider 等数据类型(从旧 api.js mapper 返回结构反推)

export interface ProviderMappings {
  default: string
  gpt_5_5: string
  gpt_5_4: string
  gpt_5_4_mini: string
  gpt_5_3_codex: string
  gpt_5_2: string
}

export interface Provider {
  id: string
  name: string
  baseUrl: string
  apiFormat: string
  authScheme: string
  hasApiKey: boolean
  hasMimoCookie: boolean
  extraHeaders: Record<string, string>
  modelCapabilities: Record<string, unknown>
  requestOptions: Record<string, unknown>
  default: boolean
  isBuiltin: boolean
  reviewModelSlot: string
  mappings: ProviderMappings
  logo?: string
  icon?: string
}

export interface Preset {
  id: string
  name: string
  baseUrl: string
  apiFormat: string
  authScheme?: string
  baseUrlOptions?: string[]
  baseUrlHint?: string
  allowApiFormatSelection?: boolean
  logo?: string
  icon?: string
}

// providerBody 的输入(编辑表单收集的 payload), 字段宽松
export interface ProviderPayload {
  name: string
  baseUrl: string
  authScheme?: string
  apiFormat?: string
  apiKey?: string
  models?: Record<string, string>
  extraHeaders?: Record<string, string>
  modelCapabilities?: Record<string, unknown>
  requestOptions?: Record<string, unknown>
  reviewModelSlot?: string | null
  grokWeb?: unknown
}
