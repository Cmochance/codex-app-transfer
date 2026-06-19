import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'

// FineTune redesign: 顶部 5 tab(providers/proxy/usage/codex/settings)+ 次要页
// codex-skin(Codex Desktop 皮肤注入)/ desktop(配置状态)。Dashboard/Guide 已 drop
// (FineTune 主页 = providers,引导内容并入文档)。
const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/providers' },
  { path: '/providers', name: 'providers', component: () => import('@/pages/ProvidersPage.vue'), meta: { navKey: 'nav.providers', icon: 'plug' } },
  { path: '/proxy', name: 'proxy', component: () => import('@/pages/ProxyPage.vue'), meta: { navKey: 'nav.proxy', icon: 'radio' } },
  { path: '/usage', name: 'usage', component: () => import('@/pages/UsagePage.vue'), meta: { navKey: 'nav.usage', icon: 'chart' } },
  { path: '/settings', name: 'settings', component: () => import('@/pages/SettingsPage.vue'), meta: { navKey: 'nav.settings', icon: 'settings' } },
  { path: '/codex', name: 'codex', component: () => import('@/pages/CodexPage.vue'), meta: { navKey: 'nav.codex', icon: 'bookmark' } },
  { path: '/marketplace', name: 'marketplace', component: () => import('@/pages/MarketplacePage.vue'), meta: { navKey: 'nav.market', icon: 'store' } },
  { path: '/codex-skin', name: 'codex-skin', component: () => import('@/pages/CodexSkinPage.vue'), meta: { navKey: 'nav.theme', icon: 'palette' } },
  { path: '/desktop', name: 'desktop', component: () => import('@/pages/DesktopPage.vue'), meta: { hidden: true } },
  { path: '/:pathMatch(.*)*', redirect: '/providers' },
]

export const router = createRouter({
  history: createWebHashHistory(),
  routes,
})
