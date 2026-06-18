import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import Icons from 'unplugin-icons/vite'
import { fileURLToPath, URL } from 'node:url'

// Tauri webview 在 prod 通过 cas://localhost/ 加载，由后端 axum static_files.rs
// 吐 include_dir! 嵌入的 dist 字节，并附严格 CSP（script-src 'self'，禁 inline/eval）。
// 因此构建产物必须：① 纯外链 ES module、零 inline <script>；② Vue runtime-only
// （SFC 预编译，无模板编译器/eval）。下面的配置确保这两点。
// https://vite.dev/config/
export default defineConfig({
  plugins: [
    vue(), // @vitejs/plugin-vue 在构建期把 SFC 编译成 render 函数，运行时不含 eval
    // 编译期把 lucide 图标内联成 Vue SVG 组件(~icons/lucide/*), CSP 友好、tree-shake、无运行时字体
    Icons({ compiler: 'vue3' }),
  ],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  // cas://localhost/ 下用相对路径加载资源，命中 serve_static 的 trim_start_matches('/')
  base: './',
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
    // dev 时 webview 指向此 devUrl，/api 经此代理到后端 debug TCP listener
    // （main.rs 的 #[cfg(debug_assertions)] 监听 127.0.0.1:18900，跑同一 axum router）
    proxy: { '/api': 'http://127.0.0.1:18900' },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022', // WebKit(Tauri macOS/Linux) + WebView2(Windows) 均支持
    // ★ CSP 合规关键：关闭 Vite 默认注入 index.html 的 inline modulepreload polyfill
    //   <script>，否则违反 script-src 'self' → 整个应用白屏。关掉后改用外链
    //   <link rel="modulepreload">（CSP 不拦 link）。
    modulePreload: { polyfill: false },
    cssCodeSplit: true,
    rollupOptions: {
      output: {
        manualChunks: { vue: ['vue', 'vue-router', 'pinia'] },
      },
    },
  },
})
