import { api } from './http'

// 连接器市场(展示镜像,phase2)。源:私有仓库 codex-app-transfer-storage(镜像自 OpenAI Codex
// 插件目录展示数据)。后端 /api/marketplace/* 持 token 代拉 + 缓存,前端不直连私有仓库。
// 这些连接器是 OpenAI 平台 OAuth 远程连接器、由单一 plugin-runtime broker、无独立 MCP 端点,
// 故仅展示浏览(不含 install)。

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
}

export interface ConnectorRegistry {
  version?: number
  source?: string
  captured_at?: string
  count?: number
  categories?: string[]
  connectors: Connector[]
}

// GET /api/marketplace/connectors — 私有 storage 仓库的 registry.json(内存缓存 30min)。
export const getConnectors = () => api<ConnectorRegistry>('GET', '/api/marketplace/connectors')

// 图标地址:本地路径(icons/*.png)经后端代理(同源,CSP img-src 'self' 放行);少量镜像失败的
// 兜底是 OpenAI CDN 绝对 URL(CSP 会拦,由卡片 @error 回退字母占位)。
export function iconSrc(path?: string | null): string {
  if (!path) return ''
  if (path.startsWith('http')) return path
  return `/api/marketplace/icon?path=${encodeURIComponent(path)}`
}
