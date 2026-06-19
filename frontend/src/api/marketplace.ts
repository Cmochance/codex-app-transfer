import { api } from './http'

// 连接器市场(多源,phase2)。官方源=私有 storage 仓库镜像(后端持 token);自加源=用户加的
// 公开 registry.json URL(同 schema)。后端聚合 + 图标代理(官方走 token,自加走其 base)。

export interface Connector {
  id: string
  name: string
  display_name?: string | null
  category?: string
  category_id?: string | null
  short_description?: string | null
  long_description?: string | null
  developer_name?: string | null
  brand_color?: string | null
  website_url?: string | null
  logo_url?: string | null
  composer_icon_url?: string | null
  default_prompts?: string[]
  status?: string
  version?: string
  source?: string // 聚合时后端注入的源 id
}

export interface ConnectorSourceMeta {
  id: string
  name: string
  official: boolean
  enabled: boolean
  count: number
}

export interface ConnectorRegistry {
  sources: ConnectorSourceMeta[]
  connectors: Connector[]
  categories?: string[]
  errors?: Record<string, string>
}

// GET /api/marketplace/connectors — 聚合所有启用源(每源 body 缓存 30min,force 跳缓存)。
export const getConnectors = (forceRefresh = false) =>
  api<ConnectorRegistry>(
    'GET',
    `/api/marketplace/connectors${forceRefresh ? '?force_refresh=true' : ''}`,
  )

export const addConnectorSource = (name: string, url: string) =>
  api('POST', '/api/marketplace/sources/add', { name, url })
export const removeConnectorSource = (id: string) =>
  api('POST', '/api/marketplace/sources/remove', { id })
export const toggleConnectorSource = (id: string, enabled: boolean) =>
  api('POST', '/api/marketplace/sources/toggle', { id, enabled })

// 图标统一走后端代理(同源,绕 CSP img-src 'self');带 source 让后端按源解析(官方 token / 自加 base)。
export function iconSrc(path?: string | null, source?: string | null): string {
  if (!path) return ''
  const params = new URLSearchParams({ path })
  if (source) params.set('source', source)
  return `/api/marketplace/icon?${params.toString()}`
}
