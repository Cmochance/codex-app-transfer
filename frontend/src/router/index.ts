import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'

// 路由表映射旧 SPA 的 9 个页面 + providers/add 子路由 + 隐藏 desktop。
// 原 #theme(Codex Desktop 皮肤注入)改名 /codex-skin, 与"本应用三主题"区分。
const routes: RouteRecordRaw[] = [
  // FineTune 风顶部 tab 无 dashboard, 默认进主页 providers(dashboard 路由保留, 仅不在 tab)
  { path: '/', redirect: '/providers' },
  { path: '/dashboard', name: 'dashboard', component: () => import('@/pages/DashboardPage.vue'), meta: { navKey: 'nav.dashboard', icon: 'gauge', hidden: true } },
  { path: '/providers', name: 'providers', component: () => import('@/pages/ProvidersPage.vue'), meta: { navKey: 'nav.providers', icon: 'plug' } },
  { path: '/providers/add', name: 'provider-form', component: () => import('@/pages/ProviderFormPage.vue') },
  { path: '/proxy', name: 'proxy', component: () => import('@/pages/ProxyPage.vue'), meta: { navKey: 'nav.proxy', icon: 'radio' } },
  { path: '/usage', name: 'usage', component: () => import('@/pages/UsagePage.vue'), meta: { navKey: 'nav.usage', icon: 'chart' } },
  { path: '/settings', name: 'settings', component: () => import('@/pages/SettingsPage.vue'), meta: { navKey: 'nav.settings', icon: 'settings' } },
  { path: '/codex', name: 'codex', component: () => import('@/pages/CodexPage.vue'), meta: { navKey: 'nav.codex', icon: 'bookmark' } },
  { path: '/codex-skin', name: 'codex-skin', component: () => import('@/pages/CodexSkinPage.vue'), meta: { navKey: 'nav.theme', icon: 'palette' } },
  { path: '/guide', name: 'guide', component: () => import('@/pages/GuidePage.vue'), meta: { navKey: 'nav.guide', icon: 'book' } },
  { path: '/desktop', name: 'desktop', component: () => import('@/pages/DesktopPage.vue'), meta: { hidden: true } },
  { path: '/:pathMatch(.*)*', redirect: '/providers' },
]

export const router = createRouter({
  history: createWebHashHistory(),
  routes,
})
