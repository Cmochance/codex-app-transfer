// OAuth 账号登录(浏览器授权)provider 的 typed API 层。
// 后端三套独立路由,形态一致(status/login/login·cancel/logout):
//   - zai-login / bigmodel-login → /api/zai-oauth/*       (需 ?provider=zai|bigmodel)
//   - gemini-cli                → /api/gemini-oauth/*
//   - antigravity               → /api/antigravity-oauth/*
// login 为长阻塞:POST 后端开系统浏览器授权,直到回调完成/取消才返回。
import { api } from './http'

export type OAuthKind = 'zai' | 'bigmodel' | 'gemini' | 'antigravity'

export interface OAuthStatus {
  loggedIn: boolean
  email?: string
}

// 各 kind 的端点前缀 + query(zai/bigmodel 共用 zai-oauth, 靠 ?provider 区分)
function endpoint(kind: OAuthKind): { base: string; query: string } {
  switch (kind) {
    case 'zai':
      return { base: '/api/zai-oauth', query: '?provider=zai' }
    case 'bigmodel':
      return { base: '/api/zai-oauth', query: '?provider=bigmodel' }
    case 'gemini':
      return { base: '/api/gemini-oauth', query: '' }
    case 'antigravity':
      return { base: '/api/antigravity-oauth', query: '' }
  }
}

export function oauthStatus(kind: OAuthKind) {
  const { base, query } = endpoint(kind)
  return api<OAuthStatus>('GET', `${base}/status${query}`)
}
// 长阻塞:解析成功 = 授权完成;被 cancel 时后端返回错误,调用方按取消处理。
export function oauthLogin(kind: OAuthKind) {
  const { base, query } = endpoint(kind)
  return api(`POST`, `${base}/login${query}`)
}
export function oauthCancelLogin(kind: OAuthKind) {
  const { base, query } = endpoint(kind)
  return api('DELETE', `${base}/login/cancel${query}`)
}
export function oauthLogout(kind: OAuthKind) {
  const { base, query } = endpoint(kind)
  return api('DELETE', `${base}/logout${query}`)
}
