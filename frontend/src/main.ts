import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import { useAppearance } from './composables/useAppearance'
import { setLocale, cachedLocale } from './i18n'
import './styles/index.css'

// 启动即应用缓存的主题 + 语言(Vue 挂载前 #app 为空, 天然无首屏闪烁)。
// Stage 3 接 settings store 后, 再用后端 settings.theme/language 覆盖缓存值。
useAppearance().load()
setLocale(cachedLocale())

createApp(App).use(createPinia()).use(router).mount('#app')
