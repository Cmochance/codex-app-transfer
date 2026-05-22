---
id: 40
priority: P2
type: refactor
status: active
created: 2026-05-23
related_pr: TBD
---

# MCP Marketplace + Deeplink 暂隐藏(后端代码已实现,等 registry 起好再暴露)

## 触发上下文

MCP tab 重做 PR(2026-05-23)按用户要求"4 sub-tab 单 PR ship":Servers / Plugins / Marketplace / Deeplink。

Servers + Plugins 立即可用;Marketplace + Deeplink **后端代码已完整实现**(handlers/mcp.rs marketplace/* endpoints / services/marketplace.rs / services/codex_plugins.rs install_tarball / src-tauri/src/main.rs deep-link plugin 注册 / frontend codexMcpHandleDeeplink),但**前端 sub-nav 暂时只显 Servers + Plugins 两个**。

用户决定隐藏原因:registry repo `Cmochance/codex-app-transfer-registry` 还没起,UI 直接打开会显示空列表 + fetch 错误,体验差。先把代码 ship 进 main(防止后续重复实现),UI 待 registry 起好再放出来。

## 当前隐藏状态

**前端 hidden**:
- `frontend/index.html` `#codexMcpSubnav` 只渲染 Servers + Plugins 两个 button,注释掉 Marketplace
- `#codexMcpMarketplace*` subpane DOM 仍在,只是 sub-nav 不暴露 → 不可达
- Deeplink confirmation modal `#codexMcpDeeplinkModal` 仍在,但 deeplink 触发需要外部 URL(实际触发不了因为没注册过链接的地方)

**后端可用**(可独立测,API 直接调):
- `GET /api/codex/mcp/marketplace/sources` / `/index` `force_refresh=true` 全工作
- `POST /api/codex/mcp/marketplace/sources/{add,remove,toggle}` 全工作
- `POST /api/codex/mcp/plugins/install`(tar.gz download + atomic 解压)全工作
- Tauri deep-link plugin 注册了 `codex-app-transfer://` scheme(`tauri.conf.json` plugins.deep-link.desktop.schemes),系统层可接 URL,只是 frontend confirmation 链路因没 marketplace 入口不会被触发

## 激活前置条件

1. **起 `Cmochance/codex-app-transfer-registry` repo** + `main` 分支根目录放 `registry.json`,schema(已 align `services/marketplace.rs::RegistryDoc`):

```json
{
  "version": 1,
  "servers": [
    {
      "id": "fetch",
      "name": "Fetch",
      "description": "通用 HTTP 抓取",
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-fetch"]
    }
  ],
  "plugins": [
    {
      "id": "demo",
      "marketplace": "official",
      "version": "1.0.0",
      "tarballUrl": "https://github.com/.../demo-1.0.0.tar.gz",
      "description": "示例 plugin",
      "capabilities": { "mcpServers": 2, "skills": 3, "apps": 0 }
    }
  ]
}
```

2. **curate 至少 5 个 server preset**(fetch / time / memory / sequential-thinking / context7 等)+ 1 个 plugin 走通流程

3. **frontend 取消注释 Marketplace sub-nav button**:
   - `frontend/index.html` 改回 3 个 sub-nav item
   - 不需要改其他代码,subpane / handler 都已就位

4. **Deeplink share URL 生成**:Marketplace 详情页加 "Share link" 按钮,生成 `codex-app-transfer://v1/import?resource=mcp-server&name=...&config=<base64-JSON>` URL 让用户分享(README 加文档)

## 启用激活后的验证清单

- [ ] sub-nav 显 3 个 tab,Marketplace 可点
- [ ] 官方源默认 fetch 成功(无 error 卡片),server preset / plugin 列表正常
- [ ] 自定义源添加 + toggle + remove 工作
- [ ] 一键添加 server → Servers tab 出现新条目 + ~/.codex/config.toml 写入 `[mcp_servers.<id>]`
- [ ] 一键安装 plugin → tar.gz 下载 → atomic 解压到 `~/.codex/plugins/cache/<market>/<plugin>/<ver>/` → `[plugins."<key>"] enabled = true` 落入 config.toml
- [ ] 浏览器点 `codex-app-transfer://v1/import?...` 链接 → 弹本 app + confirmation modal 显 spec preview → 用户点确认才执行
- [ ] deeplink 校验:tarball_url 必须 https / base64 解码 / JSON parse / 长度 ≤ 16KB

## 相关代码锚点

- 隐藏点:`frontend/index.html` 的 `<!-- Marketplace + Deeplink 进 followup #40 -->` 注释
- 后端:`src-tauri/src/admin/services/marketplace.rs` / `src-tauri/src/admin/services/codex_plugins.rs::install_tarball` / `src-tauri/src/admin/handlers/mcp.rs`
- 前端:`frontend/js/app.js` `codexMcpReloadSources` / `codexMcpReloadMarketIndex` / `codexMcpHandleDeeplink` / `codexMcpMarketInstallServer` / `codexMcpMarketInstallPlugin`
- Tauri 注册:`src-tauri/src/main.rs` `app.deep_link().on_open_url` + `src-tauri/tauri.conf.json` `plugins.deep-link.desktop.schemes`

## 不在范围

- 不动 Servers / Plugins 两个 sub-tab(已正式 ship)
- 不删除已实现的 marketplace / deeplink 后端跟前端代码(只是 UI 入口隐藏)
- 不起 registry repo(用户自己起,跟代码无关)
