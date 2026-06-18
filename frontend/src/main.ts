import { createApp } from 'vue'
import App from './App.vue'

// Stage 1: 最小挂载，验证脚手架 + 构建链 + CSP 合规。
// router / pinia / 全局样式 / i18n 在 Stage 2 起接入。
createApp(App).mount('#app')
