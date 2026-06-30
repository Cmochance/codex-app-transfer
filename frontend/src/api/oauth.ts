// OAuth 账号登录(浏览器授权)provider 的 typed API 层。
// 后端各套独立路由,形态一致(status/login/login·cancel/logout):
//   - zai-login / bigmodel-login → /api/zai-oauth/*       (需 ?provider=zai|bigmodel)
//   - gemini-cli                → /api/gemini-oauth/*
//   - antigravity               → /api/antigravity-oauth/*
//   - trae                      → /api/trae-oauth/*        (需 ?providerId=<id>,多账号按 provider 条目隔离)
//   - workbuddy                 → /api/workbuddy-oauth/*   (需 ?providerId=<id>,单 provider 内**账号池**)
// login 为长阻塞:POST 后端开系统浏览器授权,直到回调完成/取消才返回。
import { api } from './http'

export type OAuthKind = 'zai' | 'bigmodel' | 'gemini' | 'antigravity' | 'trae' | 'workbuddy'

export interface OAuthStatus {
  loggedIn: boolean
  email?: string
  nickname?: string
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
    case 'workbuddy':
      return {
        base: '/api/workbuddy-oauth',
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

// login-first 收尾(仅 trae):保存 provider 拿到新 id 后,把登录时落下的 pending 凭证绑定到该 id。
export function oauthClaimPending(kind: OAuthKind, providerId: string) {
  const { base, query } = endpoint(kind, providerId)
  return api<{ claimed: boolean }>('POST', `${base}/claim${query}`)
}

// ── WorkBuddy 账号池(单 provider 多账号,额度守护自动切换)─────────────
// 一个 workbuddy-login provider 维护账号池:status 列所有账号 + 当前服务账号;login 加账号;
// account 移除单账号;switch 手动切当前服务账号。
export interface WorkbuddyAccount {
  uid: string
  nickname?: string
  isActive: boolean // 当前服务账号(sticky)
  exhausted: boolean // 额度低于守护阈值、当前被跳过
  exhaustedUntil: number
}
export interface WorkbuddyPoolStatus {
  loggedIn: boolean
  accounts: WorkbuddyAccount[]
}
const wbQ = (providerId: string, uid?: string) =>
  `?providerId=${encodeURIComponent(providerId)}` +
  (uid ? `&uid=${encodeURIComponent(uid)}` : '')
export function workbuddyPoolStatus(providerId: string) {
  return api<WorkbuddyPoolStatus>('GET', `/api/workbuddy-oauth/status${wbQ(providerId)}`)
}
export function workbuddyRemoveAccount(providerId: string, uid: string) {
  return api('DELETE', `/api/workbuddy-oauth/account${wbQ(providerId, uid)}`)
}
export function workbuddySwitchAccount(providerId: string, uid: string) {
  return api('POST', `/api/workbuddy-oauth/switch${wbQ(providerId, uid)}`)
}
