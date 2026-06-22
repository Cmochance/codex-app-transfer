// OAuth 账号登录(浏览器授权)provider 的 typed API 层。
// 后端各套独立路由,形态一致(status/login/login·cancel/logout):
//   - zai-login / bigmodel-login → /api/zai-oauth/*       (需 ?provider=zai|bigmodel)
//   - gemini-cli                → /api/gemini-oauth/*
//   - antigravity               → /api/antigravity-oauth/*
//   - trae                      → /api/trae-oauth/*        (需 ?providerId=<id>,多账号按 provider 条目隔离)
// login 为长阻塞:POST 后端开系统浏览器授权,直到回调完成/取消才返回。
import { api } from './http'

export type OAuthKind = 'zai' | 'bigmodel' | 'gemini' | 'antigravity' | 'trae'

export interface OAuthStatus {
  loggedIn: boolean
  email?: string
}

// trae 按 provider 条目 keying(同设备多账号指纹隔离),需传 providerId;其余 kind 忽略 providerId。
function endpoint(kind: OAuthKind, providerId?: string): { base: string; query: string } {
  switch (kind) {
    case 'zai':
      return { base: '/api/zai-oauth', query: '?provider=zai' }
    case 'bigmodel':
      return { base: '/api/zai-oauth', query: '?provider=bigmodel' }
    case 'gemini':
      return { base: '/api/gemini-oauth', query: '' }
    case 'antigravity':
      return { base: '/api/antigravity-oauth', query: '' }
    case 'trae':
      return {
        base: '/api/trae-oauth',
        query: `?providerId=${encodeURIComponent(providerId ?? '')}`,
      }
  }
}

export function oauthStatus(kind: OAuthKind, providerId?: string) {
  const { base, query } = endpoint(kind, providerId)
  return api<OAuthStatus>('GET', `${base}/status${query}`)
}
// 长阻塞:解析成功 = 授权完成;被 cancel 时后端返回错误,调用方按取消处理。
export function oauthLogin(kind: OAuthKind, providerId?: string) {
  const { base, query } = endpoint(kind, providerId)
  return api(`POST`, `${base}/login${query}`)
}
// cancel 是进程级(不分账号),不带 providerId。
export function oauthCancelLogin(kind: OAuthKind) {
  const { base } = endpoint(kind)
  return api('DELETE', `${base}/login/cancel`)
}
export function oauthLogout(kind: OAuthKind, providerId?: string) {
  const { base, query } = endpoint(kind, providerId)
  return api('DELETE', `${base}/logout${query}`)
}
