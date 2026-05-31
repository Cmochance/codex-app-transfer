(function () {
  const routes = ["dashboard", "providers/add", "providers", "desktop", "proxy", "usage", "settings", "codex", "theme", "guide"];
  const providerFormModelSlots = [
    { key: "default", label: "Default", icon: "bi-circle-fill", iconClass: "default", source: "未配置映射时默认使用这一项", required: true },
    { key: "gpt_5_5", label: "gpt-5.5", icon: "bi-circle", iconClass: "default", source: "gpt-5.5" },
    { key: "gpt_5_4", label: "gpt-5.4", icon: "bi-circle", iconClass: "default", source: "gpt-5.4" },
    { key: "gpt_5_4_mini", label: "gpt-5.4-mini", icon: "bi-circle", iconClass: "default", source: "gpt-5.4-mini" },
    { key: "gpt_5_3_codex", label: "gpt-5.3-codex", icon: "bi-circle", iconClass: "default", source: "gpt-5.3-codex" },
    { key: "gpt_5_2", label: "gpt-5.2", icon: "bi-circle", iconClass: "default", source: "gpt-5.2" },
  ];
  const availableThemes = ["default", "green", "orange", "gray", "dark", "white"];
  // **2026-05-10 修复**:加 google_api_key(Gemini native 用 `x-goog-api-key` header)。
  // 旧实现白名单只有 bearer / x-api-key / none,Google AI Studio preset 的
  // authScheme=google_api_key 经 setAuthSchemeValue 校验失败 → fallback 'bearer'
  // → backend 用 Authorization: Bearer 调 Gemini /v1beta/models → 401(Google 不接 Bearer)
  // → 测速看似绿(401 走 auth_not_verified 路径)但实际从未真正鉴权过 + 列模型失败。
  const providerAuthSchemes = ["bearer", "x-api-key", "google_api_key", "grok_cookie", "none"];
  const providerFormDefaultRows = ["default", "gpt_5_5", "gpt_5_4", "gpt_5_4_mini", "gpt_5_3_codex", "gpt_5_2"];
  let pendingDeleteId = null;
  let selectedPreset = null;
  let presetCache = [];
  // MOC-91:是否在 preset 选择器里展示「灰色」(TOS-gray / 实验性)preset。默认 false
  // (隐藏)。仅过滤**展示**,presetCache 始终保留全量供已配置 provider 反查 preset。
  let showGrayPresets = false;
  let formApiFormatValue = "openai_chat";
  let formModelCapabilities = {};
  let formRequestOptions = {};
  let providerFormMappings = {};
  let providerFormRows = [...providerFormDefaultRows];
  let providerFormCustomLabels = {};
  let customRowCounter = 0;
  let providerAvailableModels = [];
  let openProviderSlotMenuIndex = null;
  let openProviderModelMenuKey = null;
  let baseUrlMenuOpen = false;
  let editingProviderId = null;
  let deleteModal = null;
  let restartReminderModal = null;
  let toast = null;
  let updateCheckCache = null;
  let updateInstallPhase = "idle";

  function $(selector, root = document) {
    return root.querySelector(selector);
  }

  function $all(selector, root = document) {
    return Array.from(root.querySelectorAll(selector));
  }

  function routeFromHash() {
    const hash = window.location.hash.replace(/^#/, "");
    return routes.includes(hash) ? hash : "dashboard";
  }

  function showToast(message) {
    $("#toastBody").textContent = message;
    toast.show();
  }

  // MOC-62:MCP 凭据文件整个丢失但镜像有备份时弹确认。原生 dialog.ask(yes/no):
  // 确认 → 从备份恢复;否 → 忽略(删镜像,停止再弹,接受"凭据已不在")。
  // dialog 不可用时退回 window.confirm。in-flight guard 防重入重复弹。
  let mcpRestorePromptInFlight = false;
  async function mcpCredentialsHandleRestorePrompt(count) {
    if (mcpRestorePromptInFlight) return;
    mcpRestorePromptInFlight = true;
    try {
      let restore;
      try {
        const dialog = window.__TAURI__?.dialog;
        const body = tFmt("mcp.restorePromptBody", { count });
        if (dialog && typeof dialog.ask === "function") {
          restore = await dialog.ask(body, {
            title: t("mcp.restorePromptTitle"),
            kind: "warning",
          });
        } else {
          restore = window.confirm(`${t("mcp.restorePromptTitle")}\n\n${body}`);
        }
      } catch (err) {
        console.error("mcp restore prompt:", err);
        return;
      }
      try {
        if (restore) {
          const r = await CCApi.restoreMcpCredentials();
          showToast(tFmt("mcp.restoreDone", { count: r?.restored ?? count }));
        } else {
          await CCApi.discardMcpCredentialsMirror();
          showToast(t("mcp.restoreDismissed"));
        }
      } catch (err) {
        showToast(err.message || t("toast.requestFailed"));
      }
    } finally {
      mcpRestorePromptInFlight = false;
    }
  }

  // MOC-62:load 时查询是否有可恢复的 MCP 凭据备份(整文件丢失 + 镜像有备份),有则弹
  // 确认。轮询比一次性 startup event 可靠(后者可能在 listener 注册前 emit 丢失)。
  async function mcpCredentialsCheckRestoreOnLoad() {
    try {
      const s = await CCApi.getMcpCredentialsStatus();
      const count = Number(s?.restoreAvailable) || 0;
      if (count > 0) await mcpCredentialsHandleRestorePrompt(count);
    } catch (err) {
      console.error("mcp restore status:", err);
    }
  }

  function showRestartReminder() {
    restartReminderModal?.show();
  }

  function dismissRestartReminderLater() {
    restartReminderModal?.hide();
  }

  async function restartCodexAppNow({
    buttonId = "restartReminderNow",
    fallbackLabelKey = "restartReminder.now",
    hideModal = true,
  } = {}) {
    const button = $(`#${buttonId}`);
    // MOC-20 / PR #281 fix:三种按钮形态都要兼容,不能直接改 button.textContent 否则抹 DOM:
    //   a) modal 内纯文本按钮(#restartReminderNow):textContent 含 label,直接改安全
    //   b) 含 icon + label span 按钮(dashboard quick-actions 形态):必须改 [data-i18n] 子节点
    //   c) icon-only 按钮(header `.theme-btn`):textContent = 空白,只能改 disabled + .is-loading class
    // 判断:有 [data-i18n] 子 → label swap;无子但 button.textContent.trim() 非空 → 改 button text;
    // 都不是 → icon-only,仅 disabled + class 反馈。
    const labelEl = button?.querySelector("[data-i18n]");
    const buttonHasText = button && !labelEl && button.textContent.trim().length > 0;
    const swapEl = labelEl || (buttonHasText ? button : null);
    const original = swapEl?.textContent;
    try {
      if (button) {
        button.disabled = true;
        button.classList.add("is-loading");
      }
      if (swapEl) swapEl.textContent = t("restartReminder.restarting") || "重启中…";
      await CCApi.restartCodexApp();
      if (hideModal) restartReminderModal?.hide();
      showToast(t("toast.codexAppRestartRequested"));
    } catch (error) {
      console.error(error);
      showToast(error.message || t("toast.codexAppRestartFailed"));
    } finally {
      if (button) {
        button.disabled = false;
        button.classList.remove("is-loading");
      }
      if (swapEl) swapEl.textContent = original || t(fallbackLabelKey);
    }
  }

  function t(key) {
    return CCI18n.t(key);
  }

  // formatI18n removed (M2 migration) — 使用 tFmt 统一(line ~1131),tFmt 多了
  // missing-key + unsubstituted-placeholder warning,行为更安全。所有原 callsite
  // 已迁到 tFmt


  function iconMarkup(item) {
    if (item.logo) return `<img src="${item.logo}" alt="">`;
    if (item.iconText) return `<span>${item.iconText}</span>`;
    return `<i class="bi ${item.icon || "bi-plug-fill"}"></i>`;
  }

  function escapeHtml(value) {
    return String(value ?? "").replace(/[&<>"']/g, (char) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      "\"": "&quot;",
      "'": "&#39;",
    }[char]));
  }

  function safeHttpUrl(value) {
    try {
      const parsed = new URL(String(value || ""), window.location.origin);
      if (["http:", "https:"].includes(parsed.protocol)) return parsed.href;
    } catch (error) {
      return "#";
    }
    return "#";
  }

  function normalizePresetKey(value) {
    return String(value || "").trim().toLowerCase().replace(/\/+$/, "");
  }

  function presetExists(preset, providers) {
    // 「自定义第三方」是无限重复添加入口卡片(用户每次填不同 baseUrl + apiKey),
    // 永远视为不存在 → 永远在 dashboard available presets 列表显示
    if (preset.id === "custom-third-party") return false;
    const presetName = normalizePresetKey(preset.name);
    const presetUrl = normalizePresetKey(preset.baseUrl);
    const presetApiFormat = String(preset.apiFormat || "").toLowerCase();
    return providers.some((provider) => {
      // **多 preset 共享上游场景**:eg gemini-cli-oauth + antigravity-oauth 都
      // 走 cloudcode-pa.googleapis.com 上游但 apiFormat 不同 (前者
      // gemini_cli_oauth 后者 antigravity_oauth) — 加一个另一个不能被
      // baseUrl 去重隐藏。同 apiFormat 才视为同 preset(2026-05-11 修)
      if (presetApiFormat && String(provider.apiFormat || "").toLowerCase() !== presetApiFormat) {
        return false;
      }
      return (
        normalizePresetKey(provider.name) === presetName
        || normalizePresetKey(provider.baseUrl) === presetUrl
      );
    });
  }

  function updatePresetSelection() {
    const selectedId = selectedPreset?.id || "";
    $all("#presetList [data-preset]").forEach((button) => {
      const active = button.dataset.preset === selectedId;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", active ? "true" : "false");
      const icon = $("i:last-child", button);
      if (icon) icon.className = `bi ${active ? "bi-check2" : "bi-chevron-right"}`;
    });
  }

  function normalizeApiFormat(apiFormat) {
    const v = String(apiFormat || "").toLowerCase().replace(/-/g, "_");
    if (["responses", "openai_responses"].includes(v)) return { key: "responses", canonical: "responses" };
    if (["anthropic_messages", "anthropic", "claude", "messages", "claude_messages"].includes(v)) {
      return { key: "anthropic", canonical: "anthropic_messages" };
    }
    if (["gemini_native", "google_ai_studio", "gemini"].includes(v)) return { key: "geminiNative", canonical: "gemini_native" };
    if (["gemini_cli_oauth", "gemini_cli", "google_oauth_cloud_code"].includes(v)) return { key: "geminiCliOauth", canonical: "gemini_cli_oauth" };
    if (["grok_web", "grok", "grok_com"].includes(v)) return { key: "grokWeb", canonical: "grok_web" };
    if (["antigravity_oauth", "antigravity", "google_oauth_antigravity"].includes(v)) return { key: "antigravityOauth", canonical: "antigravity_oauth" };
    return { key: "openaiChat", canonical: "openai_chat" };
  }

  /// Cloud Code Assist OAuth provider 的 per-canonical 配置:i18n key 前缀(决定
  /// 整套 UI 文案 namespace)+ CCApi 方法名(login/status/logout)。新增 OAuth
  /// provider 时往这里加一条即可,setOauthRowState/refreshOauthStatusUi/
  /// handleOauthLogin/handleOauthLogout 都 share 这个 dispatch 表
  const OAUTH_PROVIDER_CONFIGS = {
    gemini_cli_oauth: {
      i18nPrefix: "geminiOauth",
      api: {
        getStatus: () => CCApi.getGeminiOauthStatus(),
        login: () => CCApi.loginGeminiOauth(),
        logout: () => CCApi.logoutGeminiOauth(),
      },
    },
    antigravity_oauth: {
      i18nPrefix: "antigravityOauth",
      api: {
        getStatus: () => CCApi.getAntigravityOauthStatus(),
        login: () => CCApi.loginAntigravityOauth(),
        logout: () => CCApi.logoutAntigravityOauth(),
      },
    },
  };

  /// 当前 form 已选 apiFormat 对应的 OAuth provider config(none → null)。
  /// setOauthRowState 时缓存,refresh / login / logout 复用避免每次重 lookup
  let activeOauthConfig = null;

  /// Cloud Code Assist OAuth 路径不需要 apiKey input,改 OAuth login UI block。
  /// 返 true 表示当前协议是 OAuth 模式(gemini_cli_oauth 或 antigravity_oauth),
  /// 调用方据此切换 form 显示
  function isOauthApiFormat(apiFormat) {
    const { canonical } = normalizeApiFormat(apiFormat);
    return Object.prototype.hasOwnProperty.call(OAUTH_PROVIDER_CONFIGS, canonical);
  }

  function renderApiFormatDisplay(apiFormat) {
    const { key, canonical } = normalizeApiFormat(apiFormat);
    formApiFormatValue = canonical;
    const nameEl = $("#providerApiFormatName");
    const detailEl = $("#providerApiFormatDetail");
    if (nameEl) {
      const nameKey = `apiFormatDisplay.${key}.name`;
      nameEl.dataset.i18n = nameKey;
      nameEl.textContent = t(nameKey);
    }
    if (detailEl) {
      const detailKey = `apiFormatDisplay.${key}.detail`;
      detailEl.dataset.i18n = detailKey;
      detailEl.textContent = t(detailKey);
    }
  }

  function updateApiFormatSelectDetail(value) {
    const { key, canonical } = normalizeApiFormat(value);
    formApiFormatValue = canonical;
    const detailEl = $("#providerApiFormatSelectDetail");
    if (detailEl) {
      const detailKey = `apiFormatDisplay.${key}.detail`;
      detailEl.dataset.i18n = detailKey;
      detailEl.textContent = t(detailKey);
    }
    // 协议切换 → 重渲 mappings UI 让 default required 状态跟当前协议同步
    // (direct 模式 default 解锁为可空,其他场景仍 required)
    setProviderMappings(providerFormMappings);
    // OAuth 模式切换:apiFormat=gemini_cli_oauth 时隐藏 apiKey input,显示 OAuth UI
    setOauthRowState(canonical);
  }

  // 控制 web_search 配置开关 row 的显示 + 初始 checkbox state + provider-specific
  // hint 文案。preset.supportsWebSearch === true 才显示(Kimi / Kimi Code / MiMo
  // PAYG / MiMo Token Plan 四家;Gemini OpenAI compat chat 不支持 grounding,
  // 已实测 5 种 variant 全 400,故不开)。
  // hint 文案按 presetId 选 i18n key,fallback 到 .default;切换 preset 时
  // 同步更新 dataset.i18n + textContent,语言切换 + preset 切换都不会留旧文案。
  function setWebSearchRow(supports, enabled, presetId) {
    const row = $("#providerWebSearchRow");
    const cb = $("#providerWebSearchEnabled");
    const hint = $("#providerWebSearchHint");
    if (row) row.hidden = !supports;
    if (cb) cb.checked = !!enabled;
    if (hint) {
      const specificKey = presetId
        ? `providersAdd.webSearchEnabledHint.${presetId}`
        : null;
      const fallbackKey = "providersAdd.webSearchEnabledHint.default";
      const useKey = specificKey && t(specificKey) !== specificKey ? specificKey : fallbackKey;
      hint.dataset.i18n = useKey;
      hint.textContent = t(useKey);
    }
  }

  function setApiFormatMode(allowSelect, currentValue) {
    const displayEl = $("#providerApiFormatDisplay");
    const selectableEl = $("#providerApiFormatSelectable");
    const selectEl = $("#providerApiFormatSelect");
    if (displayEl) displayEl.hidden = allowSelect;
    if (selectableEl) selectableEl.hidden = !allowSelect;
    if (allowSelect && selectEl) {
      const { canonical } = normalizeApiFormat(currentValue);
      selectEl.value = canonical;
      updateApiFormatSelectDetail(canonical);
    }
  }

  function firstHealthMessage(health) {
    return health?.issues?.[0]?.message || "";
  }

  function renderDesktopHealthWarning(selector, health) {
    const warning = $(selector);
    if (!warning) return;
    const message = firstHealthMessage(health);
    warning.hidden = !message;
    const text = $("span", warning);
    if (text) text.textContent = message;
  }

  function renderUpdateBadge(result) {
    const badge = $("#dashboardUpdateBadge");
    const available = !!result?.updateAvailable;
    const installButton = $("#settingsInstallUpdate");
    const busy = updateInstallPhase !== "idle";
    if (badge) {
      badge.hidden = !(available || busy);
      badge.disabled = busy;
      badge.title = available && !busy ? t("settings.installUpdate") : "";
      badge.setAttribute("aria-label", available ? t("settings.installUpdate") : t("dashboard.updateAvailable"));
    }
    if (installButton) {
      installButton.hidden = !(available || busy);
      installButton.disabled = busy;
    }
    const badgeIcon = badge ? $("i", badge) : null;
    if (badgeIcon) {
      badgeIcon.className = busy ? "bi bi-arrow-repeat" : "bi bi-cloud-arrow-down";
    }
    const installIcon = installButton ? $("i", installButton) : null;
    if (installIcon) {
      installIcon.className = busy ? "bi bi-arrow-repeat" : "bi bi-download";
    }
    const text = badge ? $("span", badge) : null;
    if (text) {
      if (updateInstallPhase === "downloading") {
        text.textContent = t("settings.downloadingUpdate");
      } else if (updateInstallPhase === "installing") {
        text.textContent = t("settings.installingUpdate");
      } else if (available) {
        text.textContent = result.latestVersion
          ? `${t("dashboard.updateAvailable")} ${result.latestVersion}`
          : t("dashboard.updateAvailable");
      }
    }
    const installText = installButton ? $("span", installButton) : null;
    if (installText) {
      if (updateInstallPhase === "downloading") {
        installText.textContent = t("settings.downloadingUpdate");
      } else if (updateInstallPhase === "installing") {
        installText.textContent = t("settings.installingUpdate");
      } else {
        installText.textContent = t("settings.installUpdate");
      }
    }
  }

  function setUpdateInstallPhase(phase = "idle") {
    updateInstallPhase = phase;
    renderUpdateBadge(updateCheckCache);
  }

  async function refreshUpdateBadge(force = false) {
    try {
      updateCheckCache = await CCApi.checkUpdate("");
      renderUpdateBadge(updateCheckCache);
    } catch (error) {
      console.warn(error);
      updateCheckCache = null;
      renderUpdateBadge(null);
    }
  }

  function emptyMappings() {
    return Object.fromEntries(providerFormModelSlots.map((slot) => [slot.key, ""]));
  }

  const predefinedSlotKeys = new Set(providerFormModelSlots.map((s) => s.key));

  function isCustomMappingRow(key) {
    return key.startsWith("_custom_");
  }

  function normalizeMappings(mappings = {}) {
    const normalized = emptyMappings();
    if (!mappings || typeof mappings !== "object") return normalized;
    normalized.default = String(mappings.default || "").trim();
    normalized.gpt_5_5 = String(mappings.gpt_5_5 || "").trim();
    normalized.gpt_5_4 = String(mappings.gpt_5_4 || "").trim();
    normalized.gpt_5_4_mini = String(mappings.gpt_5_4_mini || "").trim();
    normalized.gpt_5_3_codex = String(mappings.gpt_5_3_codex || "").trim();
    normalized.gpt_5_2 = String(mappings.gpt_5_2 || "").trim();
    // preserve custom (non-predefined) keys
    for (const [key, value] of Object.entries(mappings)) {
      if (!predefinedSlotKeys.has(key)) {
        const trimmed = String(value || "").trim();
        if (trimmed) normalized[key] = trimmed;
      }
    }
    return normalized;
  }

  function normalizeCapabilities(capabilities = {}) {
    if (!capabilities || typeof capabilities !== "object") return {};
    return Object.fromEntries(Object.entries(capabilities).filter(([, value]) => (
      value && typeof value === "object" && value.supports1m === true
    )));
  }

  const EFFORT_VALUES = ["low", "medium", "high", "xhigh", "max"];

  function normalizeResponsesBlock(source = {}) {
    if (!source || typeof source !== "object") return {};
    const block = {};
    if (["enabled", "disabled"].includes(source.thinking?.type)) {
      block.thinking = { type: source.thinking.type };
    }
    if (EFFORT_VALUES.includes(source.output_config?.effort)) {
      block.output_config = { effort: source.output_config.effort };
    }
    return block;
  }

  function normalizeChatBlock(source = {}) {
    if (!source || typeof source !== "object") return {};
    const block = {};
    // DeepSeek V4：thinking 对象只接受 type；reasoning_effort 在请求体顶层
    const thinkingType = source.thinking?.type;
    if (["enabled", "disabled"].includes(thinkingType)) {
      block.thinking = { type: thinkingType };
    }
    if (EFFORT_VALUES.includes(source.reasoning_effort)) {
      block.reasoning_effort = source.reasoning_effort;
    }
    return block;
  }

  function normalizeRequestOptions(options = {}) {
    if (!options || typeof options !== "object") return {};
    // 兼容旧配置：anthropic 键重命名为 responses
    const responsesSource = options.responses && typeof options.responses === "object"
      ? options.responses
      : (options.anthropic && typeof options.anthropic === "object" ? options.anthropic : null);
    const result = {};
    const responsesBlock = normalizeResponsesBlock(responsesSource || (responsesSource === null ? options : {}));
    if (Object.keys(responsesBlock).length) result.responses = responsesBlock;
    const chatBlock = normalizeChatBlock(options.chat);
    if (Object.keys(chatBlock).length) result.chat = chatBlock;
    // **顶级 boolean / 标量字段**:web_search_enabled 由 backend
    // `convert_web_search_tool` 读 `provider.request_options.web_search_enabled`
    // 决定是否启用 web 搜索(MiMo / Kimi / Gemini 等支持 web_search 的 provider)。
    // 之前版本 normalize 不保留此字段 → frontend 编辑保存即剥光,用户必须
    // 手改 config.json,UX 痛点。本字段必须保留(boolean),否则功能失效。
    if (typeof options.web_search_enabled === "boolean") {
      result.web_search_enabled = options.web_search_enabled;
    }
    return result;
  }

  function mergeRequestOptions(base = {}, extra = {}) {
    const baseNorm = normalizeRequestOptions(base);
    const extraNorm = normalizeRequestOptions(extra);
    const merged = {};
    const responsesMerged = { ...(baseNorm.responses || {}), ...(extraNorm.responses || {}) };
    if (Object.keys(responsesMerged).length) merged.responses = responsesMerged;
    const chatMerged = { ...(baseNorm.chat || {}), ...(extraNorm.chat || {}) };
    if (Object.keys(chatMerged).length) merged.chat = chatMerged;
    return normalizeRequestOptions(merged);
  }

  function clearRequestOptions(base = {}, option = {}) {
    const baseNorm = normalizeRequestOptions(base);
    const optionNorm = normalizeRequestOptions(option);
    const next = {};
    if (baseNorm.responses) {
      const block = { ...baseNorm.responses };
      Object.keys(optionNorm.responses || {}).forEach((key) => { delete block[key]; });
      if (Object.keys(block).length) next.responses = block;
    }
    if (baseNorm.chat) {
      const block = { ...baseNorm.chat };
      Object.keys(optionNorm.chat || {}).forEach((key) => { delete block[key]; });
      if (Object.keys(block).length) next.chat = block;
    }
    return normalizeRequestOptions(next);
  }

  function requestOptionsMatch(left = {}, right = {}) {
    return JSON.stringify(normalizeRequestOptions(left)) === JSON.stringify(normalizeRequestOptions(right));
  }

  function capabilitiesMatch(left = {}, right = {}) {
    return JSON.stringify(normalizeCapabilities(left)) === JSON.stringify(normalizeCapabilities(right));
  }

  function mergeCapabilities(base = {}, extra = {}) {
    return {
      ...normalizeCapabilities(base),
      ...normalizeCapabilities(extra),
    };
  }

  function clearCapabilities(base = {}, option = {}) {
    const current = normalizeCapabilities(base);
    Object.keys(normalizeCapabilities(option)).forEach((modelId) => {
      delete current[modelId];
    });
    return current;
  }

  function optionEnabled(option = {}, currentMappings = collectProviderMappings()) {
    const hasModels = option.models && typeof option.models === "object";
    const hasRequestOptions = option.requestOptions && typeof option.requestOptions === "object";
    const hasCapabilities = option.modelCapabilities && typeof option.modelCapabilities === "object";
    const modelsOk = !hasModels || modelsMatch(option.models, currentMappings);
    const requestOptionsOk = !hasRequestOptions || requestOptionsMatch(option.requestOptions, formRequestOptions);
    const optionChangesModels = hasModels && !modelsMatch(option.models, selectedPreset?.models || {});
    const capabilitiesOk = !hasCapabilities || optionChangesModels || capabilitiesMatch(option.modelCapabilities, formModelCapabilities);
    if (hasModels || hasRequestOptions || hasCapabilities) {
      return modelsOk && requestOptionsOk && capabilitiesOk;
    }
    return false;
  }

  function modelsMatch(left = {}, right = {}) {
    const a = normalizeMappings(left);
    const b = normalizeMappings(right);
    return providerFormModelSlots.every((slot) => (a[slot.key] || "") === (b[slot.key] || ""));
  }

  function presetMatchesProvider(preset, provider) {
    if (!preset || !provider) return false;
    const baseUrlOptions = Array.isArray(preset.baseUrlOptions) ? preset.baseUrlOptions : [];
    // **多 preset 共享上游场景**:apiFormat 不同就不算同 preset
    // (gemini-cli-oauth vs antigravity-oauth 同 baseUrl 但不同协议;2026-05-11 修)
    if (preset.apiFormat && provider.apiFormat
        && String(preset.apiFormat).toLowerCase() !== String(provider.apiFormat).toLowerCase()) {
      return false;
    }
    return normalizePresetKey(preset.name) === normalizePresetKey(provider.name)
      || normalizePresetKey(preset.baseUrl) === normalizePresetKey(provider.baseUrl)
      || baseUrlOptions.some((option) => normalizePresetKey(option?.value) === normalizePresetKey(provider.baseUrl));
  }

  function presetBaseUrlOptions(preset = null) {
    return Array.isArray(preset?.baseUrlOptions) ? preset.baseUrlOptions.filter((option) => option?.value) : [];
  }

  function closeBaseUrlMenu() {
    if (!baseUrlMenuOpen) return;
    baseUrlMenuOpen = false;
    renderBaseUrlOptions();
  }

  function toggleBaseUrlMenu() {
    if (!presetBaseUrlOptions(selectedPreset).length) return;
    baseUrlMenuOpen = !baseUrlMenuOpen;
    renderBaseUrlOptions();
  }

  function setBaseUrlValue(value) {
    const input = $("#providerBaseUrl");
    if (!input) return;
    input.value = value;
    closeBaseUrlMenu();
  }

  function renderBaseUrlOptions(preset = selectedPreset) {
    const input = $("#providerBaseUrl");
    const trigger = $("#providerBaseUrlTrigger");
    const menu = $("#providerBaseUrlMenu");
    const wrap = $("#providerBaseUrlControl");
    const hint = $("#providerBaseUrlHint");
    if (!input || !trigger || !menu || !wrap || !hint) return;
    const options = presetBaseUrlOptions(preset);
    const helpText = String(preset?.baseUrlHint || "").trim();
    trigger.hidden = !options.length;
    trigger.disabled = !options.length;
    trigger.setAttribute("aria-expanded", options.length && baseUrlMenuOpen ? "true" : "false");
    wrap.classList.toggle("open", !!options.length && baseUrlMenuOpen);
    menu.innerHTML = options.map((option) => {
      const selected = input.value.trim() === option.value;
      return `
        <button
          class="baseurl-option ${selected ? "selected" : ""}"
          type="button"
          role="option"
          data-action="select-baseurl-option"
          data-baseurl-value="${escapeHtml(option.value)}"
          aria-selected="${selected ? "true" : "false"}"
        >
          <span>${escapeHtml(option.value)}</span>
          <small>${escapeHtml(option.label || "")}</small>
          ${selected ? '<i class="bi bi-check2"></i>' : ""}
        </button>
      `;
    }).join("");
    hint.textContent = helpText;
    hint.hidden = !helpText;
  }

  function capabilitiesForCurrentMappings(mappings = collectProviderMappings()) {
    const usedModelIds = new Set(Object.values(mappings).filter(Boolean));
    return Object.fromEntries(Object.entries(normalizeCapabilities(formModelCapabilities)).filter(([modelId]) => (
      usedModelIds.has(modelId)
    )));
  }

  function formMappingRowsFromMappings(mappings = {}) {
    const rows = [...providerFormDefaultRows];
    providerFormModelSlots.forEach((slot) => {
      if (slot.key !== "default" && mappings[slot.key] && !rows.includes(slot.key)) {
        rows.push(slot.key);
      }
    });
    // restore custom mapping rows (keys not in predefined slots)
    providerFormCustomLabels = {};
    for (const [key, value] of Object.entries(mappings)) {
      if (!predefinedSlotKeys.has(key) && String(value || "").trim()) {
        const customKey = `_custom_${customRowCounter++}`;
        rows.push(customKey);
        providerFormMappings[customKey] = String(value || "").trim();
        providerFormCustomLabels[customKey] = key;
      }
    }
    return rows;
  }

  function slotByKey(key) {
    return providerFormModelSlots.find((slot) => slot.key === key) || providerFormModelSlots[0];
  }

  function slotOptionsForRow(currentKey) {
    const used = new Set(providerFormRows.filter((key) => key !== currentKey));
    return providerFormModelSlots.filter((slot) => !used.has(slot.key));
  }

  // [MOC-69] provider 可用 model 列表项可能是 raw id string(gemini-cli / 普通
  // OpenAI provider),也可能是带元数据的 object(Antigravity `/api/antigravity-oauth/
  // models` 的 entry,含 display_name / recommended / tag_title)。下面 helper 统一
  // 抽取,**只影响展示文本 / 排序 / 标记,绝不改提交的 value** —— value 永远是 raw id。
  function modelEntryId(entry) {
    // 【硬约束】写进 select / mapping 的 value 永远是 raw id,绝不用 display_name。
    if (typeof entry === "string") return entry;
    if (!entry || typeof entry !== "object") return "";
    return entry.id || entry.model || "";
  }

  function modelEntryDisplayLabel(entry) {
    // 显示文本优先 display_name(如 "Gemini 3.5 Flash (High)"),fallback name -> id。
    // 其他 provider 的 entry(string / 无 display_name 的 object)自动回退到 id。
    if (typeof entry === "string") return entry;
    if (!entry || typeof entry !== "object") return "";
    return entry.display_name || entry.name || entry.id || entry.model || "";
  }

  function modelEntryIsRecommended(entry) {
    return !!(entry && typeof entry === "object" && entry.recommended === true);
  }

  function modelEntryTagLabel(entry) {
    // recommended model 的标记:优先 tag_title(如 "Fast"),没有就 i18n "推荐"。
    if (!modelEntryIsRecommended(entry)) return "";
    const tag = (entry && typeof entry.tag_title === "string") ? entry.tag_title.trim() : "";
    return tag || t("common.recommended");
  }

  // [MOC-69] 按 raw id 在 providerAvailableModels 里反查 entry —— 给映射「选框」显示
  // displayName 用。case-insensitive;找不到返回 null(其他 provider / 未拉取 /
  // 自定义 id → 选框 fallback 显 raw id,行为同改前)。
  function modelEntryById(id) {
    const target = String(id || "").trim().toLowerCase();
    if (!target) return null;
    return providerAvailableModels.find((e) => modelEntryId(e).trim().toLowerCase() === target) || null;
  }

  function providerModelOptionsMarkup(currentValue = "") {
    // recommended:true 置顶,其余保持原相对顺序(稳定排序);非推荐仍全量保留可见。
    // 其他 provider(全 string entry)recommended 恒 false,排序 no-op,行为同改前。
    const indexed = providerAvailableModels.map((entry, i) => ({ entry, i }));
    indexed.sort((a, b) => {
      const ra = modelEntryIsRecommended(a.entry) ? 0 : 1;
      const rb = modelEntryIsRecommended(b.entry) ? 0 : 1;
      if (ra !== rb) return ra - rb;
      return a.i - b.i;
    });
    return indexed.map(({ entry }) => {
      const modelId = modelEntryId(entry);
      const label = modelEntryDisplayLabel(entry);
      const isRecommended = modelEntryIsRecommended(entry);
      const tagLabel = modelEntryTagLabel(entry);
      return `
      <button
        class="mapping-slot-option ${modelId === currentValue ? "selected" : ""} ${isRecommended ? "recommended" : ""}"
        type="button"
        role="option"
        data-action="select-provider-model-option"
        data-model-value="${escapeHtml(modelId)}"
        aria-selected="${modelId === currentValue ? "true" : "false"}"
      >
        <span>${escapeHtml(label)}${tagLabel ? `<span class="model-option-tag" style="margin-left:6px;padding:1px 6px;border-radius:999px;font-size:11px;line-height:1.5;background:var(--primary-soft,#dbeafe);color:var(--primary,#2563eb);vertical-align:middle;">${escapeHtml(tagLabel)}</span>` : ""}</span>
        ${modelId === currentValue ? '<i class="bi bi-check2"></i>' : ""}
      </button>
    `;
    }).join("");
  }

  // [MOC-69] 映射「选框」渲染 —— raw id 能反查到带 displayName 的 entry 时,**只读**显示
  // displayName(用户只看 displayName,实际存储/发上游仍是 raw id,从右侧下拉选);否则
  // (未拉取 / 其他 provider / 自定义 id)保持可编辑输入显示 raw id,行为同改前。
  // 只读分支**不带** data-provider-model-input → input 事件不会用 displayName 覆盖存储值;
  // title 悬停可看真实 raw id。
  function providerModelValueInputMarkup(rowKey, index, currentProviderModel, isRequired) {
    const entry = modelEntryById(currentProviderModel);
    const displayName = entry ? modelEntryDisplayLabel(entry) : "";
    if (entry && displayName && displayName !== currentProviderModel) {
      return `
              <input
                class="form-control provider-model-input provider-model-input-readonly"
                id="providerMappingValue-${index}"
                value="${escapeHtml(displayName)}"
                data-model-value="${escapeHtml(currentProviderModel)}"
                data-action="toggle-provider-model-menu"
                data-row-key="${escapeHtml(rowKey)}"
                title="${escapeHtml(currentProviderModel)}"
                readonly
              >`;
    }
    return `
              <input
                class="form-control provider-model-input"
                id="providerMappingValue-${index}"
                data-provider-model-input="${escapeHtml(rowKey)}"
                value="${escapeHtml(currentProviderModel)}"
                placeholder="${escapeHtml(t("providersAdd.providerModelPlaceholder"))}"
                ${isRequired ? "required" : ""}
              >`;
  }

  function slotMenuMarkup(rowKey, index) {
    const slot = slotByKey(rowKey);
    const isRequired = rowKey === "default";
    const expanded = openProviderSlotMenuIndex === index;
    const options = slotOptionsForRow(rowKey).map((option) => (`
      <button
        class="mapping-slot-option ${option.key === rowKey ? "selected" : ""}"
        type="button"
        role="option"
        data-action="select-provider-model-slot"
        data-row-index="${index}"
        data-slot-key="${escapeHtml(option.key)}"
        aria-selected="${option.key === rowKey ? "true" : "false"}"
      >
        <span>${escapeHtml(option.label)}</span>
        ${option.key === rowKey ? '<i class="bi bi-check2"></i>' : ""}
      </button>
    `)).join("");
    return `
      <div class="mapping-slot-menu-wrap ${expanded ? "open" : ""}">
        <button
          class="form-select mapping-slot-trigger"
          id="providerMappingSlot-${index}"
          type="button"
          ${isRequired ? "disabled" : ""}
          data-action="toggle-provider-model-slot-menu"
          data-row-index="${index}"
          aria-haspopup="listbox"
          aria-expanded="${expanded ? "true" : "false"}"
        >
          <span>${escapeHtml(slot.label)}</span>
          <i class="bi bi-chevron-down"></i>
        </button>
        ${isRequired ? "" : `
          <div class="mapping-slot-menu" role="listbox" aria-labelledby="providerMappingSlot-${index}">
            ${options}
          </div>
        `}
      </div>
    `;
  }

  function isDirectResponsesMode() {
    // 自定义第三方 + apiFormat=responses → Codex.app 直连上游(direct 模式),
    // 模型透传给上游,代理不做 alias 翻译 → default mapping 可空。
    return (
      formApiFormatValue === "responses" && !!selectedPreset?.allowApiFormatSelection
    );
  }

  function customMappingRowMarkup(rowKey, index) {
    const customLabel = providerFormCustomLabels[rowKey] || "";
    const currentProviderModel = providerFormMappings[rowKey] || "";
    return `
      <article class="form-mapping-row">
        <div class="form-mapping-left">
          <label class="form-label visually-hidden" for="providerMappingSlot-${index}">${t("providersAdd.claudeModel")}</label>
          <div class="mapping-select-wrap">
            <span class="mapping-icon default"><i class="bi bi-pencil"></i></span>
            <input
              class="form-control form-select custom-model-name-input"
              id="providerMappingSlot-${index}"
              data-custom-model-label="${escapeHtml(rowKey)}"
              value="${escapeHtml(customLabel)}"
              placeholder="${escapeHtml(t("providersAdd.customModelPlaceholder"))}"
            >
          </div>
        </div>
        <div class="form-mapping-right">
          <label class="form-label visually-hidden" for="providerMappingValue-${index}">${t("providersAdd.providerModel")}</label>
          <div class="provider-model-input-wrap ${openProviderModelMenuKey === rowKey ? "open" : ""}">
            ${providerModelValueInputMarkup(rowKey, index, currentProviderModel, false)}
            <button
              class="provider-model-trigger"
              type="button"
              data-action="toggle-provider-model-menu"
              data-row-key="${escapeHtml(rowKey)}"
              ${providerAvailableModels.length ? "" : "disabled"}
              aria-haspopup="listbox"
              aria-expanded="${providerAvailableModels.length && openProviderModelMenuKey === rowKey ? "true" : "false"}"
              aria-label="${escapeHtml(t("providersAdd.providerModel"))}"
            >
              <i class="bi bi-chevron-down" aria-hidden="true"></i>
            </button>
            ${providerAvailableModels.length ? `
              <div class="mapping-slot-menu provider-model-menu" role="listbox" aria-labelledby="providerMappingValue-${index}">
                ${providerModelOptionsMarkup(currentProviderModel)}
              </div>
            ` : ""}
          </div>
        </div>
        <div class="form-mapping-actions">
          <button class="btn btn-outline-secondary btn-sm mapping-remove-button" type="button" data-action="remove-provider-model-row" data-row-index="${index}" aria-label="${escapeHtml(t("providersAdd.removeMapping"))}">${escapeHtml(t("providersAdd.removeMapping"))}</button>
        </div>
      </article>
    `;
  }

  function formMappingMarkup() {
    return providerFormRows.map((rowKey, index) => {
      if (isCustomMappingRow(rowKey)) {
        return customMappingRowMarkup(rowKey, index);
      }
      const slot = slotByKey(rowKey);
      // direct 模式不需要 model alias 映射,default 字段也可空;其他场景仍 required
      const isRequired = rowKey === "default" && !isDirectResponsesMode();
      const currentProviderModel = providerFormMappings[rowKey] || "";
      return `
        <article class="form-mapping-row">
          <div class="form-mapping-left">
            <label class="form-label visually-hidden" for="providerMappingSlot-${index}">${t("providersAdd.claudeModel")}</label>
            <div class="mapping-select-wrap">
              <span class="mapping-icon ${slot.iconClass}"><i class="bi ${slot.icon}"></i></span>
              ${slotMenuMarkup(rowKey, index)}
            </div>
          </div>
          <div class="form-mapping-right">
            <label class="form-label visually-hidden" for="providerMappingValue-${index}">${t("providersAdd.providerModel")}</label>
            <div class="provider-model-input-wrap ${openProviderModelMenuKey === rowKey ? "open" : ""}">
              ${providerModelValueInputMarkup(rowKey, index, currentProviderModel, isRequired)}
              <button
                class="provider-model-trigger"
                type="button"
                data-action="toggle-provider-model-menu"
                data-row-key="${escapeHtml(rowKey)}"
                ${providerAvailableModels.length ? "" : "disabled"}
                aria-haspopup="listbox"
                aria-expanded="${providerAvailableModels.length && openProviderModelMenuKey === rowKey ? "true" : "false"}"
                aria-label="${escapeHtml(t("providersAdd.providerModel"))}"
              >
                <i class="bi bi-chevron-down" aria-hidden="true"></i>
              </button>
              ${providerAvailableModels.length ? `
                <div class="mapping-slot-menu provider-model-menu" role="listbox" aria-labelledby="providerMappingValue-${index}">
                  ${providerModelOptionsMarkup(currentProviderModel)}
                </div>
              ` : ""}
            </div>
          </div>
          <div class="form-mapping-actions">
            ${isRequired
              ? '<span class="mapping-remove-placeholder" aria-hidden="true"></span>'
              : `<button class="btn btn-outline-secondary btn-sm mapping-remove-button" type="button" data-action="remove-provider-model-row" data-row-index="${index}" aria-label="${escapeHtml(t("providersAdd.removeMapping"))}">${escapeHtml(t("providersAdd.removeMapping"))}</button>`}
          </div>
        </article>
      `;
    }).join("");
  }

  function renderProviderMappings() {
    const stack = $("#providerMappingStack");
    if (!stack) return;
    if (openProviderSlotMenuIndex !== null && !providerFormRows[openProviderSlotMenuIndex]) {
      openProviderSlotMenuIndex = null;
    }
    if (openProviderModelMenuKey !== null && !providerFormRows.includes(openProviderModelMenuKey)) {
      openProviderModelMenuKey = null;
    }
    stack.innerHTML = `
      <div class="provider-mapping-card">
        <div class="provider-mapping-list">
          ${formMappingMarkup()}
        </div>
        <div class="provider-mapping-footer">
          <button class="btn btn-outline-primary btn-sm" type="button" data-action="add-provider-model-row">
            <i class="bi bi-plus-lg"></i><span>${escapeHtml(t("providersAdd.addMapping"))}</span>
          </button>
        </div>
      </div>
    `;
  }

  function setProviderMappings(mappings = {}, options = {}) {
    providerFormMappings = normalizeMappings(mappings);
    providerFormRows = formMappingRowsFromMappings(providerFormMappings);
    if (Array.isArray(options.availableModels)) {
      providerAvailableModels = options.availableModels.slice();
    }
    openProviderSlotMenuIndex = null;
    openProviderModelMenuKey = null;
    renderProviderMappings();
  }

  // [MOC-69] 编辑 antigravity provider 时静默拉一次模型列表,让映射选框立即显示 displayName
  // (否则要手点「获取模型」才有反查表)。失败 / 离线 → 保持现状(选框显示 raw id),
  // 不报错不清空(非破坏性 fallback)。antigravity 上游 list 走 OAuth token,不依赖 apiKey。
  async function autoFetchModelsForDisplay() {
    try {
      const payload = providerPayloadFromForm(false);
      if (editingProviderId && !payload.apiKey) {
        try {
          const secret = await CCApi.getProviderSecret(editingProviderId);
          if (secret.apiKey) payload.apiKey = secret.apiKey;
        } catch (e) { /* ignore — antigravity OAuth 不依赖 apiKey */ }
      }
      const result = await CCApi.fetchProviderModelsPayload(payload);
      const models = Array.isArray(result.models) ? result.models.slice() : [];
      if (models.length) {
        setProviderMappings(providerFormMappings, { availableModels: models });
      }
    } catch (e) {
      // 非破坏性 fallback:保持选框 raw id 显示,不弹 toast 不打扰用户;但留 devtools
      // 面包屑便于诊断(显式「获取模型」按钮才 surface 错误给用户)。
      console.warn("[autoFetchModelsForDisplay] displayName 预取失败,保持 raw id 显示:", e);
    }
  }

  function collectProviderMappingsWithCustom() {
    const base = normalizeMappings(providerFormMappings);
    // convert _custom_N internal keys to the user-typed model name
    const result = {};
    for (const [key, value] of Object.entries(base)) {
      if (isCustomMappingRow(key)) {
        const label = (providerFormCustomLabels[key] || "").trim();
        if (label && String(value || "").trim()) {
          result[label] = String(value).trim();
        }
      } else {
        result[key] = value;
      }
    }
    return result;
  }

  function updateProviderModelInput(slotKey, value) {
    providerFormMappings[slotKey] = value.trim();
  }

  function moveProviderMappingRow(index, nextKey) {
    const prevKey = providerFormRows[index];
    if (!nextKey || prevKey === nextKey) return;
    const currentValue = providerFormMappings[prevKey] || "";
    providerFormRows[index] = nextKey;
    if (!providerFormMappings[nextKey]) {
      providerFormMappings[nextKey] = currentValue;
    }
    if (prevKey !== "default") {
      providerFormMappings[prevKey] = "";
    }
    openProviderSlotMenuIndex = null;
    renderProviderMappings();
  }

  function addProviderMappingRow() {
    const remaining = providerFormModelSlots
      .map((slot) => slot.key)
      .find((key) => !providerFormRows.includes(key));
    if (remaining) {
      providerFormRows = [...providerFormRows, remaining];
    } else {
      // all predefined slots used — add a custom row
      const customKey = `_custom_${customRowCounter++}`;
      providerFormRows = [...providerFormRows, customKey];
      providerFormMappings[customKey] = "";
      providerFormCustomLabels[customKey] = "";
    }
    openProviderSlotMenuIndex = null;
    openProviderModelMenuKey = null;
    renderProviderMappings();
  }

  function removeProviderMappingRow(index) {
    const key = providerFormRows[index];
    if (!key || key === "default") return;
    providerFormRows = providerFormRows.filter((_, rowIndex) => rowIndex !== index);
    if (isCustomMappingRow(key)) {
      delete providerFormMappings[key];
      delete providerFormCustomLabels[key];
    } else {
      providerFormMappings[key] = "";
    }
    openProviderSlotMenuIndex = null;
    if (openProviderModelMenuKey === key) openProviderModelMenuKey = null;
    renderProviderMappings();
  }

  function toggleProviderSlotMenu(index) {
    openProviderSlotMenuIndex = openProviderSlotMenuIndex === index ? null : index;
    renderProviderMappings();
  }

  function closeProviderSlotMenu() {
    if (openProviderSlotMenuIndex === null) return;
    openProviderSlotMenuIndex = null;
    renderProviderMappings();
  }

  function toggleProviderModelMenu(rowKey) {
    openProviderModelMenuKey = openProviderModelMenuKey === rowKey ? null : rowKey;
    renderProviderMappings();
  }

  function closeProviderModelMenu() {
    if (openProviderModelMenuKey === null) return;
    openProviderModelMenuKey = null;
    renderProviderMappings();
  }

  function setAuthSchemeValue(value) {
    const input = $("#providerAuth");
    if (!input) return;
    input.value = providerAuthSchemes.includes(value) ? value : "bearer";
  }

  function renderPresetOptions(preset = null, mappings = null) {
    const container = $("#providerPresetOptions");
    if (!container) return;
    const modelOptions = preset?.modelOptions && typeof preset.modelOptions === "object"
      ? Object.entries(preset.modelOptions)
      : [];
    const requestOptionPresets = preset?.requestOptionPresets && typeof preset.requestOptionPresets === "object"
      ? Object.entries(preset.requestOptionPresets)
      : [];
    const options = [...modelOptions, ...requestOptionPresets];
    const notices = Array.isArray(preset?.notices) ? preset.notices.filter((n) => n && n.text) : [];

    if (!options.length && !notices.length) {
      container.hidden = true;
      container.innerHTML = "";
      return;
    }

    const currentMappings = normalizeMappings(mappings || collectProviderMappings());
    container.hidden = false;

    const noticeIcon = (type) => {
      if (type === "warning") return "bi-exclamation-triangle-fill";
      if (type === "success") return "bi-check-circle-fill";
      return "bi-info-circle-fill";
    };
    const noticesHtml = notices.map((n) => `
      <div class="preset-notice preset-notice-${escapeHtml(n.type || "info")}" role="note">
        <i class="bi ${noticeIcon(n.type)}" aria-hidden="true"></i>
        <span>${escapeHtml(n.text)}</span>
      </div>
    `).join("");

    const optionsHtml = options.map(([id, option]) => `
      <label class="preset-option-item">
        <input class="form-check-input" type="checkbox" data-preset-model-option="${escapeHtml(id)}" ${optionEnabled(option, currentMappings) ? "checked" : ""}>
        <span>
          <strong>${escapeHtml(option.label || id)}</strong>
          <small>${escapeHtml(option.description || "")}</small>
        </span>
      </label>
    `).join("");

    container.innerHTML = noticesHtml + optionsHtml;
  }

  function applyPresetModelOption(optionId, enabled) {
    const option = selectedPreset?.modelOptions?.[optionId] || selectedPreset?.requestOptionPresets?.[optionId];
    if (!option) return;
    const hasModels = option.models && typeof option.models === "object";
    const hasCapabilities = option.modelCapabilities && typeof option.modelCapabilities === "object";
    const mappings = option.models
      ? (enabled ? option.models : selectedPreset.models || emptyMappings())
      : collectProviderMappings();
    if (hasModels) {
      setProviderMappings(mappings);
    }
    if (hasCapabilities) {
      formModelCapabilities = enabled
        ? mergeCapabilities(formModelCapabilities || selectedPreset.modelCapabilities || {}, option.modelCapabilities)
        : clearCapabilities(formModelCapabilities, option.modelCapabilities);
    } else if (hasModels) {
      formModelCapabilities = normalizeCapabilities(enabled
        ? option.modelCapabilities || selectedPreset.modelCapabilities || {}
        : selectedPreset.modelCapabilities || {});
    }
    if (option.requestOptions) {
      formRequestOptions = enabled
        ? mergeRequestOptions(selectedPreset.requestOptions || {}, option.requestOptions)
        : clearRequestOptions(formRequestOptions, option.requestOptions);
    }
    renderPresetOptions(selectedPreset, mappings);
    showToast(`${option.label || optionId} ${t("providersAdd.optionApplied")}`);
  }

  function collectProviderMappings() {
    return collectProviderMappingsWithCustom();
  }

  function providerPayloadFromForm(includeModels = true) {
    const apiKey = $("#providerApiKey").value.trim();
    const mappings = includeModels ? collectProviderMappings() : null;
    // Web Search 开关:仅当 row 显示(preset.supportsWebSearch === true)时,从
    // checkbox 收集 web_search_enabled 写入 formRequestOptions;否则保留 form
    // 状态(preset 不支持时 normalize 阶段会自动剥)。
    const webSearchRow = $("#providerWebSearchRow");
    const webSearchToggle = $("#providerWebSearchEnabled");
    if (webSearchRow && webSearchToggle && !webSearchRow.hidden) {
      formRequestOptions = {
        ...formRequestOptions,
        web_search_enabled: webSearchToggle.checked,
      };
    }
    const payload = {
      name: $("#providerName").value.trim(),
      baseUrl: $("#providerBaseUrl").value.trim(),
      authScheme: $("#providerAuth").value,
      apiFormat: formApiFormatValue,
      extraHeaders: selectedPreset?.extraHeaders || {},
      modelCapabilities: mappings ? capabilitiesForCurrentMappings(mappings) : normalizeCapabilities(formModelCapabilities),
      requestOptions: normalizeRequestOptions(formRequestOptions),
    };
    if (apiKey) {
      payload.apiKey = apiKey;
    }
    if (includeModels) {
      payload.models = mappings;
    }
    // R1 PR-7:apiFormat=grok_web 时打包 extra.grokWeb(cookies + statsigId)。
    // Provider 后端 schema 用 `#[serde(flatten)] extra`,任何不在已知字段的 key
    // 自动收进 provider.extra,所以前端 payload 顶层加 `grokWeb` 就 work。
    const grokWebPayload = collectGrokWebPayload();
    if (grokWebPayload) {
      payload.grokWeb = grokWebPayload;
    }
    return payload;
  }

  function findDocsUrlForProvider(provider) {
    if (!presetCache.length) return null;
    const stripSlash = (s) => String(s || "").replace(/\/+$/, "");
    const target = stripSlash(provider.baseUrl);
    const providerId = String(provider.id || "");
    for (const preset of presetCache) {
      const candidates = new Set([stripSlash(preset.baseUrl)]);
      for (const opt of (preset.baseUrlOptions || [])) {
        if (opt && opt.value) candidates.add(stripSlash(opt.value));
      }
      if (preset.id === providerId || (target && candidates.has(target))) {
        return preset.docsUrl || null;
      }
    }
    return null;
  }

  function providerCardMarkup(provider) {
    const mapping = [
      provider.mappings.default,
      provider.mappings.gpt_5_5,
      provider.mappings.gpt_5_4,
      provider.mappings.gpt_5_4_mini,
      provider.mappings.gpt_5_3_codex,
      provider.mappings.gpt_5_2,
    ]
      .filter(Boolean)
      .slice(0, 2)
      .join(" / ");
    const providerId = escapeHtml(provider.id);
    const providerName = escapeHtml(provider.name);
    const providerUrl = escapeHtml(provider.baseUrl);
    const mappingText = escapeHtml(mapping || provider.apiFormat);
    const docsUrl = findDocsUrlForProvider(provider);
    const baseUrlMarkup = docsUrl
      ? `<a class="truncate baseurl-docs-link" href="#" data-action="open-docs" data-docs-url="${escapeHtml(docsUrl)}" data-provider-name="${providerName}" title="${t("providers.openDocsHint")}">${providerUrl}<i class="bi bi-box-arrow-up-right baseurl-docs-icon"></i></a>`
      : `<span class="truncate">${providerUrl}</span>`;
    return `
      <article class="provider-switch-card ${provider.default ? "active" : ""}" draggable="true" data-provider-id="${providerId}">
        <span class="drag-handle"><i class="bi bi-grip-vertical"></i></span>
        <span class="provider-logo">${iconMarkup(provider)}</span>
        <span class="provider-main">
          <strong>${providerName}</strong>
          ${baseUrlMarkup}
        </span>
        <span class="provider-meta truncate">${mappingText}</span>
        <span class="provider-actions">
          ${provider.default ? `<span class="active-indicator" role="status" aria-label="${escapeHtml(t("status.active"))}"><i class="bi bi-broadcast" aria-hidden="true"></i><span>${escapeHtml(t("status.active"))}</span></span>` : ""}
          <button class="btn btn-primary compact-enable" type="button" data-action="set-default" data-id="${providerId}">
            <i class="bi bi-play-fill"></i><span>${t("providers.enable")}</span>
          </button>
          <button class="icon-action" type="button" data-action="test-provider" data-id="${providerId}" title="${t("providers.testSpeed")}" aria-label="${t("providers.testSpeed")}"><i class="bi bi-lightning-charge"></i></button>
          <button class="icon-action" type="button" data-action="query-usage" data-id="${providerId}" title="${t("providers.usage")}" aria-label="${t("providers.usage")}"><i class="bi bi-wallet2"></i></button>
          <button class="icon-action" type="button" data-action="edit-provider" data-id="${providerId}" title="${t("common.edit")}" aria-label="${t("common.edit")}"><i class="bi bi-pencil-square"></i></button>
          <button class="icon-action" type="button" data-action="copy-url" data-url="${providerUrl}" title="${t("common.copy")}" aria-label="${t("common.copy")}"><i class="bi bi-copy"></i></button>
          <a class="icon-action" href="#proxy" title="${t("nav.proxy")}" aria-label="${t("nav.proxy")}"><i class="bi bi-terminal"></i></a>
          <button class="icon-action danger" type="button" data-action="delete-provider" data-id="${providerId}" title="${t("common.delete")}" aria-label="${t("common.delete")}"><i class="bi bi-trash"></i></button>
        </span>
        <span class="provider-feedback">
          <span class="speed-result inline" data-speed-for="${providerId}"></span>
          <span class="usage-result inline" data-usage-for="${providerId}"></span>
        </span>
      </article>
    `;
  }

  function providerPresetCardMarkup(preset, added = false) {
    const presetId = escapeHtml(preset.id);
    return `
      <button class="provider-switch-card preset-card ${added ? "added" : ""}" type="button" data-action="new-from-preset" data-preset="${presetId}" ${added ? "disabled" : ""}>
        <span class="drag-handle preset-plus"><i class="bi ${added ? "bi-check2" : "bi-plus-lg"}"></i></span>
        <span class="provider-logo">${iconMarkup(preset)}</span>
        <span class="provider-main"><strong>${escapeHtml(preset.name)}</strong><span class="truncate">${escapeHtml(preset.baseUrl)}</span></span>
        <span class="provider-meta">${escapeHtml(t(`apiFormatDisplay.${normalizeApiFormat(preset.apiFormat).key}.name`))}</span>
        <span class="provider-actions"><span class="compact-enable ghost"><i class="bi ${added ? "bi-check2" : "bi-plus-lg"}"></i><span>${added ? t("providers.added") : t("providers.add")}</span></span></span>
      </button>
    `;
  }

  function dashboardPresetSectionMarkup(providers, presets) {
    const available = presets.filter((preset) => !presetExists(preset, providers));
    if (!available.length) return "";
    return `
      <section class="dashboard-preset-section" aria-label="${escapeHtml(t("dashboard.availablePresets"))}">
        <div class="section-title-row compact">
          <div>
            <h2>${escapeHtml(t("dashboard.availablePresets"))}</h2>
            <p>${escapeHtml(t("dashboard.availablePresetsHint"))}</p>
          </div>
        </div>
        <div class="provider-preset-grid">
          ${available.map((preset) => providerPresetCardMarkup(preset)).join("")}
        </div>
      </section>
    `;
  }

  function getDragAfterElement(container, y) {
    const items = [...container.querySelectorAll("[data-provider-id]:not(.dragging)")];
    return items.reduce((closest, child) => {
      const box = child.getBoundingClientRect();
      const offset = y - box.top - box.height / 2;
      if (offset < 0 && offset > closest.offset) return { offset, element: child };
      return closest;
    }, { offset: Number.NEGATIVE_INFINITY, element: null }).element;
  }

  function enableProviderReorder(listEl) {
    if (!listEl || listEl.dataset.reorderBound === "1") return;
    listEl.dataset.reorderBound = "1";

    listEl.addEventListener("dragstart", (event) => {
      const card = event.target.closest("[data-provider-id]");
      if (!card) return;
      card.classList.add("dragging");
      event.dataTransfer.effectAllowed = "move";
      event.dataTransfer.setData("text/plain", card.dataset.providerId);
    });

    listEl.addEventListener("dragover", (event) => {
      const dragging = listEl.querySelector(".dragging");
      if (!dragging) return;
      event.preventDefault();
      const afterElement = getDragAfterElement(listEl, event.clientY);
      if (afterElement) {
        listEl.insertBefore(dragging, afterElement);
      } else {
        listEl.appendChild(dragging);
      }
    });

    listEl.addEventListener("drop", async (event) => {
      const dragging = listEl.querySelector(".dragging");
      if (!dragging) return;
      event.preventDefault();
      dragging.classList.remove("dragging");
      const providerIds = $all("[data-provider-id]", listEl).map((item) => item.dataset.providerId);
      try {
        await CCApi.reorderProviders(providerIds);
        showToast(t("toast.providersReordered"));
        await renderProviders();
        if (routeFromHash() === "dashboard") await renderDashboard();
      } catch (error) {
        console.error(error);
        if (routeFromHash() === "dashboard") {
          await renderDashboard();
        } else {
          await renderProviders();
        }
        showToast(error.message || t("toast.requestFailed"));
      }
    });

    listEl.addEventListener("dragend", (event) => {
      event.target.closest("[data-provider-id]")?.classList.remove("dragging");
    });
  }

  async function renderProviderCards(targetSelector, options = {}) {
    const target = $(targetSelector);
    if (!target) return;
    const providers = await CCApi.getProviders();
    if (!presetCache.length) presetCache = await CCApi.getPresets();
    const providerList = providers.length
      ? `<div class="provider-configured-list" data-provider-list>${providers.map(providerCardMarkup).join("")}</div>`
      : "";
    if (!providers.length) {
      target.innerHTML = `<div class="provider-preset-grid">${visiblePresets().map((preset) => providerPresetCardMarkup(preset)).join("")}</div>`;
      return;
    }
    if (options.includePresets) {
      target.innerHTML = `${providerList}${dashboardPresetSectionMarkup(providers, visiblePresets())}`;
    } else {
      target.innerHTML = providerList;
    }
    enableProviderReorder($("[data-provider-list]", target));
  }


  // ── Plugin Unlock 状态刷新 ──
  async function refreshPluginUnlockStatus() {
    try {
      const unlock = await CCApi.pluginUnlock.status();
      const icon = $("#pluginUnlockIcon");
      const statusText = $("#pluginUnlockStatus");
      const actions = $("#pluginUnlockActions");
      if (!icon || !statusText) return;

      icon.classList.remove("muted", "success", "warning", "danger");
      const statusMap = {
        disconnected: { icon: "bi-lock", class: "muted", text: t("pluginUnlock.disconnected") || "未运行" },
        connecting: { icon: "bi-arrow-repeat", class: "warning", text: t("pluginUnlock.connecting") || "连接中..." },
        connected: { icon: "bi-plug", class: "warning", text: t("pluginUnlock.connected") || "已连接" },
        injected: { icon: "bi-unlock", class: "success", text: t("pluginUnlock.injected") || "已解锁" },
        failed: { icon: "bi-exclamation-triangle", class: "danger", text: unlock.message || "失败" },
      };
      const s = statusMap[unlock.status] || statusMap.disconnected;
      icon.innerHTML = `<i class="bi ${s.icon}"></i>`;
      icon.classList.add(s.class);
      statusText.classList.toggle("muted-text", s.class === "muted");
      statusText.textContent = s.text;
      if (actions) actions.style.display = unlock.status === "injected" || unlock.status === "connected" ? "block" : "none";

      // 同步设置页"运行时状态"提示。dashboard 卡片跟 settings 页用同一份
      // /api/desktop/plugin-unlock/status 数据,文案前缀 "运行时状态：" 标识
      // 这是 daemon 当前态(跟用户配置区分开)。
      const runtimeNote = $("#pluginUnlockRuntimeStatus");
      if (runtimeNote) {
        const prefix = t("settings.pluginUnlockRuntimeStatusPrefix") || "运行时状态：";
        runtimeNote.textContent = `${prefix}${s.text}`;
      }
    } catch (e) {
      console.log("[PluginUnlock] status refresh failed:", e);
    }
  }

  async function renderDashboard() {
    // **#249 fix**:getStatus / getActivities 分别 try-catch,任一失败
    // 仍渲染其余卡片,避免单个 API 崩溃 → 整个 dashboard 白屏。
    let status;
    try {
      status = await CCApi.getStatus();
    } catch (err) {
      console.error("[renderDashboard] getStatus failed:", err);
      status = {};
    }
    let activities = [];
    try {
      activities = await CCApi.getActivities();
    } catch (err) {
      console.error("[renderDashboard] getActivities failed:", err);
    }
    const health = status.desktopHealth || {};
    const desktopReady = status.desktopConfigured && !health.needsApply;
    try {
      await renderProviderCards("#dashboardProviderCards", { includePresets: true });
    } catch (err) {
      console.error("[renderDashboard] renderProviderCards failed:", err);
    }
    const desktopIcon = $("#dashboardDesktopIcon");
    desktopIcon.classList.toggle("muted", !desktopReady);
    desktopIcon.innerHTML = `<i class="bi ${desktopReady ? "bi-check-lg" : "bi-exclamation-lg"}"></i>`;
    const desktopStatus = $("#dashboardDesktopStatus");
    desktopStatus.classList.toggle("muted-text", !desktopReady);
    desktopStatus.textContent = health.needsApply
      ? t("status.needsApply")
      : status.desktopConfigured ? t("status.configured") : t("status.notConfigured");
    renderDesktopHealthWarning("#dashboardDesktopWarning", health);
    // ── proxy 状态卡片:图标颜色 + 文字颜色跟随 running 状态 ──
    const proxyIcon = $("#dashboardProxyIcon");
    if (proxyIcon) {
      proxyIcon.classList.toggle("success", !!status.proxyRunning);
      proxyIcon.classList.toggle("muted", !status.proxyRunning);
      proxyIcon.innerHTML = status.proxyRunning
        ? '<i class="bi bi-hdd-network"></i><i class="bi bi-activity badge-icon"></i>'
        : '<i class="bi bi-hdd-network"></i>';
    }
    const proxyStatusEl = $("#dashboardProxyStatus");
    proxyStatusEl.textContent = status.proxyRunning ? `${t("status.running")} :${status.proxyPort}` : t("status.stopped");
    proxyStatusEl.classList.toggle("muted-text", !status.proxyRunning);
    $("#dashboardProviderName").textContent = status.activeProvider?.name ?? "—";
    // Plugin Unlock 状态刷新
    refreshPluginUnlockStatus();
    // MOC-32 PR-2b: silently dropped Responses tool types
    refreshDroppedToolsWarning();
    $("#activityList").innerHTML = activities.map((item) => (
      `<div class="activity-row"><time>${escapeHtml(item.time)}</time><span>${escapeHtml(item.text)}</span></div>`
    )).join("");
    try {
      await refreshUpdateBadge();
    } catch (err) {
      console.error("[renderDashboard] refreshUpdateBadge failed:", err);
    }
  }

  /// MOC-32 PR-2b: query /api/diagnostic/dropped-tools, total>0 时弹 warning
  /// 让 user / maintainer 看到 transfer adapter 静默 drop 的 Responses API
  /// 工具类型(防 MOC-32 类静默 bug 再藏 N 月)。total=0 隐藏 warning(0 是
  /// healthy 状态不要刷屏)。
  async function refreshDroppedToolsWarning() {
    const warning = $("#dashboardDroppedToolsWarning");
    if (!warning) return;
    try {
      const data = await CCApi.getDroppedTools();
      const total = Number(data?.total ?? 0);
      if (total <= 0) {
        warning.hidden = true;
        return;
      }
      const byType = data.by_type || {};
      const types = Object.keys(byType).sort();
      const summary = $("#dashboardDroppedToolsSummary");
      if (summary) {
        summary.textContent = ` (${total} ${t("dashboard.droppedToolsCalls") || "次"} / ${types.length} ${t("dashboard.droppedToolsTypes") || "种"})`;
      }
      const list = $("#dashboardDroppedToolsList");
      if (list) {
        list.innerHTML = types
          .map((tt) => `<li><code>${escapeHtml(tt)}</code> × ${Number(byType[tt])}</li>`)
          .join("");
      }
      warning.hidden = false;
    } catch (_) {
      warning.hidden = true;
    }
  }

  // MOC-91:展示用的 preset 列表 —— `showGrayPresets=false` 时滤掉灰色(`gray:true`)preset。
  // **只过滤展示**,不动 presetCache 本身(它还要供已配置 provider 反查 preset:logo /
  // notices / 默认值 —— 见 presetMatchesProvider / showProviderForm),否则已添加的灰色
  // provider 会匹配不到自己的 preset。
  function visiblePresets(list) {
    const src = list || presetCache;
    return showGrayPresets ? src : src.filter((p) => p && p.gray !== true);
  }

  async function renderPresets() {
    presetCache = await CCApi.getPresets();
    $("#presetList").innerHTML = visiblePresets().map((preset) => {
      const active = selectedPreset?.id === preset.id;
      const isCustom = preset.id === "custom-third-party";
      const nameMarkup = isCustom
        ? `<strong data-i18n="providersAdd.customThirdPartyName">${escapeHtml(preset.name)}</strong>`
        : `<strong>${escapeHtml(preset.name)}</strong>`;
      const subText = isCustom
        ? `<span data-i18n="providersAdd.customThirdPartyHint">${escapeHtml(preset.baseUrlHint || "")}</span>`
        : `<span>${escapeHtml(preset.baseUrl)}</span>`;
      return `
      <button class="preset-item ${active ? "active" : ""}" type="button" data-preset="${escapeHtml(preset.id)}" aria-pressed="${active ? "true" : "false"}">
        <span class="preset-logo">${iconMarkup(preset)}</span>
        <span>${nameMarkup}${subText}</span>
        <i class="bi ${active ? "bi-check2" : "bi-chevron-right"}"></i>
      </button>
    `;
    }).join("");
  }

  function setProviderFormMode(titleKey) {
    const title = $("#page-providers-add .page-title h1");
    if (title) title.textContent = t(titleKey);
    const submit = $("#providerSaveOnly");
    if (submit) submit.textContent = t("common.saveOnly");
    const result = $("#formSpeedResult");
    if (result) {
      result.textContent = "";
      result.className = "speed-result";
    }
    const modelResult = $("#providerModelFetchResult");
    if (modelResult) modelResult.textContent = "";
  }

  function setApiKeyInputState(hasSavedKey = false, savedKey = "") {
    const input = $("#providerApiKey");
    const label = $("label[for='providerApiKey']");
    if (!input) return;
    input.type = "password";
    input.value = savedKey || "";
    input.required = !hasSavedKey && !savedKey;
    input.placeholder = (hasSavedKey || savedKey) ? t("providers.keySavedPlaceholder") : t("providers.keyPlaceholder");
    const toggle = $("[data-action='toggle-key']");
    if (toggle) toggle.innerHTML = '<i class="bi bi-eye"></i>';
    if (label) label.classList.toggle("required", input.required);
  }

  /// i18n template fill — 替代 ad-hoc `t(key).replace("{var}",val)`。fallback 行为:
  /// 1) t() 返 key 字符串(missing key)→ console.warn + return key (silent-failure
  ///    M1 修)。2) replace 后仍残留 `{var}` 占位 → console.warn 让 i18n 不全暴露
  function tFmt(key, vars = {}) {
    const tmpl = t(key);
    if (tmpl === key) {
      console.warn(`[i18n] missing key: ${key}`);
    }
    let out = tmpl;
    for (const [k, v] of Object.entries(vars)) {
      out = out.split(`{${k}}`).join(String(v ?? ""));
    }
    if (/\{[a-zA-Z_]+\}/.test(out)) {
      console.warn(`[i18n] unsubstituted placeholder in "${key}": ${out}`);
    }
    return out;
  }

  /// Cloud Code Assist OAuth row 切换:apiFormat 是 OAuth 类(gemini_cli_oauth /
  /// antigravity_oauth)时隐藏 apiKey input,显示 OAuth login button + status
  /// widget;其他 apiFormat 隐藏 OAuth row。调用时机:form 加载 / apiFormat
  /// select 切换。
  /// 内部 update activeOauthConfig 让后续 refresh/login/logout 走对的 provider
  /// R1 PR-7:apiFormat=grok_web 时显示 grok web cookie 输入 row,隐藏 apiKey
  /// 输入(grok_web 不用 apiKey,用 extra.grokWeb.{cookies, statsigId})。
  /// 与 setOauthRowState 互斥 — 调用方应先确保 apiFormat 解析后路由到对的一个。
  function setGrokWebRowState(apiFormat) {
    const row = $("#providerGrokWebRow");
    const apiKeyRow = $("#providerApiKeyRow");
    const apiKeyInput = $("#providerApiKey");
    const { canonical } = normalizeApiFormat(apiFormat);
    const isGrokWeb = canonical === "grok_web";
    if (row) row.hidden = !isGrokWeb;
    if (apiKeyRow) {
      // grok_web 隐藏 apiKey input;非 grok_web 显示(避免与 OAuth 状态冲突
      // —— setOauthRowState 自己控制 isOauth case 的 apiKey 可见性,所以
      // 我们**只在切换到/离开 grok_web 时操作**,其它情况让 OAuth/默认逻辑接管)
      if (isGrokWeb) {
        apiKeyRow.hidden = true;
      }
    }
    if (apiKeyInput && isGrokWeb) {
      // required 兜底:grok_web 不需要 apiKey 必填,否则浏览器 form validation 会
      // 卡住 submit(即使 input 被 hidden div 包着,required 仍校验)。
      //
      // **chatgpt-codex P1 修(2026-05-12)**:**不**在非 grok_web 时无条件设
      // `required = true` —— OAuth modes(gemini_cli_oauth / antigravity_oauth)
      // 由 setOauthRowState 自己管理 required(它走 dataset.origRequired
      // 保存/恢复机制,1308-1313 行),无条件覆盖会让 OAuth 模式 hidden apiKey
      // 仍 required → form submit 静默被浏览器拒。chain 调用顺序是
      // setOauthRowState → setGrokWebRowState,我们离开 grok_web 时**不动**,
      // 让上游 setter 的决定生效。
      apiKeyInput.required = false;
    }
    if (isGrokWeb) {
      const authEl = $("#providerAuth");
      if (authEl) authEl.value = "grok_cookie";
    }
  }

  /// 从 grok_web form input 收集 grokWeb extra payload(用于 POST 时打包到
  /// provider.extra.grokWeb)。
  ///
  /// Plan A:仅 sso 必填;sso-rw / cf_clearance / statsigId / userAgent 都 optional
  /// (后端 auth.rs 缺失时分别复用 sso / 跳过 segment / 动态生成 / 用默认 UA)。
  ///
  /// 返回 null 表示不是 grok_web 模式或 input 全空(编辑模式留空 = 保留现值)。
  function collectGrokWebPayload() {
    const row = $("#providerGrokWebRow");
    if (!row || row.hidden) return null;
    const sso = $("#grokWebSso")?.value.trim() || "";
    const ssoRw = $("#grokWebSsoRw")?.value.trim() || "";
    const cf = $("#grokWebCfClearance")?.value.trim() || "";
    const cookieString = $("#grokWebCookieString")?.value.trim() || "";
    const statsigId = $("#grokWebStatsigId")?.value.trim() || "";
    const userAgent = $("#grokWebUserAgent")?.value.trim() || "";
    if (!sso && !ssoRw && !cf && !cookieString && !statsigId && !userAgent)
      return null;
    const cookies = { sso };
    if (ssoRw) cookies["sso-rw"] = ssoRw;
    if (cf) cookies.cf_clearance = cf;
    if (cookieString) cookies.cookieString = cookieString;
    const payload = { cookies };
    if (statsigId) payload.statsigId = statsigId;
    if (userAgent) payload.userAgent = userAgent;
    return payload;
  }

  /// 编辑现有 provider 时初始化 grok_web form。
  ///
  /// 后端 public_provider 已把 grokWeb 字段 mask 出去,只保留 `hasGrokWeb: bool`
  /// (cookies + statsigId 是高敏感凭证,跟 apiKey 一样不回传前端)。所以这里:
  ///   - 清空 input 值
  ///   - hasGrokWeb=true 时给 placeholder 提示"已保存凭证,留空则保持不变"
  ///   - 用户若真要替换才填新值,save 时 collectGrokWebPayload 返回新对象;
  ///     若空白 save → payload 不带 grokWeb → 后端 update_provider 不动现值
  function fillGrokWebFormFromProvider(provider) {
    const hasGrokWeb = !!provider?.hasGrokWeb;
    const ids = [
      "grokWebSso",
      "grokWebSsoRw",
      "grokWebCfClearance",
      "grokWebCookieString",
      "grokWebStatsigId",
      "grokWebUserAgent",
    ];
    const savedPlaceholder = t("grokWeb.savedPlaceholder") || "已保存,留空则保持";
    for (const id of ids) {
      const el = $(`#${id}`);
      if (!el) continue;
      el.value = "";
      el.placeholder = hasGrokWeb ? savedPlaceholder : "";
    }
  }

  function setOauthRowState(apiFormat) {
    const oauthRow = $("#providerOauthRow");
    const apiKeyRow = $("#providerApiKeyRow");
    const apiKeyInput = $("#providerApiKey");
    const baseUrlRow = $("#providerBaseUrlRow");
    const baseUrlInput = $("#providerBaseUrl");
    const { canonical } = normalizeApiFormat(apiFormat);
    const config = OAUTH_PROVIDER_CONFIGS[canonical] || null;
    activeOauthConfig = config;
    const isOauth = !!config;
    if (oauthRow) oauthRow.hidden = !isOauth;
    if (apiKeyRow) apiKeyRow.hidden = isOauth;
    // OAuth 模式 baseUrl 由 preset 写死(cloudcode-pa.googleapis.com),
    // user 不需要看 / 改;切回非 OAuth 显示
    if (baseUrlRow) baseUrlRow.hidden = isOauth;
    // **silent-failure-hunter H3 修**:原 `required = !isOauth && req` 单调毁掉
    // required 字段(切 OAuth 后 required=false,切回 openai_chat 仍 false)。改用
    // dataset.origRequired 缓存,switch 回非 OAuth 时恢复
    if (apiKeyInput) {
      if (apiKeyInput.dataset.origRequired === undefined) {
        apiKeyInput.dataset.origRequired = apiKeyInput.required ? "1" : "0";
      }
      if (isOauth) {
        apiKeyInput.required = false;
      } else {
        apiKeyInput.required = apiKeyInput.dataset.origRequired === "1";
      }
    }
    // baseUrl 同样的 required cache 处理 — OAuth 时解 required 防表单提交卡住
    if (baseUrlInput) {
      if (baseUrlInput.dataset.origRequired === undefined) {
        baseUrlInput.dataset.origRequired = baseUrlInput.required ? "1" : "0";
      }
      baseUrlInput.required = isOauth ? false : baseUrlInput.dataset.origRequired === "1";
      // **silent-failure-hunter H2 修**:hide 行不等于清 value。OAuth 模式下
      // baseUrl 由 preset 写死(cloudcode-pa.googleapis.com),user 切换 preset
      // 时残留的旧 value 可能跟着 form submit 上去让 backend 用错 endpoint。
      // 强制锁定 value 防止 hidden field 数据漂移
      if (isOauth) {
        baseUrlInput.value = "https://cloudcode-pa.googleapis.com";
      }
    }
    if (isOauth && config) {
      // 切换 provider 时,把 OAuth row 内**全部**静态 i18n 节点的 data-i18n key
      // 重写到当前 provider namespace。语言切换时 i18n.applyTranslations 会读
      // dataset.i18n 拿对的本地化 — 缺写就 stale 显错 provider 文案。
      // refreshOauthStatusUi 异步,resolve 前 user 切语言会撞旧 key,所以这里
      // 4 个节点全部同步重写一次(label / loginBtn / logoutBtn / statusText)
      const k = (suffix) => `${config.i18nPrefix}.${suffix}`;
      const label = $("#providerOauthRow > label.form-label");
      if (label) {
        label.dataset.i18n = k("title");
        label.textContent = tFmt(k("title"));
      }
      const loginBtn = $("#oauthLoginBtn");
      if (loginBtn) {
        loginBtn.dataset.i18n = k("loginBtn");
        loginBtn.textContent = tFmt(k("loginBtn"));
      }
      const logoutBtn = $("#oauthLogoutBtn");
      if (logoutBtn) {
        logoutBtn.dataset.i18n = k("logoutBtn");
        logoutBtn.textContent = tFmt(k("logoutBtn"));
      }
      const statusEl = $("#oauthStatusText");
      if (statusEl) {
        statusEl.dataset.i18n = k("statusLoading");
        statusEl.textContent = tFmt(k("statusLoading"));
        statusEl.classList.remove("text-warning");
      }
      refreshOauthStatusUi().catch((e) => {
        console.error("refresh oauth status failed:", e);
      });
    }
  }

  /// 调 GET /api/<provider>-oauth/status 同步 UI 状态(已登录 / 未登录 / partial)。
  /// 错误路径完整:清旧 i18n key + 复位 button visibility + structured error message
  /// (silent-failure-hunter C1 修)。i18n key 前缀按 activeOauthConfig 切换,
  /// 让 gemini-cli vs antigravity 用各自文案 namespace。
  ///
  /// **race 安全**(2026-05-11 codex-connector P2):入口 snapshot `activeOauthConfig`,
  /// await 后**identity check** 才动 DOM。否则 user 在 status fetch 飞行中切 provider
  /// (eg gemini-cli ↔ antigravity 互切),旧 provider 的延迟 response 会覆盖
  /// 新 provider UI,显错登录状态。同 handleOauthLogin/Logout 的 race fix 模式
  async function refreshOauthStatusUi() {
    const statusEl = $("#oauthStatusText");
    const loginBtn = $("#oauthLoginBtn");
    const logoutBtn = $("#oauthLogoutBtn");
    if (!statusEl) return;
    const config = activeOauthConfig;
    if (!config) return; // OAuth row 隐藏中,不刷新
    const k = (suffix) => `${config.i18nPrefix}.${suffix}`;
    try {
      const status = await config.api.getStatus();
      // **post-await identity check**:status 拿回时 user 可能已切到别 provider /
      // 关 OAuth row,这条 response 是过时数据,不该污染新 UI 上下文
      if (activeOauthConfig !== config) {
        return;
      }
      if (!status.loggedIn) {
        statusEl.dataset.i18n = k("statusNotLoggedIn");
        statusEl.textContent = tFmt(k("statusNotLoggedIn"));
        statusEl.classList.remove("text-warning");
        if (logoutBtn) logoutBtn.hidden = true;
        if (loginBtn) {
          loginBtn.hidden = false;
          loginBtn.dataset.i18n = k("loginBtn");
          loginBtn.textContent = tFmt(k("loginBtn"));
        }
      } else {
        const expiresStr = status.expiresAt
          ? new Date(status.expiresAt).toLocaleString()
          : "?";
        const tmplKey = status.projectId
          ? k("statusLoggedIn")
          : k("statusLoggedInNoProject");
        statusEl.dataset.i18n = tmplKey;
        statusEl.textContent = tFmt(tmplKey, {
          email: status.email || "?",
          projectId: status.projectId || "?",
          expiresAt: expiresStr,
        });
        // partial state(无 projectId)加 visual cue(M3 修)
        statusEl.classList.toggle("text-warning", !status.projectId);
        if (logoutBtn) logoutBtn.hidden = false;
        if (loginBtn) {
          loginBtn.hidden = false;
          // 已登录时改 button 文案为"切换账号"(reviewer #2 修)
          loginBtn.dataset.i18n = k("switchAccountBtn");
          loginBtn.textContent = tFmt(k("switchAccountBtn"));
        }
      }
    } catch (e) {
      // **catch 路径同 identity check**:fetch reject 后 user 可能也已切 provider
      if (activeOauthConfig !== config) {
        return;
      }
      const msg = e?.message || String(e);
      statusEl.dataset.i18n = k("statusFetchFailed");
      statusEl.textContent = tFmt(k("statusFetchFailed"), { error: msg });
      statusEl.classList.add("text-warning");
      // catch 路径**复位** button visibility 防 stale(C1 修)
      if (loginBtn) {
        loginBtn.hidden = false;
        loginBtn.dataset.i18n = k("loginBtn");
        loginBtn.textContent = tFmt(k("loginBtn"));
      }
      if (logoutBtn) logoutBtn.hidden = true;
    }
  }

  /// OAuth login click handler — long-poll 等浏览器授权 + bootstrap project。
  /// **silent-failure-hunter C2 修**:fetch 自带 timeout 风险(浏览器 ~ 90-300s vs
  /// 后端 5min),browser timeout 后端继续成功 → frontend 看 fail 但 status 看 success
  /// 矛盾 UX。修法:catch 不显 "failed" toast,而是显"timeout 等 status 刷新"+ refresh。
  /// 走 activeOauthConfig 拿对应 provider(gemini-cli vs antigravity)的 API + i18n
  async function handleOauthLogin() {
    const config = activeOauthConfig;
    if (!config) {
      // OAuth row 在隐藏状态下被点(button stale / e2e replay / 罕见 race)。
      // 不能 silent return —— user 看 click 没反应会怀疑 app hang。toast 给出提示
      console.warn("handleOauthLogin called with no active oauth config");
      showToast("OAuth login skipped: no active OAuth provider in form (switch apiFormat first)");
      return;
    }
    const k = (suffix) => `${config.i18nPrefix}.${suffix}`;
    const loginBtn = $("#oauthLoginBtn");
    const logoutBtn = $("#oauthLogoutBtn");
    if (loginBtn) {
      loginBtn.disabled = true;
      loginBtn.textContent = tFmt(k("loginBtnInProgress"));
    }
    if (logoutBtn) logoutBtn.disabled = true;
    try {
      const result = await config.api.login();
      if (result.loggedIn && result.projectId) {
        showToast(tFmt(k("loginSuccess"), {
          email: result.email || "?",
          projectId: result.projectId,
        }));
      } else if (result.loggedIn && !result.projectId) {
        // partial state — token 不该在此分支被持久化(后端 commit C C2 修),但 UI 防御
        showToast(tFmt(k("loginPartial")));
      } else if (result.error) {
        showToast(tFmt(k("loginFailed"), { error: result.error }));
      } else {
        // 未知 shape — 把整个 response 序列化进 toast 给 user 看到(silent H1 修)
        const dump = JSON.stringify(result).slice(0, 200);
        console.error("OAuth login unknown response shape:", result);
        showToast(tFmt(k("loginFailed"), { error: `unknown response: ${dump}` }));
      }
    } catch (e) {
      // C2:浏览器 fetch timeout 而后端可能仍成功 — 不显 "failed",改提示"refreshing"
      const msg = e?.message || String(e);
      console.warn("OAuth login fetch error (backend may have succeeded):", msg);
      showToast(`Login fetch interrupted (${msg}); refreshing status...`);
    } finally {
      if (loginBtn) loginBtn.disabled = false;
      if (logoutBtn) logoutBtn.disabled = false;
      // **silent-failure C1+C2 修**:long-poll 期间 user 可能切到非 OAuth /
      // 另一 OAuth provider,activeOauthConfig 已变。这种情况 refresh 会画错
      // provider 的状态进 row(toast 已给确认)— 跳过 refresh。回到原 provider
      // 时 setOauthRowState 会重新 fetch 一次 status,UI 收敛
      if (activeOauthConfig === config) {
        await refreshOauthStatusUi();
      }
    }
  }

  /// Logout click handler。**silent-failure-hunter H2 修**:logout 失败时显手动删
  /// 提示而不是 blindly refresh status (那会让 user 看 logged in 没意识 token 还在)。
  /// 走 activeOauthConfig 拿对应 provider 的 API + i18n
  async function handleOauthLogout() {
    const config = activeOauthConfig;
    if (!config) {
      console.warn("handleOauthLogout called with no active oauth config");
      showToast("OAuth logout skipped: no active OAuth provider in form");
      return;
    }
    const k = (suffix) => `${config.i18nPrefix}.${suffix}`;
    let failed = false;
    try {
      await config.api.logout();
      showToast(tFmt(k("logoutConfirmed")));
    } catch (e) {
      failed = true;
      const msg = e?.message || String(e);
      showToast(tFmt(k("logoutFailedManual"), { error: msg }));
      const statusEl = $("#oauthStatusText");
      if (statusEl) {
        statusEl.dataset.i18n = k("logoutFailedManual");
        statusEl.textContent = tFmt(k("logoutFailedManual"), { error: msg });
        statusEl.classList.add("text-warning");
      }
    } finally {
      // 失败时不刷新 status — 防覆盖 manual-delete 警告。成功时刷新让 UI 转 "未登录"。
      // **silent-failure C1+C2 修**:logout 长 poll 罕见但 race 同样适用 —
      // 切到别 provider 时不画旧 provider 的状态进 row
      if (!failed && activeOauthConfig === config) {
        await refreshOauthStatusUi();
      }
    }
  }

  function resetProviderForm() {
    editingProviderId = null;
    selectedPreset = null;
    providerAvailableModels = [];
    baseUrlMenuOpen = false;
    renderPresetOptions(null);
    updatePresetSelection();
    formModelCapabilities = {};
    formRequestOptions = {};
    setProviderFormMode("providersAdd.title");
    $("#providerName").value = "";
    $("#providerName").placeholder = "";
    $("#providerBaseUrl").value = "";
    $("#providerBaseUrl").placeholder = "";
    $("#providerBaseUrl").disabled = false;
    const trigger = $("#providerBaseUrlTrigger");
    if (trigger) trigger.hidden = true;
    renderBaseUrlOptions(null);
    setApiKeyInputState(false);
    $("#providerAuth").value = "bearer";
    renderApiFormatDisplay("openai_chat");
    setApiFormatMode(false, "openai_chat");
    setOauthRowState("openai_chat"); // P2.2 reset OAuth row 隐藏
    setGrokWebRowState("openai_chat"); // R1 PR-7 reset grok_web row 隐藏
    fillGrokWebFormFromProvider(null);
    setWebSearchRow(false, false, null);
    setProviderMappings(emptyMappings());
  }

  function applyPresetToForm(preset, notify = true) {
    // 自定义第三方:不预填 name/baseUrl(用户必须自己填),用 placeholder 提示
    // builtin preset:直接预填 name + baseUrl,用户保存即可
    const isCustom = preset.id === "custom-third-party";
    if (isCustom) {
      $("#providerName").value = "";
      $("#providerName").placeholder = preset.name;
      $("#providerBaseUrl").value = "";
      $("#providerBaseUrl").placeholder = "https://api.example.com/v1";
    } else {
      $("#providerName").value = preset.name;
      $("#providerName").placeholder = "";
      $("#providerBaseUrl").value = preset.baseUrl;
      $("#providerBaseUrl").placeholder = "";
    }
    $("#providerBaseUrl").disabled = false;
    const trigger = $("#providerBaseUrlTrigger");
    if (trigger) trigger.hidden = true;
    baseUrlMenuOpen = false;
    renderBaseUrlOptions(preset);
    setAuthSchemeValue(preset.authScheme);
    setApiKeyInputState(false);
    selectedPreset = preset;
    renderApiFormatDisplay(preset.apiFormat);
    setApiFormatMode(!!preset.allowApiFormatSelection, preset.apiFormat);
    setOauthRowState(preset.apiFormat); // P2.2 OAuth UI 切换
    setGrokWebRowState(preset.apiFormat); // R1 PR-7 grok_web UI 切换
    formModelCapabilities = normalizeCapabilities(preset.modelCapabilities || {});
    formRequestOptions = normalizeRequestOptions(preset.requestOptions || {});
    // Web Search 配置开关:preset 标支持 + preset.requestOptions.web_search_enabled
    // 决定初始 checkbox state(kimi / kimi-code 默认 true,xiaomi-mimo-* 默认 false,
    // 跟 backend `provider_web_search_enabled` 读取契约一致);hint 文案按
    // preset.id 选 provider-specific 段落
    setWebSearchRow(
      !!preset.supportsWebSearch,
      !!formRequestOptions.web_search_enabled,
      preset.id
    );
    providerAvailableModels = [];
    setProviderMappings(preset.models || emptyMappings());
    renderPresetOptions(preset, preset.models || emptyMappings());
    updatePresetSelection();
    if (notify) showToast(`${preset.name} ${t("toast.presetFilled")}`);
  }

  async function fillProviderForEdit(providerId) {
    const providers = await CCApi.getProviders();
    const provider = providers.find((item) => item.id === providerId);
    if (!provider) return;
    editingProviderId = provider.id;
    const matchedPreset = presetCache.find((preset) => presetMatchesProvider(preset, provider));
    selectedPreset = matchedPreset
      ? { ...matchedPreset, extraHeaders: provider.extraHeaders || matchedPreset.extraHeaders || {} }
      : {
        models: provider.mappings,
        extraHeaders: provider.extraHeaders || {},
        modelCapabilities: provider.modelCapabilities || {},
        requestOptions: provider.requestOptions || {},
      };
    formModelCapabilities = normalizeCapabilities(provider.modelCapabilities || selectedPreset.modelCapabilities || {});
    formRequestOptions = normalizeRequestOptions(provider.requestOptions || selectedPreset.requestOptions || {});
    setProviderFormMode("providersAdd.editTitle");
    $("#providerName").value = provider.name;
    $("#providerName").placeholder = "";
    $("#providerBaseUrl").value = provider.baseUrl;
    $("#providerBaseUrl").placeholder = "";
    // 内置 provider 不允许修改 baseUrl
    $("#providerBaseUrl").disabled = !!provider.isBuiltin;
    const baseUrlTrigger = $("#providerBaseUrlTrigger");
    if (baseUrlTrigger) baseUrlTrigger.hidden = !!provider.isBuiltin;
    baseUrlMenuOpen = false;
    renderBaseUrlOptions(selectedPreset);
    setApiKeyInputState(provider.hasApiKey);
    if (provider.hasApiKey) {
      try {
        const secret = await CCApi.getProviderSecret(provider.id);
        setApiKeyInputState(true, secret.apiKey || "");
      } catch (error) {
        console.error(error);
        showToast(error.message || t("toast.requestFailed"));
      }
    }
    setAuthSchemeValue(provider.authScheme);
    const effectiveFormat = (matchedPreset && matchedPreset.apiFormat) || provider.apiFormat;
    renderApiFormatDisplay(effectiveFormat);
    setApiFormatMode(false, effectiveFormat);
    setOauthRowState(effectiveFormat); // OAuth UI 切换(P2.2)
    setGrokWebRowState(effectiveFormat); // R1 PR-7 grok_web UI 切换
    fillGrokWebFormFromProvider(provider);
    // 编辑场景:支持判定走 matchedPreset.supportsWebSearch(自定义 provider 不命中
    // builtin → matchedPreset undefined → 不显示开关);初始 checkbox state 读
    // provider 实际保存的 requestOptions.web_search_enabled;hint 文案按
    // matchedPreset.id(自定义 provider 时 fallback 到 .default 通用文案)
    setWebSearchRow(
      !!(matchedPreset && matchedPreset.supportsWebSearch),
      !!formRequestOptions.web_search_enabled,
      matchedPreset?.id || null
    );
    providerAvailableModels = [];
    setProviderMappings(provider.mappings || emptyMappings());
    renderPresetOptions(selectedPreset, provider.mappings || emptyMappings());
    updatePresetSelection();
    // [MOC-69] antigravity 自动拉模型列表,让映射选框立即显示 displayName(不必手点「获取模型」);
    // 失败/离线静默保持 raw id 显示。只对 antigravity(唯一带 displayName 的 provider)生效。
    if ((effectiveFormat || provider.apiFormat) === "antigravity_oauth") {
      await autoFetchModelsForDisplay();
    }
  }

  async function renderProviderForm() {
    await renderPresets();
    if (editingProviderId) {
      await fillProviderForEdit(editingProviderId);
      return;
    }
    if (selectedPreset) {
      setProviderFormMode("providersAdd.title");
      applyPresetToForm(selectedPreset, false);
      return;
    }
    resetProviderForm();
  }

  async function renderProviders() {
    await renderModelMenuModePanel();
    await renderProviderCards("#providerRows");
  }

  function renderModelMenuModeState(settings = {}) {
    const enabled = !!settings.exposeAllProviderModels;
    const button = $("#modelMenuModeToggle");
    const hint = $("#modelMenuModeHint");
    if (button) {
      button.classList.toggle("btn-primary", enabled);
      button.classList.toggle("btn-outline-primary", !enabled);
      const span = $("span", button);
      if (span) span.textContent = enabled ? t("providers.showSingleModel") : t("providers.showAllModels");
      button.setAttribute("aria-pressed", enabled ? "true" : "false");
    }
    if (hint) {
      hint.textContent = enabled ? t("providers.modelMenuAllHint") : t("providers.modelMenuSingleHint");
    }
    const settingToggle = $("#exposeAllProviderModels");
    if (settingToggle) settingToggle.checked = enabled;
  }

  async function renderModelMenuModePanel() {
    const settings = await CCApi.getSettings();
    renderModelMenuModeState(settings);
  }

  async function renderModelSelectors() {
    const providers = await CCApi.getProviders();
    const select = $("#modelProvider");
    select.innerHTML = providers.map((provider) => `<option value="${escapeHtml(provider.id)}">${escapeHtml(provider.name)}</option>`).join("");
    const active = providers.find((provider) => provider.default) || providers[0];
    if (active) select.value = active.id;
    renderMappingCards();
  }

  async function renderMappingCards() {
    const providers = await CCApi.getProviders();
    const provider = providers.find((item) => item.id === $("#modelProvider").value) || providers[0];
    if (!provider) return;
    const defaultSelect = $("#defaultModel");
    if (defaultSelect) {
      const defaultValue = provider.mappings.default || provider.mappings.gpt_5_5 || "";
      const defaultKey = providerFormModelSlots.find((slot) => provider.mappings[slot.key] === defaultValue)?.key || "gpt_5_5";
      defaultSelect.value = defaultKey;
    }
    const result = $("#modelFetchResult");
    if (result) result.textContent = "";
    $("#mappingStack").innerHTML = providerFormModelSlots.slice(1).map((slot) => `
      <article class="mapping-card">
        <div class="mapping-title">
          <span class="mapping-icon ${slot.iconClass}"><i class="bi ${slot.icon}"></i></span>
          <strong>${slot.label}</strong>
          <span class="alias-pill">${slot.label}</span>
        </div>
        <input class="form-control form-control-lg" data-model-input="${slot.key}" value="${escapeHtml(provider.mappings[slot.key] || "")}">
        <span class="source-model"><i class="bi bi-arrow-left"></i>${slot.source}</span>
      </article>
    `).join("");
  }

  async function renderDesktop() {
    const desktop = await CCApi.getDesktopStatus();
    const entries = Object.entries(desktop.config || {});
    const health = desktop.health || {};
    const desktopReady = desktop.configured && !health.needsApply;
    const statusText = $("#desktopConfiguredText");
    statusText.textContent = health.needsApply
      ? t("status.needsApply")
      : desktop.configured ? t("status.configured") : t("status.notConfigured");
    statusText.classList.toggle("muted-text", !desktopReady);
    $(".desktop-card .circle-check")?.classList.toggle("warning", !desktopReady);
    renderDesktopHealthWarning("#desktopPageWarning", health);
    $("#desktopConfigList").innerHTML = entries.map(([key, value]) => `
      <div class="config-row"><i class="bi bi-check-circle-fill"></i><span>${escapeHtml(key)}:</span><code>${escapeHtml(Array.isArray(value) ? JSON.stringify(value) : value)}</code></div>
    `).join("");
    // Show env config commands instead of raw JSON
    const cmdBlock = desktop.commands?.temporary || JSON.stringify(desktop.config, null, 2);
    $("#desktopJson").textContent = cmdBlock;
  }

  let proxyLogTimer = null;
  let proxyLogInflight = false;
  let proxyLogAtBottom = true;
  const PROXY_LOG_BOTTOM_TOLERANCE = 8;

  function isProxyLogAtBottom(el) {
    return el.scrollTop + el.clientHeight >= el.scrollHeight - PROXY_LOG_BOTTOM_TOLERANCE;
  }

  function bindProxyLogScroll() {
    const logEl = $("#proxyLog");
    if (!logEl || logEl.dataset.scrollBound === "1") return;
    logEl.dataset.scrollBound = "1";
    logEl.addEventListener("scroll", () => {
      proxyLogAtBottom = isProxyLogAtBottom(logEl);
    }, { passive: true });
  }

  async function refreshProxyLog() {
    if (proxyLogInflight) return;
    const logEl = $("#proxyLog");
    if (!logEl) return;
    proxyLogInflight = true;
    try {
      const [proxyStatus, logs] = await Promise.all([
        CCApi.getProxyStatus(),
        CCApi.getProxyLogs(),
      ]);
      const wasAtBottom = proxyLogAtBottom;
      const prevScrollTop = logEl.scrollTop;
      logEl.innerHTML = logs.map((line) => `
        <div class="log-line"><span>${escapeHtml(line.at)}</span><span class="log-level ${escapeHtml(line.level)}">${escapeHtml(line.level.toUpperCase())}</span><span>${escapeHtml(line.message)}</span></div>
      `).join("");
      const userToggleOn = $("#autoScroll")?.checked !== false;
      if (userToggleOn && wasAtBottom) {
        logEl.scrollTop = logEl.scrollHeight;
        proxyLogAtBottom = true;
      } else {
        logEl.scrollTop = prevScrollTop;
        proxyLogAtBottom = isProxyLogAtBottom(logEl);
      }
      const statsEl = $("#proxyStats");
      if (statsEl) {
        const stats = [
          { label: t("proxy.stats.total"), value: proxyStatus.stats.total, icon: "bi-list-ul" },
          { label: t("proxy.stats.success"), value: proxyStatus.stats.success, icon: "bi-check-circle" },
          { label: t("proxy.stats.failed"), value: proxyStatus.stats.failed, icon: "bi-x-circle", danger: true },
          { label: t("proxy.stats.today"), value: proxyStatus.stats.today, icon: "bi-calendar3" },
        ];
        statsEl.innerHTML = stats.map((stat) => `
          <article class="stat-card ${stat.danger ? "danger" : ""}"><i class="bi ${stat.icon}"></i><div><span>${stat.label}</span><strong>${stat.value}</strong></div></article>
        `).join("");
      }
    } catch (err) {
      // 静默吞掉单次轮询失败，避免在控制台刷错误
    } finally {
      proxyLogInflight = false;
    }
  }

  function stopProxyLogAutoRefresh() {
    if (proxyLogTimer !== null) {
      clearInterval(proxyLogTimer);
      proxyLogTimer = null;
    }
    proxyLogAtBottom = true;
  }

  function startProxyLogAutoRefresh() {
    stopProxyLogAutoRefresh();
    proxyLogTimer = setInterval(() => {
      if (document.visibilityState === "hidden") return;
      refreshProxyLog();
    }, 2000);
  }

  async function renderProxy() {
    const status = await CCApi.getStatus();
    $("#proxyPort").value = status.proxyPort;
    $("#settingsProxyPort").value = status.proxyPort;
    $("#proxyStateText").textContent = status.proxyRunning ? t("status.running") : t("status.stopped");
    // ── 停止态视觉反馈:pulse-dot 灰色 + 状态文字灰色 ──
    const proxyRunningEl = document.querySelector(".proxy-running");
    if (proxyRunningEl) proxyRunningEl.classList.toggle("stopped", !status.proxyRunning);
    // ── toggle 按钮:running → Stop(danger),stopped → Start(success) ──
    const toggleBtn = $("#proxyToggleBtn");
    if (toggleBtn) {
      if (status.proxyRunning) {
        toggleBtn.className = "btn btn-danger btn-lg";
        toggleBtn.innerHTML = `<i class="bi bi-stop-fill"></i><span>${t("proxy.stop")}</span>`;
      } else {
        toggleBtn.className = "btn btn-success btn-lg";
        toggleBtn.innerHTML = `<i class="bi bi-play"></i><span>${t("proxy.start")}</span>`;
      }
    }
    bindProxyLogScroll();
    proxyLogAtBottom = true;
    await refreshProxyLog();
    startProxyLogAutoRefresh();
  }

  async function renderSettings() {
    const settings = await CCApi.getSettings();
    applyTheme(settings.theme || "default");
    $("#settingsProxyPort").value = settings.proxyPort;
    $("#settingsAdminPort").value = settings.adminPort;
    $("#autoApplyOnStart").checked = settings.autoApplyOnStart !== false;
   // 强制关闭(hotfix MOC-98):plugins 注入硬禁用,开关锁死 OFF + 禁用,不可操作。
   $("#autoUnlockCodexPlugins").checked = false;
   $("#autoUnlockCodexPlugins").disabled = true;
   if ($("#autoUnlockRestartCodex")) $("#autoUnlockRestartCodex").disabled = true;
    $("#autoWakeCodexPet").checked = settings.autoWakeCodexPet !== false;
   $("#exposeAllProviderModels").checked = !!settings.exposeAllProviderModels;
    showGrayPresets = settings.showGrayProviders === true;
    $("#showGrayProviders").checked = showGrayPresets;
    $("#restoreCodexOnExit").checked = settings.restoreCodexOnExit !== false;
    $("#mcpCredentialsPortableStore").checked = settings.mcpCredentialsPortableStore !== false;
    $("#codexNetworkAccess").checked = settings.codexNetworkAccess !== false;
    $("#codexStatusSectionDefaultVisible").checked = settings.codexStatusSectionDefaultVisible !== false;
    $("#settingsUpdateUrl").value = settings.updateUrl || "";
    renderModelMenuModeState(settings);
    await refreshAppVersion();
    await refreshBackupList();
    await refreshCodexSnapshotStatus();
    await refreshResidualScanStatus();
  }

  // #268 — Codex 原配置完整性自检渲染.
  async function refreshResidualScanStatus() {
    const statusEl = $("#residualScanStatus");
    const repairBtn = $("#repairResidualBtn");
    const previewEl = $("#residualScanPreview");
    if (!statusEl) return;
    statusEl.classList.remove("residual-clean", "residual-dirty");
    statusEl.textContent = t("settings.residualScanStatusUnknown");
    if (repairBtn) repairBtn.hidden = true;
    if (previewEl) {
      previewEl.hidden = true;
      previewEl.textContent = "";
    }
    let report;
    try {
      report = await CCApi.scanResidualPollution();
    } catch (error) {
      statusEl.textContent = tFmt("settings.residualScanStatusError", {
        error: error?.message || String(error),
      });
      return;
    }
    const count = (report?.polluted || []).length;
    if (count === 0) {
      statusEl.classList.add("residual-clean");
      statusEl.textContent = report?.transferCurrentlyApplied
        ? t("settings.residualScanStatusCleanWhileApplied")
        : t("settings.residualScanStatusClean");
      return;
    }
    statusEl.classList.add("residual-dirty");
    statusEl.textContent = tFmt("settings.residualScanStatusDirty", { count });
    if (repairBtn) repairBtn.hidden = false;
  }

  function formatResidualPreview(polluted) {
    const lines = [];
    for (const file of polluted) {
      const kindLabel = (() => {
        switch (file.kind) {
          case "liveConfig":
            return "~/.codex/config.toml";
          case "activeSnapshot":
            return "active snapshot";
          case "recoverySnapshot":
            return "recovery snapshot";
          default:
            return file.kind;
        }
      })();
      lines.push(`[${kindLabel}] ${file.path}`);
      for (const key of file.fieldsToStrip || []) {
        lines.push(`  - ${key}`);
      }
    }
    return lines.join("\n");
  }

  async function handleRepairResidual() {
    const previewEl = $("#residualScanPreview");
    let scan;
    try {
      scan = await CCApi.scanResidualPollution();
    } catch (error) {
      showToast(tFmt("settings.residualScanStatusError", {
        error: error?.message || String(error),
      }));
      return;
    }
    if (!scan?.polluted?.length) {
      await refreshResidualScanStatus();
      return;
    }
    const preview = formatResidualPreview(scan.polluted);
    if (previewEl) {
      previewEl.textContent = `${t("settings.residualScanPreviewTitle")}\n\n${preview}`;
      previewEl.hidden = false;
    }
    if (!window.confirm(tFmt("settings.residualScanConfirm", { preview }))) {
      return;
    }
    try {
      const result = await CCApi.repairResidualPollution({ dryRun: false });
      const cleaned = (result?.repair?.repaired || []).length;
      showToast(tFmt("settings.residualScanToastCleaned", { count: cleaned }));
    } catch (error) {
      showToast(tFmt("settings.residualScanStatusError", {
        error: error?.message || String(error),
      }));
    } finally {
      await refreshResidualScanStatus();
    }
  }

  async function refreshAppVersion() {
    const target = $("#appVersion");
    if (!target) return;
    try {
      const payload = await CCApi.getVersion();
      if (payload && payload.version) {
        target.textContent = payload.version;
      }
    } catch (error) {
      console.warn("Failed to load app version", error);
    }
  }

  async function refreshCodexSnapshotStatus() {
    const target = $("#codexSnapshotStatus");
    if (!target) return;
    try {
      const status = await CCApi.getDesktopSnapshotStatus();
      if (status && status.hasSnapshot) {
        target.textContent = tFmt("settings.codexSnapshotStatusActive", {
          time: status.snapshotAt || "",
        });
      } else if (status && status.restorableCount > 0) {
        target.textContent = tFmt("settings.codexSnapshotStatusRecovery", {
          count: status.restorableCount,
        });
      } else {
        target.textContent = t("settings.codexSnapshotStatusEmpty");
      }
    } catch (error) {
      target.textContent = t("settings.codexSnapshotStatusEmpty");
    }
  }

  function formatCodexSnapshotChoice(snapshot, index) {
    const kind = t(`settings.codexSnapshotKind.${snapshot.kind || "unknown"}`);
    const provider = snapshot.providerName || t("settings.codexSnapshotProviderUnknown");
    const time = snapshot.snapshotAt || t("settings.codexSnapshotTimeUnknown");
    const version = snapshot.appVersion || t("settings.codexSnapshotVersionUnknown");
    const files = [
      snapshot.configExisted ? "config.toml" : null,
      snapshot.authExisted ? "auth.json" : null,
    ].filter(Boolean).join(" + ") || t("settings.codexSnapshotFilesNone");
    return `${index + 1}. ${time} | ${kind} | ${provider} | ${version} | ${files}`;
  }

  async function chooseCodexRestoreTarget() {
    const snapshots = await CCApi.getDesktopSnapshots();
    if (!snapshots.length) {
      return window.confirm(t("confirm.desktopClearFallback")) ? { fallback: true } : null;
    }
    if (snapshots.length === 1) {
      const summary = formatCodexSnapshotChoice(snapshots[0], 0);
      return window.confirm(tFmt("confirm.desktopSnapshotRestoreSingle", { summary }))
        ? { snapshotId: snapshots[0].id }
        : null;
    }
    const list = snapshots.map(formatCodexSnapshotChoice).join("\n");
    const input = window.prompt(tFmt("confirm.desktopSnapshotSelect", { list }));
    if (input === null) return null;
    const selectedIndex = Number.parseInt(String(input).trim(), 10) - 1;
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0 || selectedIndex >= snapshots.length) {
      showToast(t("toast.desktopSnapshotInvalid"));
      return null;
    }
    const summary = formatCodexSnapshotChoice(snapshots[selectedIndex], selectedIndex);
    if (!window.confirm(tFmt("confirm.desktopSnapshotRestoreSelected", { summary }))) return null;
    return { snapshotId: snapshots[selectedIndex].id };
  }

  async function renderRoute(route) {
    $all(".page").forEach((page) => page.classList.toggle("active", page.dataset.page === route));
    $all(".route-tab").forEach((tab) => {
      const key = route.startsWith("providers") ? "providers" : route;
      tab.classList.toggle("active", tab.dataset.nav === key);
    });
    if (route !== "proxy") stopProxyLogAutoRefresh();
    // **#249 fix**:每个 render 函数单独 try-catch,防止单页 API 失败
    // 级联阻断其他页面渲染 / 首屏白屏。路由表保持与原 if 链一致(含 usage / theme)。
    const renders = {
      dashboard: renderDashboard,
      "providers/add": renderProviderForm,
      providers: renderProviders,
      desktop: renderDesktop,
      proxy: renderProxy,
      usage: renderUsage,
      settings: renderSettings,
      codex: renderCodexAssets,
      theme: renderTheme,
    };
    const fn = renders[route];
    if (fn) {
      try {
        await fn();
      } catch (err) {
        console.error(`[renderRoute] ${route} failed:`, err);
        showToast(err.message || t("toast.requestFailed"));
      }
    }
  }

  // ── Usage 页 (#279) — token 统计 ────────────────────────────────────────────
  // 数据流: GET /api/usage/summary → 后端 codex-app-transfer-usage-tracker
  // 扫 ~/.codex/sessions/ rollout JSONL,解析层 vendor 自 ryoppippi/ccusage(MIT)。
  let usageCache = null;
  let usageActiveView = "conversation"; // conversation | daily | model

  function fmtNum(n) {
    if (n === null || n === undefined) return "—";
    return Number(n).toLocaleString();
  }

  function fmtLastActivity(s) {
    if (!s) return "—";
    // ccusage 写 RFC3339;表格列宽紧张,只显示 YYYY-MM-DD HH:MM(秒/ms/Z/offset 全省)
    const m = s.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})/);
    return m ? `${m[1]} ${m[2]}` : s;
  }

  function renderUsageKpis(report) {
    const el = $("#usageKpis");
    if (!el) return;
    const kpis = [
      { label: t("usage.kpi.totalInput"), value: fmtNum(report.totalInputTokens), icon: "bi-arrow-down-circle" },
      { label: t("usage.kpi.totalOutput"), value: fmtNum(report.totalOutputTokens), icon: "bi-arrow-up-circle" },
      { label: t("usage.kpi.totalTokens"), value: fmtNum(report.totalTokens), icon: "bi-stack" },
      { label: t("usage.kpi.conversations"), value: fmtNum(report.totalConversations), icon: "bi-chat-square-text" },
    ];
    el.innerHTML = kpis.map((kpi) => `
      <article class="stat-card"><i class="bi ${kpi.icon}"></i><div><span>${escapeHtml(kpi.label)}</span><strong>${escapeHtml(kpi.value)}</strong></div></article>
    `).join("");
  }

  // 缓存命中率(#304):整体 hit% = cachedInput / input;input=0 → null(显示 —)
  function cacheHitPct(row) {
    const input = row.inputTokens || 0;
    if (input <= 0) return null;
    return Math.round(((row.cachedInputTokens || 0) / input) * 100);
  }

  // 按对话视图把命中率做成可点击(打开逐轮分布弹窗);其余视图纯数字。
  function cacheHitCell(row, view) {
    const pct = cacheHitPct(row);
    const txt = pct == null ? "—" : `${pct}%`;
    if (view === "conversation" && pct != null && row.group) {
      return `<td><button type="button" class="usage-cache-hit" data-session="${escapeHtml(row.group)}" title="${escapeHtml(t("usage.cacheModal.title"))}">${escapeHtml(txt)}</button></td>`;
    }
    return `<td>${escapeHtml(txt)}</td>`;
  }

  // 「按对话」首列:显示 Codex 对话名(session_index thread_name)前 5 字,全名 +
  // rollout 路径放 hover;无名时回退日期(MM/DD)。其余视图原样(日期 / 模型)。
  function firstColCell(row, view) {
    if (view !== "conversation") return `<td>${escapeHtml(row.group || "—")}</td>`;
    const name = (row.displayName || "").trim();
    let label;
    if (name) {
      label = name.length > 5 ? `${name.slice(0, 5)}…` : name;
    } else {
      const m = (row.group || "").match(/^\d{4}\/(\d{2})\/(\d{2})\//);
      label = m ? `${m[1]}/${m[2]}` : "—";
    }
    const full = name ? `${name}\n${row.group || ""}` : (row.group || "");
    return `<td title="${escapeHtml(full)}">${escapeHtml(label)}</td>`;
  }

  async function openCacheHitModal(session) {
    const modal = $("#usageCacheModal");
    const chart = $("#usageCacheChart");
    const summary = $("#usageCacheModalSummary");
    if (!modal || !chart) return;
    if (summary) summary.textContent = session || "";
    chart.innerHTML = `<div class="usage-cache-loading">${escapeHtml(t("usage.cacheModal.loading"))}</div>`;
    modal.hidden = false;
    try {
      const res = await fetch(`/api/usage/conversation/cache-series?session=${encodeURIComponent(session)}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      renderCacheChart(chart, summary, await res.json(), session);
    } catch (e) {
      console.warn("cas: load cache series failed", e);
      chart.innerHTML = `<div class="usage-cache-loading">${escapeHtml(t("usage.loadError"))}: ${escapeHtml(e?.message || String(e))}</div>`;
    }
  }

  // ≤10 桶后端已分好;每柱高度 = 该桶 token 加权命中率(cached/input)。
  function renderCacheChart(chart, summary, buckets, session) {
    if (!Array.isArray(buckets) || buckets.length === 0) {
      chart.innerHTML = `<div class="usage-cache-loading">${escapeHtml(t("usage.cacheModal.empty"))}</div>`;
      if (summary) summary.textContent = session || "";
      return;
    }
    let totCached = 0;
    let totInput = 0;
    let totOutput = 0;
    let maxInput = 0;
    buckets.forEach((b) => {
      totCached += b.cachedInputTokens || 0;
      totInput += b.inputTokens || 0;
      totOutput += b.outputTokens || 0;
      maxInput = Math.max(maxInput, b.inputTokens || 0);
    });
    const overall = totInput > 0 ? Math.round((100 * totCached) / totInput) : 0;
    if (summary) {
      summary.textContent =
        `${t("usage.cacheModal.overall")}: ${overall}%  ·  ${fmtNum(totCached)} / ${fmtNum(totInput)}` +
        `  ·  ${t("usage.cacheModal.output")} ${fmtNum(totOutput)}`;
    }
    const totalTurns = buckets[buckets.length - 1].turnEnd || 1;
    const lblHit = t("usage.cacheModal.hitInput");
    const lblTotal = t("usage.cacheModal.totalInput");
    const lblOut = t("usage.cacheModal.output");
    const bars = buckets.map((b) => {
      const input = b.inputTokens || 0;
      const cached = b.cachedInputTokens || 0;
      const output = b.outputTokens || 0;
      const pct = input > 0 ? Math.round((100 * cached) / input) : 0;
      // 柱高 = 该桶总输入相对全局最大输入(体现 token 量);柱内命中部分(底部、
      // 不同色)= cached/input —— 命中包含在总计里。
      const barH = maxInput > 0 ? Math.round((100 * input) / maxInput) : 0;
      const posPct = Math.round((100 * b.turnEnd) / totalTurns);
      const title = `${lblHit}: ${fmtNum(cached)}\n${lblTotal}: ${fmtNum(input)}\n${lblOut}: ${fmtNum(output)}`;
      return `<div class="ucbar" title="${escapeHtml(title)}">
        <div class="ucbar-track">
          <div class="ucbar-total" style="height:${barH}%">
            <div class="ucbar-hit" style="height:${pct}%"></div>
          </div>
        </div>
        <div class="ucbar-pct">${pct}%</div>
        <div class="ucbar-x">${posPct}%</div>
      </div>`;
    }).join("");
    chart.innerHTML = `<div class="ucbars">${bars}</div>`;
  }

  function renderUsageTable(report, view) {
    const head = $("#usageTableHead");
    const body = $("#usageTableBody");
    const empty = $("#usageEmpty");
    if (!head || !body || !empty) return;

    let rows;
    let firstColKey;
    if (view === "daily") {
      rows = report.daily || [];
      firstColKey = "usage.col.date";
    } else if (view === "model") {
      rows = report.byModel || [];
      firstColKey = "usage.col.model";
    } else {
      rows = report.byConversation || [];
      firstColKey = "usage.col.conversation";
    }

    if (!rows.length) {
      head.innerHTML = "";
      body.innerHTML = "";
      empty.hidden = false;
      return;
    }
    empty.hidden = true;

    // By Model 视图下 first column = model name,第二个 models 列内容必然跟 first
    // 列重复(每个 group 只有自己一个 model)— Devin Review #280 BUG_..._0001 fix:
    // model view 跳过第二列;daily / conversation view 保留(本日 / 本会话用到的
    // 模型清单是补充信息有意义)。
    const showModelsCol = view !== "model";
    const modelHeader = showModelsCol ? `<th class="usage-col-model">${escapeHtml(t("usage.col.model"))}</th>` : "";

    head.innerHTML = `
      <tr>
        <th>${escapeHtml(t(firstColKey))}</th>
        ${modelHeader}
        <th>${escapeHtml(t("usage.col.cacheHit"))}</th>
        <th>${escapeHtml(t("usage.col.input"))}</th>
        <th>${escapeHtml(t("usage.col.output"))}</th>
        <th>${escapeHtml(t("usage.col.reasoning"))}</th>
        <th>${escapeHtml(t("usage.col.total"))}</th>
        <th>${escapeHtml(t("usage.col.turns"))}</th>
        <th>${escapeHtml(t("usage.col.lastActivity"))}</th>
      </tr>
    `;

    // ccusage daily 视图按日期降序;model / conversation 按 total tokens 降序
    const sorted = rows.slice().sort((a, b) => {
      if (view === "daily") return (b.group || "").localeCompare(a.group || "");
      return (b.totalTokens || 0) - (a.totalTokens || 0);
    });

    body.innerHTML = sorted.map((row) => {
      // 按对话视图优先显示真实上游模型(proxy 本地记录);无则回退 rollout 客户端模型名。
      const modelText =
        view === "conversation" && row.upstreamModel
          ? row.upstreamModel
          : (row.models || []).join(", ") || "—";
      const modelCell = showModelsCol
        ? `<td class="usage-cell-model">${escapeHtml(modelText)}</td>`
        : "";
      return `
      <tr>
        ${firstColCell(row, view)}
        ${modelCell}
        ${cacheHitCell(row, view)}
        <td>${escapeHtml(fmtNum(row.inputTokens))}</td>
        <td>${escapeHtml(fmtNum(row.outputTokens))}</td>
        <td>${escapeHtml(fmtNum(row.reasoningOutputTokens))}</td>
        <td><strong>${escapeHtml(fmtNum(row.totalTokens))}</strong></td>
        <td>${escapeHtml(fmtNum(row.turnCount))}</td>
        <td>${escapeHtml(fmtLastActivity(row.lastActivity))}</td>
      </tr>
      `;
    }).join("");
  }

  async function fetchUsageReport() {
    // 浏览器 tz:Intl.DateTimeFormat().resolvedOptions().timeZone
    const tz = encodeURIComponent(Intl.DateTimeFormat().resolvedOptions().timeZone || "");
    const res = await fetch(`/api/usage/summary?tz=${tz}`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return await res.json();
  }

  function renderUsageError(message) {
    // silent-failure-hunter PR #279 修:fetch 失败不再写空 cache 让 UI 误显示
    // "0 用量",而是显示带 retry 的 error banner 让用户知道是 backend 错。
    const head = $("#usageTableHead");
    const body = $("#usageTableBody");
    const empty = $("#usageEmpty");
    const kpis = $("#usageKpis");
    if (head) head.innerHTML = "";
    if (body) body.innerHTML = "";
    if (empty) empty.hidden = true;
    if (kpis) {
      kpis.innerHTML = `
        <article class="stat-card danger" style="grid-column: 1 / -1;">
          <i class="bi bi-exclamation-triangle"></i>
          <div>
            <span>${escapeHtml(t("usage.kpi.totalTokens"))}</span>
            <strong style="font-size: 0.95rem;">${escapeHtml(message)}</strong>
          </div>
        </article>
      `;
    }
  }

  async function renderUsage(forceRefresh = false) {
    const loading = $("#usageLoading");
    if (!usageCache || forceRefresh) {
      if (loading) loading.hidden = false;
      try {
        usageCache = await fetchUsageReport();
      } catch (e) {
        console.warn("cas: load usage failed", e);
        if (loading) loading.hidden = true;
        renderUsageError(`${t("usage.loadError")}: ${e?.message || e}`);
        return;
      } finally {
        if (loading) loading.hidden = true;
      }
    }
    renderUsageKpis(usageCache);
    renderUsageTable(usageCache, usageActiveView);
    if (usageCache.unknownTimestampEvents && usageCache.unknownTimestampEvents > 0) {
      // 后端 Phase 1 加的字段:>0 说明 ts 解析失败,可能 Codex CLI 改 format
      console.warn(`cas: ${usageCache.unknownTimestampEvents} events have unparseable timestamps`);
    }
  }

  // delegate Usage 页交互
  document.addEventListener("click", (e) => {
    const viewBtn = e.target.closest(".usage-view-btn");
    if (viewBtn) {
      const view = viewBtn.dataset.usageView;
      if (!view || view === usageActiveView) return;
      usageActiveView = view;
      $all(".usage-view-btn").forEach((b) => b.classList.toggle("active", b.dataset.usageView === view));
      renderUsageTable(usageCache || { daily: [], byModel: [], byConversation: [] }, view);
      return;
    }
    const hitBtn = e.target.closest(".usage-cache-hit");
    if (hitBtn) {
      openCacheHitModal(hitBtn.dataset.session);
      return;
    }
    if (e.target.closest('[data-action="usage-cache-modal-close"]') || e.target.id === "usageCacheModal") {
      const m = $("#usageCacheModal");
      if (m) m.hidden = true;
      return;
    }
    if (e.target.closest("#usageRefreshBtn")) {
      usageCache = null;
      renderUsage(true);
    }
  });

  let currentTheme = "default";

  function normalizeTheme(theme) {
    if (!theme || theme === "light" || theme === "auto") return "default";
    return availableThemes.includes(theme) ? theme : "default";
  }

  function applyTheme(theme) {
    if (theme === "toggle") {
      theme = currentTheme === "dark" ? "default" : "dark";
    }
    const normalized = normalizeTheme(theme);
    currentTheme = normalized;
    document.documentElement.setAttribute("data-bs-theme", normalized === "dark" ? "dark" : "light");
    document.documentElement.setAttribute("data-theme-palette", normalized);
    $all(".theme-segment .btn").forEach((button) => {
      const active = button.dataset.themeAction === normalized;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", active ? "true" : "false");
    });
    const icon = $("[data-theme-action='toggle'] i");
    if (icon) icon.className = normalized === "dark" ? "bi bi-sun-fill" : "bi bi-moon-stars-fill";
    return normalized;
  }

  async function saveSettingsFromForm() {
    const settings = {
      theme: currentTheme,
      proxyPort: Number($("#settingsProxyPort").value),
      adminPort: Number($("#settingsAdminPort").value),
      autoApplyOnStart: $("#autoApplyOnStart")?.checked !== false,
     autoUnlockCodexPlugins: false,  // 强制关闭(hotfix MOC-98):硬写 false
      autoWakeCodexPet: $("#autoWakeCodexPet")?.checked !== false,
     exposeAllProviderModels: $("#exposeAllProviderModels")?.checked || false,
      showGrayProviders: $("#showGrayProviders")?.checked || false,
      restoreCodexOnExit: $("#restoreCodexOnExit")?.checked !== false,
      mcpCredentialsPortableStore: $("#mcpCredentialsPortableStore")?.checked !== false,
      codexNetworkAccess: $("#codexNetworkAccess")?.checked !== false,
      codexStatusSectionDefaultVisible: $("#codexStatusSectionDefaultVisible")?.checked !== false,
      updateUrl: $("#settingsUpdateUrl").value.trim(),
    };
    await CCApi.saveSettings(settings);
    $("#proxyPort").value = settings.proxyPort;
    renderModelMenuModeState(settings);
  }

  function formatUsageItems(result) {
    if (result.supported === false) return result.message;
    if (!result.items || !result.items.length) return result.message || t("providers.usageUnavailable");
    return result.items.map((item) => {
      const unit = item.unit ? ` ${item.unit}` : "";
      if (item.remaining !== null && item.remaining !== undefined) {
        return `${item.label}: ${item.remaining}${unit}`;
      }
      if (item.used !== null && item.used !== undefined) {
        return `${item.label}: ${item.used}${unit}`;
      }
      return item.label;
    }).join(" · ");
  }

  function formatProviderTestResult(result) {
    if (result?.message) return result.message;
    if (Number.isFinite(result?.latencyMs)) return `${Math.round(result.latencyMs)} ms`;
    return t("providers.testDone");
  }

  // 测速结果是否要 UI 标黄(.bad class)。**白名单语义**(silent-failure-hunter
  // review H2):后端将来加新 authStatus 枚举(`tls_warn` / `rate_limited` /
  // `cert_expired` 等)/ 或返 `success: false` 不带 ok 字段,helper 默认标黄不漏判。
  //
  // **修复历史(2026-05-10)**:`auth_required_or_invalid`(401/403)以前被
  // 当 bad 标黄,但 backend `test.rs:312-318` 注释明确"401/403 = baseUrl 连接性
  // OK + 鉴权未验证,应绿色"(测连接性本来不需要 key,鉴权层跟连接层解耦)。
  // 显式 allow-list 这个 authStatus 走绿色,其他未来新增 authStatus 默认仍标黄。
  function isProviderTestResultBad(result) {
    if (!result) return true;
    if (result.success === false) return true;
    if (result.ok === false) return true;
    if (result.authStatus && result.authStatus !== "ok"
        && result.authStatus !== "auth_required_or_invalid") {
      return true;
    }
    return false;
  }

  // 把 backend errors[] (object 数组,含 code/host/statusCode)按当前 locale i18n 翻译。
  // 历史兼容:string 元素直接显示。未识别的 code → 走 unknown / unknown_with_status
  // fallback,把 statusCode 拼进文案("上游返回错误 (HTTP 502)" / "Upstream error (HTTP 502)")。
  function translateUpstreamError(err) {
    if (typeof err === "string") return err;
    if (!err || typeof err !== "object") return t("models.upstreamError.unknown");
    const code = err.code || "unknown";
    let translated = t(`models.upstreamError.${code}`);
    // 没命中(返了 key 自身)→ fallback 通用文案
    if (translated === `models.upstreamError.${code}`) {
      translated = t("models.upstreamError.unknown");
    }
    if (err.statusCode) {
      // 动态 key (`models.upstreamError.${code}`) 已 t() 完,这里只对模板字符串
      // 替换 `{status}` 占位 — split/join 而非 String.replace 防 statusCode 含
      // `$` / 正则元字符被 replace 误解析。tFmt 不适用于"已 t 完的字符串"
      translated = translated.split("{status}").join(String(err.statusCode));
    }
    return err.host ? `[${err.host}] ${translated}` : translated;
  }

  function formatModelFetchError(error) {
    const errs = (error && error.errors) || [];
    // 优先用第一个结构化 error(最相关) — 已 i18n;退化路径用 error.message(网络层异常)
    const detail = errs.length > 0 ? translateUpstreamError(errs[0]) : (error && error.message);
    const reason = detail || t("toast.requestFailed");
    return `${t("models.fetchFailedManual")}: ${reason}`;
  }

  function downloadJson(filename, data) {
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    document.body.appendChild(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
  }

  async function refreshBackupList() {
    const target = $("#backupList");
    if (!target) return;
    try {
      const backups = await CCApi.listBackups();
      target.innerHTML = backups.length
        ? backups.slice(0, 5).map((item) => `<span>${escapeHtml(item.name)}</span><time>${escapeHtml(item.createdAt)}</time>`).join("")
        : `<span>${t("settings.noBackups")}</span>`;
    } catch (error) {
      target.innerHTML = `<span>${t("settings.backupLoadFailed")}</span>`;
    }
  }

  async function importConfigFile(file) {
    if (!file) return;
    if (!window.confirm(t("confirm.configImport"))) return;
    try {
      const text = await file.text();
      const configData = JSON.parse(text);
      await CCApi.importConfig(configData);
      await renderRoute(routeFromHash());
      showToast(t("toast.configImported"));
    } catch (error) {
      console.error(error);
      showToast(error.message || t("toast.configImportFailed"));
    } finally {
      const input = $("#configImportFile");
      if (input) input.value = "";
    }
  }

  function renderProviderCompatibilityList(result) {
    const target = $("#providerCompatibilityList");
    if (!target) return;
    const providers = result?.providers || [];
    if (!providers.length) {
      target.innerHTML = `<p class="compatibility-empty">${escapeHtml(t("settings.compatibilityEmpty"))}</p>`;
      return;
    }
    target.innerHTML = providers.map((provider) => `
      <article class="compatibility-item ${escapeHtml(provider.level)}">
        <div>
          <strong>${escapeHtml(provider.name)}</strong>
          <span>${escapeHtml(provider.message)}</span>
        </div>
        <em>${escapeHtml(provider.apiFormat)}</em>
      </article>
    `).join("");
  }

  async function saveProviderFromForm() {
    const payload = providerPayloadFromForm(true);
    // Responses 透传协议(direct mode)必须填齐 baseUrl + apiKey,否则 backend
    // 会 silent fallback 到 local_proxy → Codex.app 经代理 → 行为偏离用户预期。
    // 前端拦下让用户立即看到错误,而不是后端 fallback 后用户毫无察觉。
    if (payload.apiFormat === "responses" || payload.apiFormat === "openai_responses") {
      if (!payload.baseUrl) {
        throw new Error(t("toast.directModeBaseUrlRequired"));
      }
      if (!editingProviderId && !payload.apiKey) {
        throw new Error(t("toast.directModeApiKeyRequired"));
      }
    }
    // **code-reviewer #3 修**:OAuth provider 必须先登录才能 save。否则 backend
    // 存了 provider 但 extra.cloud_code_project_id 缺失,任何 chat 请求都返
    // BadRequest "cloud_code_project_id required" — 前端拦下让 user 立即知道要
    // 先登录,不是 save 后才发现 silently broken provider。两个 OAuth provider
    // 各自独立检查,错的 provider 不能 save
    {
      const config = OAUTH_PROVIDER_CONFIGS[payload.apiFormat];
      if (config) {
        const loginRequiredMsg = t(`${config.i18nPrefix}.loginRequired`);
        try {
          const status = await config.api.getStatus();
          if (!status.loggedIn || !status.projectId) {
            throw new Error(loginRequiredMsg);
          }
        } catch (e) {
          // 网络错也阻止 save — 不能让 unknown state 持久化
          if (e.message && e.message.includes(loginRequiredMsg)) throw e;
          throw new Error(`OAuth status check failed: ${e.message || e}`);
        }
      }
    }
    if (editingProviderId) {
      await CCApi.saveDraft(editingProviderId, payload);
      return { id: editingProviderId, ...payload };
    }
    const provider = await CCApi.addProvider(payload);
    editingProviderId = provider.id;
    await CCApi.saveDraft(provider.id, payload);
    return provider;
  }

  async function applyProviderToDesktop(actionEl) {
    // 表单页"启用"按钮 = 保存表单 + 走 set-default 同一条后端链路
    // (`switch_provider_and_sync` 写 activeProvider 并同步到 ~/.codex)。
    // 与 dashboard 的「启用」按钮(action="set-default")在用户感知上等价,
    // 唯一差异是这里要先把表单字段保存为 provider。**不弹 window.confirm**:
    // Tauri webview 在某些环境会静默忽略原生 confirm,导致用户看不到任何反馈
    // 误以为按钮失灵(2026-05-06 现场实测)。
    //
    // 响应延迟优化(2026-05-06):
    // 旧版按顺序 await 10+ RPC(saveProvider → setDefault → startProxy →
    // renderProviderCards × 1 → renderProviders 内 × 2 → renderDashboard 内
    // × 3,getProviders 重复 3 次),链路 1.5-3s 才解锁按钮。
    // 现在:
    // 1. 关键路径只 await `saveProviderFromForm` + `setDefaultProvider`(2-3
    //    RPC),拿到结果立刻 hash 跳页 / toast / 重启提示。
    // 2. hash → "dashboard" 由路由器自动触发 `renderDashboard`,不再手动调,
    //    避免与路由器重复 fetch 同一份 providers/status。
    // 3. `startProxy`(若 desktopSync.requiresProxy)放后台,不阻塞 UI。
    // 4. providers 页 / model 选择器等"用户没在看"的渲染留给下次进入时再
    //    跑,避免冗余 RPC。
    const form = $("#providerForm");
    if (form && !form.reportValidity()) return;

    actionEl.disabled = true;
    try {
      const provider = await saveProviderFromForm();
      const result = await CCApi.setDefaultProvider(provider.id);
      const desktopSync = result?.desktopSync || {};

      editingProviderId = null;
      selectedPreset = null;
      window.location.hash = "dashboard";
      if (desktopSync.attempted && desktopSync.success === false) {
        showToast(t("toast.defaultUpdatedDesktopFailed"));
      } else {
        showToast(t("toast.defaultUpdatedDesktop"));
      }
      // MOC-20:启用解耦 — 不再强制弹 restart-reminder modal,toast 文案已提示
      // 用户去首页 quick-actions『重启 Codex』按钮手动重启(避免误点丢上下文)。

      if (desktopSync.requiresProxy) {
        CCApi.startProxy().catch((error) => {
          console.error("applyProviderToDesktop background startProxy failed:", error);
        });
      }
    } finally {
      actionEl.disabled = false;
    }
  }

  async function handleAction(target) {
    const action = target.closest("[data-action]")?.dataset.action;
    if (!action) return;
    const actionEl = target.closest("[data-action]");

    if (action === "toggle-key") {
      const input = $("#providerApiKey");
      input.type = input.type === "password" ? "text" : "password";
      actionEl.innerHTML = `<i class="bi ${input.type === "password" ? "bi-eye" : "bi-eye-slash"}"></i>`;
    }

    try {
      if (action === "set-default") {
        const result = await CCApi.setDefaultProvider(actionEl.dataset.id);
        if (result.desktopSync?.requiresProxy) {
          await CCApi.startProxy();
        }
        await renderProviderCards("#dashboardProviderCards", { includePresets: true });
        await renderProviders();
        await renderDashboard();
        const desktopSync = result.desktopSync || {};
        // MOC-20:启用解耦 — 不再强制弹 restart-reminder modal,改用 toast +
        // 让用户去首页『重启 Codex』按钮手动重启。
        if (desktopSync.attempted && desktopSync.success) {
          showToast(t("toast.defaultUpdatedDesktop"));
        } else if (desktopSync.attempted && desktopSync.success === false) {
          showToast(t("toast.defaultUpdatedDesktopFailed"));
        } else {
          showToast(t("toast.defaultUpdated"));
        }
      }

      if (action === "restart-codex-dashboard") {
        // MOC-20:首页 quick-actions『重启 Codex』按钮 — 复用 restartCodexAppNow,
        // 传 dashboard 按钮 id,hideModal=false(无 modal 可隐),按 dashboard 文案 fallback。
        await restartCodexAppNow({
          buttonId: "dashboardRestartCodexBtn",
          fallbackLabelKey: "dashboard.restartCodex",
          hideModal: false,
        });
      }

      if (action === "new-from-preset") {
        const presets = await CCApi.getPresets();
        selectedPreset = presets.find((item) => item.id === actionEl.dataset.preset) || null;
        editingProviderId = null;
        window.location.hash = "providers/add";
      }

      if (action === "edit-provider") {
        editingProviderId = actionEl.dataset.id;
        selectedPreset = null;
        window.location.hash = "providers/add";
      }

      if (action === "copy-url") {
        await navigator.clipboard.writeText(actionEl.dataset.url || "");
        showToast(t("toast.copied"));
      }

      if (action === "open-docs") {
        event.preventDefault();
        const url = actionEl.dataset.docsUrl;
        const name = actionEl.dataset.providerName || "";
        if (!url) return;
        const message = tFmt("confirm.openDocs", { provider: name });
        if (window.confirm(message)) {
          window.open(url, "_blank", "noopener,noreferrer");
        }
      }

      if (action === "test-provider") {
        const resultEl = $(`[data-speed-for="${actionEl.dataset.id}"]`);
        actionEl.disabled = true;
        if (resultEl) {
          resultEl.textContent = t("providers.testing");
          resultEl.classList.remove("bad");
        }
        try {
          const result = await CCApi.testProvider(actionEl.dataset.id);
          const message = formatProviderTestResult(result);
          if (resultEl) {
            resultEl.textContent = message;
            resultEl.classList.toggle("bad", isProviderTestResultBad(result));
          }
          showToast(message);
        } catch (error) {
          const message = error?.message || t("toast.requestFailed");
          if (resultEl) {
            resultEl.textContent = message;
            resultEl.classList.add("bad");
          }
          showToast(message);
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "query-usage") {
        const resultEl = $(`[data-usage-for="${actionEl.dataset.id}"]`) || $(`[data-speed-for="${actionEl.dataset.id}"]`);
        actionEl.disabled = true;
        if (resultEl) {
          resultEl.textContent = t("providers.usageQuerying");
          resultEl.classList.remove("bad");
        }
        try {
          const result = await CCApi.queryProviderUsage(actionEl.dataset.id);
          const message = formatUsageItems(result);
          if (resultEl) {
            resultEl.textContent = message;
            resultEl.classList.toggle("bad", result.ok === false || result.supported === false);
          }
          showToast(message);
        } catch (error) {
          const message = error?.message || t("toast.requestFailed");
          if (resultEl) {
            resultEl.textContent = message;
            resultEl.classList.add("bad");
          }
          showToast(message);
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "test-provider-form") {
        const resultEl = $("#formSpeedResult");
        actionEl.disabled = true;
        resultEl.textContent = t("providers.testing");
        resultEl.classList.remove("bad");
        try {
          const payload = providerPayloadFromForm(true);
          if (editingProviderId && !payload.apiKey) {
            try {
              const secret = await CCApi.getProviderSecret(editingProviderId);
              if (secret.apiKey) payload.apiKey = secret.apiKey;
            } catch (e) { /* ignore */ }
          }
          if (editingProviderId) {
            await CCApi.saveDraft(editingProviderId, payload);
          }
          const result = await CCApi.testProviderPayload(payload);
          const message = formatProviderTestResult(result);
          resultEl.textContent = message;
          resultEl.classList.toggle("bad", isProviderTestResultBad(result));
          showToast(message);
        } catch (error) {
          const message = error?.message || t("toast.requestFailed");
          resultEl.textContent = message;
          resultEl.classList.add("bad");
          showToast(message);
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "fetch-form-models") {
        const resultEl = $("#providerModelFetchResult");
        actionEl.disabled = true;
        if (resultEl) resultEl.textContent = t("models.fetching");
        try {
          const payload = providerPayloadFromForm(false);
          if (editingProviderId && !payload.apiKey) {
            try {
              const secret = await CCApi.getProviderSecret(editingProviderId);
              if (secret.apiKey) payload.apiKey = secret.apiKey;
            } catch (e) { /* ignore */ }
          }
          if (editingProviderId) {
            await CCApi.saveDraft(editingProviderId, payload);
          }
          const result = await CCApi.fetchProviderModelsPayload(payload);
          providerAvailableModels = Array.isArray(result.models) ? result.models.slice() : [];
          // **不覆盖 user 已有 mappings,只刷新下拉选项**(2026-05-11 修):
          // 原 `setProviderMappings(result.suggested, ...)` 会用 suggested 整覆盖
          // (suggested 后端只填 default,其他 slot 是空串)→ user 自己设的
          // gpt_5_5 / gpt_5_4 等被清空。期望行为:获取模型只更新下拉可选项,
          // 不清 user 已有映射(default 留空时才允许 suggested.default 填进去)
          const suggestedDefault = (result.suggested && result.suggested.default) || "";
          if (suggestedDefault && !providerFormMappings.default) {
            providerFormMappings.default = suggestedDefault;
          }
          setProviderMappings(providerFormMappings, { availableModels: providerAvailableModels });
          if (resultEl) resultEl.textContent = t("models.fetchSuccess");
          showToast(t("toast.modelsAutofilled"));
        } catch (error) {
          providerAvailableModels = [];
          renderProviderMappings();
          const message = formatModelFetchError(error);
          if (resultEl) resultEl.textContent = message;
          showToast(message);
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "add-provider-model-row") {
        addProviderMappingRow();
      }

      if (action === "remove-provider-model-row") {
        removeProviderMappingRow(Number(actionEl.dataset.rowIndex));
      }

      if (action === "toggle-provider-model-slot-menu") {
        toggleProviderSlotMenu(Number(actionEl.dataset.rowIndex));
      }

      if (action === "toggle-baseurl-menu") {
        toggleBaseUrlMenu();
      }

      if (action === "select-baseurl-option") {
        setBaseUrlValue(actionEl.dataset.baseurlValue || "");
      }

      if (action === "select-provider-model-slot") {
        moveProviderMappingRow(Number(actionEl.dataset.rowIndex), actionEl.dataset.slotKey);
        renderPresetOptions(selectedPreset, collectProviderMappings());
      }

      if (action === "toggle-provider-model-menu") {
        toggleProviderModelMenu(actionEl.dataset.rowKey);
      }

      if (action === "select-provider-model-option") {
        const rowKey = openProviderModelMenuKey;
        if (rowKey) {
          updateProviderModelInput(rowKey, actionEl.dataset.modelValue || "");
          closeProviderModelMenu();
          renderPresetOptions(selectedPreset, collectProviderMappings());
        }
      }

      if (action === "delete-provider") {
        pendingDeleteId = actionEl.dataset.id;
        deleteModal.show();
      }

      if (action === "save-models") {
        const mappings = {};
        $all("[data-model-input]").forEach((input) => {
          mappings[input.dataset.modelInput] = input.value.trim();
        });
        const defaultKey = $("#defaultModel")?.value || "gpt_5_5";
        mappings.default = mappings[defaultKey] || mappings.gpt_5_5 || mappings.gpt_5_4 || mappings.gpt_5_4_mini || mappings.gpt_5_3_codex || mappings.gpt_5_2 || "";
        await CCApi.saveModelMappings($("#modelProvider").value, mappings);
        showToast(t("toast.modelsSaved"));
      }

      if (action === "fetch-models") {
        const providerId = $("#modelProvider").value;
        const resultEl = $("#modelFetchResult");
        actionEl.disabled = true;
        if (resultEl) resultEl.textContent = t("models.fetching");
        try {
          const result = await CCApi.autofillProviderModels(providerId);
          await renderMappingCards();
          if (resultEl) {
            resultEl.textContent = `${t("models.fetched")} ${result.models.length}`;
          }
          showToast(t("toast.modelsAutofilled"));
        } catch (error) {
          const message = formatModelFetchError(error);
          if (resultEl) resultEl.textContent = message;
          showToast(message);
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "reset-models") {
        await renderMappingCards();
        showToast(t("toast.modelsReset"));
      }

      if (action === "apply-desktop") {
        const result = await CCApi.configureDesktop();
        if (result && result.commands && result.commands.temporary) {
          await navigator.clipboard.writeText(result.commands.temporary);
          showToast(t("toast.desktopApplied"));
        } else {
          showToast(t("toast.desktopApplied"));
        }
        await renderDesktop();
      }

      if (action === "clear-desktop") {
        const target = await chooseCodexRestoreTarget();
        if (!target) return;
        const result = target.snapshotId
          ? await CCApi.restoreDesktopSnapshot(target.snapshotId)
          : await CCApi.clearDesktop();
        const route = routeFromHash();
        if (route === "dashboard") {
          await renderDashboard();
        } else if (route === "desktop") {
          await renderDesktop();
        } else if (route === "settings") {
          await refreshCodexSnapshotStatus();
        }
        const fellBackToLegacy = result && result.restored === false;
        showToast(t(fellBackToLegacy ? "toast.desktopClearedLegacy" : "toast.desktopCleared"));
      }

      if (action === "rescan-residual") {
        await refreshResidualScanStatus();
        return;
      }

      if (action === "codex-conv-refresh") {
        await codexConversationsLoadAndRender();
        return;
      }
      if (action === "codex-conv-export-selected") {
        await codexConversationsExportSelected();
        return;
      }
      if (action === "codex-conv-delete-selected") {
        await codexConversationsDeleteSelected();
        return;
      }
      if (action === "codex-conv-default-dir-pick") {
        await codexConvPickDefaultDir();
        return;
      }
      if (action === "codex-conv-default-dir-clear") {
        codexConvClearDefaultDir();
        return;
      }
      if (action === "codex-conv-options") {
        codexConversationsOpenOptionsDialog();
        return;
      }

      if (action === "repair-residual") {
        await handleRepairResidual();
        return;
      }

      if (action === "proxy-start") {
        await CCApi.startProxy($("#proxyPort") ? $("#proxyPort").value : 18080);
        await renderProxy();
        await renderDashboard();
        showToast(t("toast.proxyStarted"));
      }

      if (action === "proxy-stop") {
        await CCApi.stopProxy();
        await renderProxy();
        await renderDashboard();
        showToast(t("toast.proxyStopped"));
      }

      if (action === "proxy-toggle") {
        const currentStatus = await CCApi.getProxyStatus();
        if (currentStatus.running) {
          await CCApi.stopProxy();
          showToast(t("toast.proxyStopped"));
        } else {
          await CCApi.startProxy($("#proxyPort") ? $("#proxyPort").value : 18080);
          showToast(t("toast.proxyStarted"));
        }
        await renderProxy();
        await renderDashboard();
      }

      if (action === "clear-logs") {
        await CCApi.clearLogs();
        await renderProxy();
        showToast(t("toast.logsCleared"));
      }

      if (action === "open-log-dir") {
        try {
          await CCApi.openLogDir();
          showToast(t("toast.logDirOpened"));
        } catch (err) {
          showToast(t("toast.logDirOpenFailed"));
        }
      }

      if (action === "view-logs") {
        window.location.hash = "proxy";
      }

      if (action === "open-feedback") {
        openFeedbackModal();
      }

      if (action === "toggle-model-menu-mode") {
        const settings = await CCApi.getSettings();
        const next = !settings.exposeAllProviderModels;
        const saved = await CCApi.saveSettings({ exposeAllProviderModels: next });
        renderModelMenuModeState(saved);
        showToast(next ? t("toast.allModelsEnabled") : t("toast.singleModelEnabled"));
      }

      if (action === "check-provider-compatibility") {
        actionEl.disabled = true;
        try {
          const result = await CCApi.getProviderCompatibility();
          renderProviderCompatibilityList(result);
          showToast(t("toast.compatibilityChecked"));
        } finally {
          actionEl.disabled = false;
        }
      }

      if (action === "check-update") {
        const result = await CCApi.checkUpdate($("#settingsUpdateUrl").value.trim());
        updateCheckCache = result;
        renderUpdateBadge(result);
        const message = result.updateAvailable
          ? `${t("toast.updateAvailable")} ${result.latestVersion}`
          : `${t("toast.noUpdate")} ${result.currentVersion}`;
        const status = $("#updateStatus");
        if (status) {
          status.textContent = message;
          status.classList.toggle("available", !!result.updateAvailable);
        }
        showToast(message);
      }

      if (action === "install-update") {
        if (!updateCheckCache?.updateAvailable) {
          updateCheckCache = await CCApi.checkUpdate($("#settingsUpdateUrl")?.value.trim() || "");
          renderUpdateBadge(updateCheckCache);
        }
        if (!updateCheckCache?.updateAvailable) {
          const message = `${t("toast.noUpdate")} ${updateCheckCache?.currentVersion || ""}`.trim();
          const status = $("#updateStatus");
          if (status) {
            status.textContent = message;
            status.classList.remove("available");
          }
          showToast(message);
          return;
        }
        if (!window.confirm(t("confirm.installUpdate"))) return;
        let keepBusyState = false;
        const status = $("#updateStatus");
        setUpdateInstallPhase("downloading");
        if (status) {
          status.textContent = t("toast.updateDownloading");
          status.classList.add("available");
        }
        try {
          const result = await CCApi.installUpdate($("#settingsUpdateUrl")?.value.trim() || "");
          updateCheckCache = result;
          keepBusyState = !!result.quitRequested;
          setUpdateInstallPhase(keepBusyState ? "installing" : "idle");
          renderUpdateBadge(result);
          const message = result.message || t("toast.updateInstallerStarted");
          if (status) {
            status.textContent = message;
            status.classList.toggle("available", !!result.updateAvailable);
          }
          showToast(message);
        } catch (error) {
          setUpdateInstallPhase("idle");
          throw error;
        } finally {
          if (!keepBusyState) setUpdateInstallPhase("idle");
        }
      }

      if (action === "backup-config") {
        await CCApi.createBackup();
        await refreshBackupList();
        showToast(t("toast.configBackedUp"));
      }

      if (action === "export-config") {
        const data = await CCApi.exportConfig();
        const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
        downloadJson(`codex-app-transfer-config-${stamp}.json`, data);
        showToast(t("toast.configExported"));
      }

      if (action === "choose-import-config") {
        $("#configImportFile").click();
      }

      if (action === "apply-provider-desktop") {
        await applyProviderToDesktop(actionEl);
      }

      // ── Codex 资产管理 (#24 / #25, Agents + MCP + Skills tab) ──
      if (action === "codex-block-preview") {
        await codexBlockPreview(currentCodexBlockType());
      }
      if (action === "codex-block-apply") {
        await codexBlockApply(currentCodexBlockType());
      }
      if (action === "codex-block-history-toggle") {
        await codexBlockToggleHistory(currentCodexBlockType());
      }
      if (action === "codex-block-clear") {
        if (window.confirm(t("codex.confirmClear"))) {
          await codexBlockClear(currentCodexBlockType());
        }
      }
      if (action === "codex-block-rollback") {
        const idx = Number(actionEl.dataset.idx);
        const type = actionEl.dataset.type || currentCodexBlockType();
        if (!Number.isFinite(idx)) return;
        if (window.confirm(tFmt("codex.confirmRollback", { type, idx }))) {
          await codexBlockRollback(type, idx);
        }
      }
      if (action === "codex-agents-path-add") {
        codexAgentsOnPathAdd("agents");
      }
      if (action === "codex-agents-path-remove") {
        await codexAgentsOnPathRemove();
      }
      if (action === "codex-memories-edit-start") {
        await codexMemoriesOnEditStart();
      }
      if (action === "codex-memories-apply") {
        await codexMemoriesOnApply();
      }
      if (action === "codex-memories-cancel") {
        codexMemoriesOnCancel();
      }
      if (action === "codex-memories-backup") {
        await codexMemoriesOnBackup();
      }
      if (action === "codex-memories-history-toggle") {
        await codexMemoriesToggleHistory();
      }
      if (action === "codex-skills-edit-start") {
        await codexSkillsOnEditStart();
      }
      if (action === "codex-skills-apply") {
        await codexSkillsOnApply();
      }
      if (action === "codex-skills-cancel") {
        codexSkillsOnCancel();
      }
      if (action === "codex-skills-backup-md") {
        await codexSkillsOnBackup();
      }
      if (action === "codex-skills-history-toggle") {
        await codexSkillsToggleHistory();
      }
      if (action === "codex-skills-reveal") {
        await codexSkillsOnReveal();
      }
      // ── MCP ──
      if (action === "codex-mcp-server-new") {
        codexMcpServerNew();
      }
      if (action === "codex-mcp-new-cancel") {
        codexMcpServerNewCancel();
      }
      if (action === "codex-mcp-new-confirm") {
        codexMcpServerNewConfirm();
      }
      if (action === "codex-mcp-server-edit") {
        codexMcpServerEditToggle();
      }
      if (action === "codex-mcp-server-delete") {
        await codexMcpServerDelete();
      }
      if (action === "codex-mcp-servers-backup") {
        await codexMcpServersBackup();
      }
      if (action === "codex-mcp-servers-history") {
        await codexMcpServersOpenHistory();
      }
      if (action === "codex-mcp-raw-toggle") {
        await codexMcpRawToggle();
      }
      if (action === "codex-mcp-raw-apply") {
        await codexMcpRawApply();
      }
      if (action === "codex-mcp-raw-cancel") {
        codexMcpRawCancel();
      }
      if (action === "codex-mcp-form-toggle-advanced") {
        const pane = $("#codexMcpAdvancedPane");
        if (pane) pane.hidden = !pane.hidden;
      }
      if (action === "codex-mcp-form-add-arg") { codexMcpAddArgRow(); }
      if (action === "codex-mcp-form-remove-arg") { codexMcpRemoveArgRow(actionEl.dataset.idx); }
      if (action === "codex-mcp-form-add-env") { codexMcpAddKvRow("env"); }
      if (action === "codex-mcp-form-add-hh") { codexMcpAddKvRow("hh"); }
      if (action === "codex-mcp-form-add-ehh") { codexMcpAddKvRow("ehh"); }
      if (action === "codex-mcp-form-remove-kv") { codexMcpRemoveKvRow(actionEl.dataset.prefix, actionEl.dataset.idx); }
      if (action === "codex-mcp-plugin-toggle") {
        const key = actionEl.dataset.key;
        const enabled = !!actionEl.checked;
        await codexMcpPluginToggle(key, enabled);
      }
      if (action === "codex-mcp-plugin-toggle-btn") {
        const key = actionEl.dataset.key;
        const wasEnabled = actionEl.dataset.enabled === "true";
        await codexMcpPluginToggle(key, !wasEnabled);
      }
      if (action === "codex-mcp-plugin-uninstall") {
        const key = actionEl.dataset.key;
        await codexMcpPluginUninstall(key);
      }
      if (action === "codex-mcp-source-add-open") {
        codexMcpSourceAddOpen();
      }
      if (action === "codex-mcp-source-modal-close") {
        codexMcpSourceAddClose();
      }
      if (action === "codex-mcp-source-modal-confirm") {
        await codexMcpSourceAddConfirm();
      }
      if (action === "codex-mcp-source-toggle") {
        const id = actionEl.dataset.id;
        const enabled = actionEl.dataset.enabled === "true";
        await codexMcpSourceToggle(id, enabled);
      }
      if (action === "codex-mcp-source-remove") {
        const id = actionEl.dataset.id;
        await codexMcpSourceRemove(id);
      }
      if (action === "codex-mcp-market-refresh") {
        await codexMcpReloadMarketIndex(true);
      }
      if (action === "codex-mcp-market-install-server") {
        await codexMcpMarketInstallServer(actionEl.dataset.id);
      }
      if (action === "codex-mcp-market-install-plugin") {
        await codexMcpMarketInstallPlugin(actionEl.dataset.id, actionEl.dataset.marketplace);
      }
      if (action === "codex-mcp-deeplink-cancel") {
        codexMcpDeeplinkCancel();
      }
      if (action === "codex-mcp-deeplink-confirm") {
        await codexMcpDeeplinkConfirm();
      }
      if (action === "codex-add-path-cancel") {
        codexAgentsClosePathModal();
      }
      if (action === "codex-add-path-browse") {
        await codexAgentsOnBrowse();
      }
      if (action === "codex-add-path-confirm") {
        await codexAgentsConfirmPathAdd();
      }
      if (action === "codex-agents-edit-start") {
        await codexAgentsOnEditStart();
      }
      if (action === "codex-agents-apply") {
        await codexAgentsOnApply();
      }
      if (action === "codex-agents-cancel") {
        codexAgentsOnCancel();
      }
      if (action === "codex-agents-backup") {
        await codexAgentsOnBackup();
      }
      if (action === "codex-agents-history-toggle") {
        await codexAgentsToggleHistory();
      }
      if (action === "codex-history-close") {
        codexHistoryClose();
      }
      if (action === "codex-history-restore") {
        await codexHistoryRestore();
      }
    } catch (error) {
      console.error(error);
      showToast(error.message || t("toast.requestFailed"));
    }
  }

  // ── Codex 文档管理: marker 受管块 (agents + mcp 共享, type ∈ {agents, mcp}) ──

  /** 当前选中的 AGENTS.md 路径 hash(null = 默认全局)*/
  let currentAgentsHash = null;
  /** 当前选中的 MEMORY.md 路径 hash */
  let currentMemoriesHash = null;
  /** 当前选中的 SKILL.md 路径 hash */
  let currentSkillsHash = null;
  /** Add modal / History modal 当前服务的 resource("agents" / "memories" / "skills")*/
  let codexDocActiveResource = "agents";

  /** resource → API base */
  function codexDocApiBase(resource) {
    if (resource === "memories") return "/api/codex/memories-md";
    if (resource === "skills") return "/api/codex/skills-md";
    return "/api/codex/agents-md";
  }

  /** resource → 当前 hash */
  function codexDocCurrentHash(resource) {
    if (resource === "memories") return currentMemoriesHash;
    if (resource === "skills") return currentSkillsHash;
    return currentAgentsHash;
  }
  function codexDocSetCurrentHash(resource, hash) {
    if (resource === "memories") currentMemoriesHash = hash;
    else if (resource === "skills") currentSkillsHash = hash;
    else currentAgentsHash = hash;
  }

  /** URL prefix for managed-block endpoints。agents tab 自动拼 ?hash=<currentAgentsHash> */
  function codexBlockUrl(type) {
    if (type === "mcp") return "/api/codex/mcp-toml";
    if (type === "memories") return "/api/codex/memories-md";
    return "/api/codex/agents-md";
  }

  /** agents endpoint suffix(hash query)— 仅 type=agents 时拼 */
  function codexAgentsHashSuffix(type) {
    if (type !== "agents") return "";
    return currentAgentsHash ? `?hash=${encodeURIComponent(currentAgentsHash)}` : "";
  }

  function currentCodexTab() {
    return $("#codexSidebar .codex-sidebar-item.active")?.dataset?.codexTab || "agents";
  }

  function currentCodexBlockType() {
    const tab = currentCodexTab();
    if (tab === "mcp") return "mcp";
    if (tab === "memories") return "memories";
    return "agents";
  }

  async function codexBlockFetchStatus(type) {
    const r = await fetch(`${codexBlockUrl(type)}/status${codexAgentsHashSuffix(type)}`);
    if (!r.ok) throw new Error("status request failed");
    return r.json();
  }

  // ── AGENTS.md 自定义路径 dropdown ──

  /** 路径 chip(s) HTML — 按 category 决定单 chip 或双 chip */
  function codexAgentsChipsHtml(entry) {
    if (entry.category === "global") {
      return `<span class="codex-path-chip codex-path-chip-global">${escapeHtml(t("codex.agentsPath.global"))}</span>`;
    }
    if (entry.category === "project-root") {
      const name = entry.projectName || "?";
      return `<span class="codex-path-chip codex-path-chip-project-root">${escapeHtml(name)}</span>`;
    }
    // subdir → 项目名(绿) + 子目录路径(橙)
    const project = entry.projectName || "?";
    const subdir = entry.subdirPath || "?";
    return `<span class="codex-path-chip codex-path-chip-project-root">${escapeHtml(project)}</span><span class="codex-path-chip codex-path-chip-subdir">${escapeHtml(subdir)}</span>`;
  }

  /** 缓存当前 picker 显示的 entries(供 toggle 渲染 + change 处理用)*/
  let codexAgentsEntriesCache = [];

  /** 渲染当前选中条目到 toggle button 内 */
  function codexAgentsRenderToggle() {
    const cur = $("#codexAgentsPathPicker .codex-path-picker-current");
    if (!cur) return;
    const entry = codexAgentsEntriesCache.find((e) => e.hash === currentAgentsHash);
    if (!entry) {
      cur.innerHTML = `<span class="codex-path-empty">${escapeHtml(t("codex.agentsPathEmpty"))}</span>`;
      return;
    }
    cur.innerHTML = `${codexAgentsChipsHtml(entry)}<span class="codex-path-text" title="${escapeHtml(entry.path)}">${escapeHtml(entry.path)}</span>`;
  }

  /** 渲染下拉菜单 ul li 列表 */
  function codexAgentsRenderMenu() {
    const menu = $("#codexAgentsPathMenu");
    if (!menu) return;
    if (codexAgentsEntriesCache.length === 0) {
      menu.innerHTML = `<li class="codex-path-picker-item" aria-disabled="true"><span class="codex-path-empty">${escapeHtml(t("codex.agentsPathEmpty"))}</span></li>`;
      return;
    }
    menu.innerHTML = codexAgentsEntriesCache
      .map((e) => {
        const selected = e.hash === currentAgentsHash ? " selected" : "";
        return `<li class="codex-path-picker-item${selected}" role="option" data-hash="${escapeHtml(e.hash)}" data-category="${escapeHtml(e.category)}">
          ${codexAgentsChipsHtml(e)}
          <span class="codex-path-text" title="${escapeHtml(e.path)}">${escapeHtml(e.path)}</span>
        </li>`;
      })
      .join("");
  }

  /** 调后端 /paths 拉列表 + 刷 picker UI */
  async function codexAgentsReloadPaths() {
    try {
      const r = await fetch("/api/codex/agents-md/paths");
      if (!r.ok) throw new Error("paths request failed");
      const j = await r.json();
      codexAgentsEntriesCache = j.entries || [];
      // 保留当前选中,若不在新 list 则回退到第一条(可能空)
      if (
        !currentAgentsHash ||
        !codexAgentsEntriesCache.some((e) => e.hash === currentAgentsHash)
      ) {
        currentAgentsHash = codexAgentsEntriesCache[0]?.hash || null;
      }
      codexAgentsRenderToggle();
      codexAgentsRenderMenu();
      // 删除按钮:仅在非全局选中时显示
      const removeBtn = $("#codexAgentsPathRemoveBtn");
      if (removeBtn) {
        const cur = codexAgentsEntriesCache.find((e) => e.hash === currentAgentsHash);
        removeBtn.hidden = !cur || cur.category === "global";
      }
      return codexAgentsEntriesCache;
    } catch (e) {
      console.error("codexAgentsReloadPaths:", e);
      codexAgentsEntriesCache = [];
      codexAgentsRenderToggle();
      codexAgentsRenderMenu();
      return [];
    }
  }

  /** dropdown item click → 切换当前 hash + 刷新该 path 的 status/content */
  async function codexAgentsSelectHash(hash) {
    if (!hash) return;
    currentAgentsHash = hash;
    codexAgentsRenderToggle();
    codexAgentsRenderMenu();
    const removeBtn = $("#codexAgentsPathRemoveBtn");
    if (removeBtn) {
      const cur = codexAgentsEntriesCache.find((e) => e.hash === hash);
      removeBtn.hidden = !cur || cur.category === "global";
    }
    codexAgentsClosePicker();
    try {
      await codexAgentsRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  /** picker toggle open/close + outside click */
  function codexAgentsOpenPicker() {
    const picker = $("#codexAgentsPathPicker");
    const menu = $("#codexAgentsPathMenu");
    if (!picker || !menu) return;
    if (codexAgentsEntriesCache.length === 0) return; // 空态 toggle 不开
    picker.classList.add("open");
    menu.hidden = false;
  }
  function codexAgentsClosePicker() {
    const picker = $("#codexAgentsPathPicker");
    const menu = $("#codexAgentsPathMenu");
    if (!picker || !menu) return;
    picker.classList.remove("open");
    menu.hidden = true;
  }
  function codexAgentsTogglePicker() {
    const picker = $("#codexAgentsPathPicker");
    if (!picker) return;
    if (picker.classList.contains("open")) codexAgentsClosePicker();
    else codexAgentsOpenPicker();
  }

  /** 添加按钮 → inline modal(替代 window.prompt)。resource 默认 agents */
  function codexAgentsOnPathAdd(resource = "agents") {
    const modal = $("#codexAddPathModal");
    const input = $("#codexAddPathInput");
    if (!modal || !input) return;
    codexDocActiveResource = resource;
    input.value = "";
    // 切换 title / desc 文案到对应 resource
    const titleEl = $("#codexAddPathModalTitle");
    const descEl = $("#codexAddPathModal .codex-modal-desc");
    const placeholder = resource === "memories" ? "/path/to/project-root" : "/path/to/AGENTS.md";
    if (titleEl) {
      titleEl.textContent = t(
        resource === "memories" ? "codex.memoriesPathAddTitle" : "codex.agentsPathAddTitle",
      );
    }
    if (descEl) {
      descEl.textContent = t(
        resource === "memories" ? "codex.memoriesPathAddPrompt" : "codex.agentsPathAddPrompt",
      );
    }
    input.placeholder = placeholder;
    modal.hidden = false;
    setTimeout(() => input.focus(), 50);
  }

  function codexAgentsClosePathModal() {
    const modal = $("#codexAddPathModal");
    if (modal) modal.hidden = true;
  }

  /** 浏览按钮:打开 Tauri file/dir dialog。Memories tab 选**目录**,Agents 选 .md 文件。*/
  async function codexAgentsOnBrowse() {
    try {
      const dialog = window.__TAURI__?.dialog;
      if (!dialog || typeof dialog.open !== "function") {
        showToast("Tauri dialog API 不可用 — 请直接粘贴绝对路径");
        return;
      }
      const input = $("#codexAddPathInput");
      const raw = (input?.value || "").trim();
      const defaultPath = raw && raw.startsWith("/") ? raw : undefined;
      const isMemories = codexDocActiveResource === "memories";
      const selected = await dialog.open({
        title: t(isMemories ? "codex.memoriesPathAddTitle" : "codex.agentsPathAddTitle"),
        multiple: false,
        directory: isMemories, // memories 选目录,agents 选文件
        defaultPath,
        filters: isMemories
          ? undefined
          : [
              { name: "AGENTS.md", extensions: ["md", "MD"] },
              { name: "All files", extensions: ["*"] },
            ],
      });
      if (typeof selected === "string" && selected) {
        if (input) input.value = selected;
      }
    } catch (e) {
      console.error("dialog open:", e);
      showToast(e.message || "dialog open failed");
    }
  }

  async function codexAgentsConfirmPathAdd() {
    const input = $("#codexAddPathInput");
    const raw = input?.value || "";
    const path = raw.trim();
    if (!path) {
      showToast(t("codex.agentsPathAddEmpty"));
      return;
    }
    const resource = codexDocActiveResource;
    try {
      const r = await fetch(`${codexDocApiBase(resource)}/paths/add`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "add path failed");
      }
      const j = await r.json();
      codexDocSetCurrentHash(resource, j.entry?.hash || null);
      codexAgentsClosePathModal();
      if (resource === "memories") {
        await codexMemoriesReloadPaths();
        await codexMemoriesRawLoadAndRender();
      } else {
        await codexAgentsReloadPaths();
        await codexAgentsRawLoadAndRender();
      }
      showToast(t("codex.agentsPathAddOk"));
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  /** 删除按钮 → POST /paths/remove(全局路径按钮自动隐藏)*/
  async function codexAgentsOnPathRemove() {
    if (!currentAgentsHash) return;
    if (!confirm(t("codex.agentsPathRemoveConfirm"))) return;
    try {
      const r = await fetch("/api/codex/agents-md/paths/remove", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ hash: currentAgentsHash }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "remove path failed");
      }
      currentAgentsHash = null; // 回退到全局
      await codexAgentsReloadPaths();
      await codexAgentsRawLoadAndRender();
      showToast(t("codex.agentsPathRemoveOk"));
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  // ── Agents raw mode(Preview/Edit/Backup/History)──

  /** UI mode 状态:"preview" / "edit" */
  let codexAgentsMode = "preview";
  /** Preview 模式下缓存的原内容(用于 Edit→Cancel 回退)*/
  let codexAgentsLastFullContent = "";

  /** 加载 raw 全文 → 写入 preview pre */
  async function codexAgentsRawLoadAndRender() {
    const pre = $("#codexAgentsPreview");
    const ta = $("#codexAgentsEdit");
    if (!pre || !ta) return;
    // 切换路径或重新加载时强制回 preview 模式
    codexAgentsSwitchMode("preview");
    if (!currentAgentsHash) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", t("codex.agentsPathEmpty"));
      ta.value = "";
      codexAgentsLastFullContent = "";
      return;
    }
    try {
      const r = await fetch(`/api/codex/agents-md/raw?hash=${encodeURIComponent(currentAgentsHash)}`);
      if (!r.ok) throw new Error("raw fetch failed");
      const j = await r.json();
      codexAgentsLastFullContent = j.content || "";
      pre.classList.add("codex-md-rendered");
      pre.innerHTML = renderMiniMd(codexAgentsLastFullContent);
      pre.removeAttribute("data-empty-hint");
      ta.value = codexAgentsLastFullContent;
    } catch (e) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", `读取失败: ${e.message || e}`);
    }
  }

  /** 切换 mode("preview" / "edit"),同步按钮 + pre/textarea 显示 */
  function codexAgentsSwitchMode(mode) {
    codexAgentsMode = mode;
    const pre = $("#codexAgentsPreview");
    const ta = $("#codexAgentsEdit");
    const editBtn = $("#codexAgentsEditBtn");
    const backupBtn = $("#codexAgentsBackupBtn");
    const applyBtn = $("#codexAgentsApplyBtn");
    const cancelBtn = $("#codexAgentsCancelBtn");
    if (mode === "edit") {
      if (pre) pre.hidden = true;
      if (ta) {
        ta.hidden = false;
        ta.value = codexAgentsLastFullContent;
      }
      if (editBtn) editBtn.hidden = true;
      if (backupBtn) backupBtn.hidden = true;
      if (applyBtn) applyBtn.hidden = false;
      if (cancelBtn) cancelBtn.hidden = false;
    } else {
      if (pre) pre.hidden = false;
      if (ta) ta.hidden = true;
      if (editBtn) editBtn.hidden = false;
      if (backupBtn) backupBtn.hidden = false;
      if (applyBtn) applyBtn.hidden = true;
      if (cancelBtn) cancelBtn.hidden = true;
    }
  }

  async function codexAgentsOnEditStart() {
    if (!currentAgentsHash) {
      showToast(t("codex.agentsPathEmpty"));
      return;
    }
    codexAgentsSwitchMode("edit");
    setTimeout(() => $("#codexAgentsEdit")?.focus(), 50);
  }

  async function codexAgentsOnApply() {
    if (!currentAgentsHash) return;
    const ta = $("#codexAgentsEdit");
    const content = ta?.value ?? "";
    try {
      const r = await fetch(`/api/codex/agents-md/raw?hash=${encodeURIComponent(currentAgentsHash)}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "write failed");
      }
      showToast(t("codex.agentsApplyOk"));
      await codexAgentsRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  function codexAgentsOnCancel() {
    codexAgentsSwitchMode("preview");
  }

  async function codexAgentsOnBackup() {
    if (!currentAgentsHash) {
      showToast(t("codex.agentsPathEmpty"));
      return;
    }
    try {
      const r = await fetch(`/api/codex/agents-md/backup?hash=${encodeURIComponent(currentAgentsHash)}`, {
        method: "POST",
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "backup failed");
      }
      showToast(t("codex.agentsBackupOk"));
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  // ── History 大 modal:picker + 应用 + diff preview ──

  /** history entries 缓存(reversed,最新在前)+ 当前选中 index */
  let codexHistoryEntries = [];
  let codexHistorySelectedIdx = null;

  /** LCS-based line diff(O(m*n) — 5K 行 OK)
   * 返回 [{ type: "ctx"|"add"|"del", text }]
   */
  function codexLineDiff(oldText, newText) {
    const oldLines = oldText.split("\n");
    const newLines = newText.split("\n");
    const m = oldLines.length;
    const n = newLines.length;
    // dp[i][j] = LCS length for oldLines[0..i], newLines[0..j]
    const dp = Array.from({ length: m + 1 }, () => new Array(n + 1).fill(0));
    for (let i = 0; i < m; i++) {
      for (let j = 0; j < n; j++) {
        if (oldLines[i] === newLines[j]) dp[i + 1][j + 1] = dp[i][j] + 1;
        else dp[i + 1][j + 1] = Math.max(dp[i + 1][j], dp[i][j + 1]);
      }
    }
    const result = [];
    let i = m, j = n;
    while (i > 0 || j > 0) {
      if (i > 0 && j > 0 && oldLines[i - 1] === newLines[j - 1]) {
        result.unshift({ type: "ctx", text: oldLines[i - 1] });
        i--; j--;
      } else if (j > 0 && (i === 0 || dp[i][j - 1] >= dp[i - 1][j])) {
        result.unshift({ type: "add", text: newLines[j - 1] });
        j--;
      } else {
        result.unshift({ type: "del", text: oldLines[i - 1] });
        i--;
      }
    }
    return result;
  }

  /** entry label:"项目名 / 子目录路径 · YYYY-MM-DD HH:MM:SS" — 项目 / 子目录从当前 active resource 的 path 推断 */
  function codexHistoryEntryLabel(entry) {
    const resource = codexDocActiveResource;
    const cache =
      resource === "memories" ? codexMemoriesEntriesCache :
      resource === "skills" ? codexSkillsEntriesCache : codexAgentsEntriesCache;
    const hash =
      resource === "memories" ? currentMemoriesHash :
      resource === "skills" ? currentSkillsHash : currentAgentsHash;
    const cur = cache.find((e) => e.hash === hash);
    const ts = new Date(entry.timestamp * 1000).toLocaleString();
    let prefix = "";
    if (resource === "mcp") {
      prefix = "config.toml";
    } else if (cur) {
      if (resource === "skills") {
        prefix = cur.name || "?";
      } else if (cur.category === "global") {
        prefix = t("codex.agentsPath.global");
      } else if (cur.category === "project-root") {
        prefix = cur.projectName || "?";
      } else {
        prefix = `${cur.projectName || "?"} / ${cur.subdirPath || "?"}`;
      }
    }
    return prefix ? `${prefix} · ${ts}` : ts;
  }

  /** render history picker toggle button(selected entry)*/
  function codexHistoryRenderToggle() {
    const cur = $("#codexHistoryPicker .codex-path-picker-current");
    if (!cur) return;
    if (codexHistorySelectedIdx == null || !codexHistoryEntries[codexHistorySelectedIdx]) {
      cur.innerHTML = `<span class="codex-path-empty">${escapeHtml(t("codex.historyEmpty"))}</span>`;
      return;
    }
    const entry = codexHistoryEntries[codexHistorySelectedIdx];
    cur.innerHTML = `<span class="codex-path-text" title="${escapeHtml(codexHistoryEntryLabel(entry))}">${escapeHtml(codexHistoryEntryLabel(entry))}</span>`;
  }

  /** render history picker dropdown menu */
  function codexHistoryRenderMenu() {
    const menu = $("#codexHistoryMenu");
    if (!menu) return;
    if (codexHistoryEntries.length === 0) {
      menu.innerHTML = `<li class="codex-path-picker-item" aria-disabled="true"><span class="codex-path-empty">${escapeHtml(t("codex.historyEmpty"))}</span></li>`;
      return;
    }
    menu.innerHTML = codexHistoryEntries
      .map((entry, i) => {
        const selected = i === codexHistorySelectedIdx ? " selected" : "";
        return `<li class="codex-path-picker-item${selected}" role="option" data-history-idx="${i}">
          <span class="codex-path-text">${escapeHtml(codexHistoryEntryLabel(entry))}</span>
        </li>`;
      })
      .join("");
  }

  /** 渲染当前选中 history 对比 file 当前内容的 diff */
  function codexHistoryRenderDiff() {
    const pre = $("#codexHistoryDiff");
    if (!pre) return;
    if (codexHistorySelectedIdx == null || !codexHistoryEntries[codexHistorySelectedIdx]) {
      pre.innerHTML = "";
      pre.setAttribute("data-empty-hint", t("codex.historyDiffEmpty"));
      return;
    }
    const entry = codexHistoryEntries[codexHistorySelectedIdx];
    const newContent = entry.appliedContent || entry.managedContent || "";
    const oldContent =
      codexDocActiveResource === "memories"
        ? codexMemoriesLastFullContent || ""
        : codexDocActiveResource === "skills"
        ? codexSkillsLastFullContent || ""
        : codexAgentsLastFullContent || "";
    const diff = codexLineDiff(oldContent, newContent);
    pre.removeAttribute("data-empty-hint");
    pre.innerHTML = diff
      .map((d) => `<span class="codex-diff-line ${d.type}">${escapeHtml(d.text) || "&nbsp;"}</span>`)
      .join("");
  }

  function codexHistoryPickerToggle() {
    const picker = $("#codexHistoryPicker");
    const menu = $("#codexHistoryMenu");
    if (!picker || !menu) return;
    if (codexHistoryEntries.length === 0) return;
    const open = picker.classList.toggle("open");
    menu.hidden = !open;
  }

  function codexHistoryPickerClose() {
    const picker = $("#codexHistoryPicker");
    const menu = $("#codexHistoryMenu");
    if (!picker || !menu) return;
    picker.classList.remove("open");
    menu.hidden = true;
  }

  async function codexHistoryOpen() {
    const resource = codexDocActiveResource;
    const hash = codexDocCurrentHash(resource);
    if (!hash) {
      showToast(
        t(
          resource === "memories"
            ? "codex.memoriesPathEmpty"
            : resource === "skills"
            ? "codex.skillsEmpty"
            : "codex.agentsPathEmpty",
        ),
      );
      return;
    }
    try {
      const r = await fetch(`${codexDocApiBase(resource)}/history?hash=${encodeURIComponent(hash)}`);
      if (!r.ok) throw new Error("history failed");
      const j = await r.json();
      const entries = j.history || [];
      codexHistoryEntries = entries.slice().reverse();
      codexHistorySelectedIdx = codexHistoryEntries.length > 0 ? 0 : null;
      codexHistoryRenderToggle();
      codexHistoryRenderMenu();
      codexHistoryRenderDiff();
      const modal = $("#codexHistoryModal");
      if (modal) modal.hidden = false;
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  function codexHistoryClose() {
    const modal = $("#codexHistoryModal");
    if (modal) modal.hidden = true;
    codexHistoryPickerClose();
  }

  function codexHistorySelect(idx) {
    if (idx < 0 || idx >= codexHistoryEntries.length) return;
    codexHistorySelectedIdx = idx;
    codexHistoryRenderToggle();
    codexHistoryRenderMenu();
    codexHistoryRenderDiff();
    codexHistoryPickerClose();
  }

  async function codexHistoryRestore() {
    if (codexHistorySelectedIdx == null || !codexHistoryEntries[codexHistorySelectedIdx]) {
      showToast(t("codex.historyEmpty"));
      return;
    }
    const entry = codexHistoryEntries[codexHistorySelectedIdx];
    if (!confirm(t("codex.agentsRestoreConfirm"))) return;
    const resource = codexDocActiveResource;
    // MCP 走独立 endpoint(无 hash,操作整个 config.toml)
    if (resource === "mcp") {
      try {
        const r = await fetch("/api/codex/mcp/servers/restore", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ index: entry.index }),
        });
        if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "restore failed"); }
        showToast(t("codex.agentsRestoreOk"));
        codexHistoryClose();
        await codexMcpReloadServers();
      } catch (e) { showToast(e.message || t("toast.requestFailed")); }
      return;
    }
    const hash = codexDocCurrentHash(resource);
    if (!hash) return;
    try {
      const r = await fetch(`${codexDocApiBase(resource)}/restore-raw?hash=${encodeURIComponent(hash)}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ index: entry.index }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "restore failed");
      }
      showToast(t("codex.agentsRestoreOk"));
      codexHistoryClose();
      if (resource === "memories") await codexMemoriesRawLoadAndRender();
      else if (resource === "skills") await codexSkillsRawLoadAndRender();
      else if (resource === "mcp") await codexMcpReloadServers();
      else await codexAgentsRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  // 旧 toggle 函数名保持给 action 用 — 等价于 open
  async function codexAgentsToggleHistory() {
    codexDocActiveResource = "agents";
    await codexHistoryOpen();
  }

  // ── Memories 完全镜像 Agents,但用 currentMemoriesHash + memories endpoints ──

  let codexMemoriesEntriesCache = [];
  let codexMemoriesMode = "preview";
  let codexMemoriesLastFullContent = "";

  /** Memories 用 2 固定 entry,chip 按文件名分色:MEMORY.md 蓝(主索引)/ summary 绿 */
  function codexMemoriesChipsHtml(entry) {
    const filename = entry.path.split("/").pop() || "";
    if (filename === "MEMORY.md") {
      return `<span class="codex-path-chip codex-path-chip-global">${escapeHtml(t("codex.memoriesPath.index"))}</span>`;
    }
    if (filename === "memory_summary.md") {
      return `<span class="codex-path-chip codex-path-chip-project-root">${escapeHtml(t("codex.memoriesPath.summary"))}</span>`;
    }
    return `<span class="codex-path-chip codex-path-chip-project-root">${escapeHtml(filename)}</span>`;
  }

  function codexMemoriesRenderToggle() {
    const cur = $("#codexMemoriesPathPicker .codex-path-picker-current");
    if (!cur) return;
    const entry = codexMemoriesEntriesCache.find((e) => e.hash === currentMemoriesHash);
    if (!entry) {
      cur.innerHTML = `<span class="codex-path-empty">${escapeHtml(t("codex.memoriesPathEmpty"))}</span>`;
      return;
    }
    cur.innerHTML = `${codexMemoriesChipsHtml(entry)}<span class="codex-path-text" title="${escapeHtml(entry.path)}">${escapeHtml(entry.path)}</span>`;
  }

  function codexMemoriesRenderMenu() {
    const menu = $("#codexMemoriesPathMenu");
    if (!menu) return;
    if (codexMemoriesEntriesCache.length === 0) {
      menu.innerHTML = `<li class="codex-path-picker-item" aria-disabled="true"><span class="codex-path-empty">${escapeHtml(t("codex.memoriesPathEmpty"))}</span></li>`;
      return;
    }
    menu.innerHTML = codexMemoriesEntriesCache
      .map((e) => {
        const selected = e.hash === currentMemoriesHash ? " selected" : "";
        return `<li class="codex-path-picker-item${selected}" role="option" data-hash="${escapeHtml(e.hash)}" data-category="${escapeHtml(e.category)}">
          ${codexMemoriesChipsHtml(e)}
          <span class="codex-path-text" title="${escapeHtml(e.path)}">${escapeHtml(e.path)}</span>
        </li>`;
      })
      .join("");
  }

  async function codexMemoriesReloadPaths() {
    try {
      const r = await fetch("/api/codex/memories-md/paths");
      if (!r.ok) throw new Error("memories paths request failed");
      const j = await r.json();
      codexMemoriesEntriesCache = j.entries || [];
      if (
        !currentMemoriesHash ||
        !codexMemoriesEntriesCache.some((e) => e.hash === currentMemoriesHash)
      ) {
        currentMemoriesHash = codexMemoriesEntriesCache[0]?.hash || null;
      }
      codexMemoriesRenderToggle();
      codexMemoriesRenderMenu();
      return codexMemoriesEntriesCache;
    } catch (e) {
      console.error("codexMemoriesReloadPaths:", e);
      codexMemoriesEntriesCache = [];
      codexMemoriesRenderToggle();
      codexMemoriesRenderMenu();
      return [];
    }
  }

  async function codexMemoriesSelectHash(hash) {
    if (!hash) return;
    currentMemoriesHash = hash;
    codexMemoriesRenderToggle();
    codexMemoriesRenderMenu();
    codexMemoriesClosePicker();
    try {
      await codexMemoriesRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  function codexMemoriesOpenPicker() {
    const picker = $("#codexMemoriesPathPicker");
    const menu = $("#codexMemoriesPathMenu");
    if (!picker || !menu) return;
    if (codexMemoriesEntriesCache.length === 0) return;
    picker.classList.add("open");
    menu.hidden = false;
  }
  function codexMemoriesClosePicker() {
    const picker = $("#codexMemoriesPathPicker");
    const menu = $("#codexMemoriesPathMenu");
    if (!picker || !menu) return;
    picker.classList.remove("open");
    menu.hidden = true;
  }
  function codexMemoriesTogglePicker() {
    const picker = $("#codexMemoriesPathPicker");
    if (!picker) return;
    if (picker.classList.contains("open")) codexMemoriesClosePicker();
    else codexMemoriesOpenPicker();
  }

  async function codexMemoriesOnPathRemove() {
    if (!currentMemoriesHash) return;
    if (!confirm(t("codex.agentsPathRemoveConfirm"))) return;
    try {
      const r = await fetch("/api/codex/memories-md/paths/remove", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ hash: currentMemoriesHash }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "remove path failed");
      }
      currentMemoriesHash = null;
      await codexMemoriesReloadPaths();
      await codexMemoriesRawLoadAndRender();
      showToast(t("codex.agentsPathRemoveOk"));
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  async function codexMemoriesRawLoadAndRender() {
    const pre = $("#codexMemoriesPreview");
    const ta = $("#codexMemoriesEdit");
    if (!pre || !ta) return;
    codexMemoriesSwitchMode("preview");
    if (!currentMemoriesHash) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", t("codex.memoriesLoading"));
      ta.value = "";
      codexMemoriesLastFullContent = "";
      return;
    }
    try {
      const r = await fetch(`/api/codex/memories-md/raw?hash=${encodeURIComponent(currentMemoriesHash)}`);
      if (!r.ok) throw new Error("raw fetch failed");
      const j = await r.json();
      codexMemoriesLastFullContent = j.content || "";
      pre.classList.add("codex-md-rendered");
      pre.innerHTML = renderMiniMd(codexMemoriesLastFullContent);
      pre.removeAttribute("data-empty-hint");
      ta.value = codexMemoriesLastFullContent;
    } catch (e) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", `读取失败: ${e.message || e}`);
    }
  }

  function codexMemoriesSwitchMode(mode) {
    codexMemoriesMode = mode;
    const pre = $("#codexMemoriesPreview");
    const ta = $("#codexMemoriesEdit");
    const editBtn = $("#codexMemoriesEditBtn");
    const backupBtn = $("#codexMemoriesBackupBtn");
    const applyBtn = $("#codexMemoriesApplyBtn");
    const cancelBtn = $("#codexMemoriesCancelBtn");
    if (mode === "edit") {
      if (pre) pre.hidden = true;
      if (ta) {
        ta.hidden = false;
        ta.value = codexMemoriesLastFullContent;
      }
      if (editBtn) editBtn.hidden = true;
      if (backupBtn) backupBtn.hidden = true;
      if (applyBtn) applyBtn.hidden = false;
      if (cancelBtn) cancelBtn.hidden = false;
    } else {
      if (pre) pre.hidden = false;
      if (ta) ta.hidden = true;
      if (editBtn) editBtn.hidden = false;
      if (backupBtn) backupBtn.hidden = false;
      if (applyBtn) applyBtn.hidden = true;
      if (cancelBtn) cancelBtn.hidden = true;
    }
  }

  async function codexMemoriesOnEditStart() {
    if (!currentMemoriesHash) {
      showToast(t("codex.memoriesPathEmpty"));
      return;
    }
    codexMemoriesSwitchMode("edit");
    setTimeout(() => $("#codexMemoriesEdit")?.focus(), 50);
  }

  async function codexMemoriesOnApply() {
    if (!currentMemoriesHash) return;
    const ta = $("#codexMemoriesEdit");
    const content = ta?.value ?? "";
    try {
      const r = await fetch(`/api/codex/memories-md/raw?hash=${encodeURIComponent(currentMemoriesHash)}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content }),
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "write failed");
      }
      showToast(t("codex.agentsApplyOk"));
      await codexMemoriesRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  function codexMemoriesOnCancel() {
    codexMemoriesSwitchMode("preview");
  }

  async function codexMemoriesOnBackup() {
    if (!currentMemoriesHash) {
      showToast(t("codex.memoriesPathEmpty"));
      return;
    }
    try {
      const r = await fetch(`/api/codex/memories-md/backup?hash=${encodeURIComponent(currentMemoriesHash)}`, {
        method: "POST",
      });
      if (!r.ok) {
        const err = await r.json().catch(() => ({}));
        throw new Error(err.error || "backup failed");
      }
      showToast(t("codex.agentsBackupOk"));
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  async function codexMemoriesToggleHistory() {
    codexDocActiveResource = "memories";
    await codexHistoryOpen();
  }

  // ── Skills 镜像 Agents/Memories,扫 ~/.codex/skills/<name>/SKILL.md ──

  let codexSkillsEntriesCache = [];
  let codexSkillsMode = "preview";
  let codexSkillsLastFullContent = "";

  /** skill chip:只显 skill 名(绿)*/
  function codexSkillsChipsHtml(entry) {
    return `<span class="codex-path-chip codex-path-chip-project-root">${escapeHtml(entry.name)}</span>`;
  }

  function codexSkillsRenderToggle() {
    const cur = $("#codexSkillsPathPicker .codex-path-picker-current");
    if (!cur) return;
    const entry = codexSkillsEntriesCache.find((e) => e.hash === currentSkillsHash);
    if (!entry) {
      cur.innerHTML = `<span class="codex-path-empty">${escapeHtml(t("codex.skillsEmpty"))}</span>`;
      return;
    }
    cur.innerHTML = `${codexSkillsChipsHtml(entry)}<span class="codex-path-text" title="${escapeHtml(entry.path)}">${escapeHtml(entry.path)}</span>`;
  }

  function codexSkillsRenderMenu() {
    const menu = $("#codexSkillsPathMenu");
    if (!menu) return;
    if (codexSkillsEntriesCache.length === 0) {
      menu.innerHTML = `<li class="codex-path-picker-item" aria-disabled="true"><span class="codex-path-empty">${escapeHtml(t("codex.skillsEmpty"))}</span></li>`;
      return;
    }
    menu.innerHTML = codexSkillsEntriesCache
      .map((e) => {
        const selected = e.hash === currentSkillsHash ? " selected" : "";
        return `<li class="codex-path-picker-item${selected}" role="option" data-hash="${escapeHtml(e.hash)}">
          ${codexSkillsChipsHtml(e)}
          <span class="codex-path-text" title="${escapeHtml(e.path)}">${escapeHtml(e.path)}</span>
        </li>`;
      })
      .join("");
  }

  async function codexSkillsReloadPaths() {
    try {
      const r = await fetch("/api/codex/skills-md/paths");
      if (!r.ok) throw new Error("skills paths request failed");
      const j = await r.json();
      codexSkillsEntriesCache = j.entries || [];
      if (
        !currentSkillsHash ||
        !codexSkillsEntriesCache.some((e) => e.hash === currentSkillsHash)
      ) {
        currentSkillsHash = codexSkillsEntriesCache[0]?.hash || null;
      }
      codexSkillsRenderToggle();
      codexSkillsRenderMenu();
      return codexSkillsEntriesCache;
    } catch (e) {
      console.error("codexSkillsReloadPaths:", e);
      codexSkillsEntriesCache = [];
      codexSkillsRenderToggle();
      codexSkillsRenderMenu();
      return [];
    }
  }

  async function codexSkillsSelectHash(hash) {
    if (!hash) return;
    currentSkillsHash = hash;
    codexSkillsRenderToggle();
    codexSkillsRenderMenu();
    codexSkillsClosePicker();
    try {
      await codexSkillsRawLoadAndRender();
    } catch (e) {
      showToast(e.message || t("toast.requestFailed"));
    }
  }

  function codexSkillsOpenPicker() {
    const picker = $("#codexSkillsPathPicker");
    const menu = $("#codexSkillsPathMenu");
    if (!picker || !menu) return;
    if (codexSkillsEntriesCache.length === 0) return;
    picker.classList.add("open");
    menu.hidden = false;
  }
  function codexSkillsClosePicker() {
    const picker = $("#codexSkillsPathPicker");
    const menu = $("#codexSkillsPathMenu");
    if (!picker || !menu) return;
    picker.classList.remove("open");
    menu.hidden = true;
  }
  function codexSkillsTogglePicker() {
    const picker = $("#codexSkillsPathPicker");
    if (!picker) return;
    if (picker.classList.contains("open")) codexSkillsClosePicker();
    else codexSkillsOpenPicker();
  }

  async function codexSkillsRawLoadAndRender() {
    const pre = $("#codexSkillsPreview");
    const ta = $("#codexSkillsEdit");
    if (!pre || !ta) return;
    codexSkillsSwitchMode("preview");
    if (!currentSkillsHash) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", t("codex.skillsEmpty"));
      ta.value = "";
      codexSkillsLastFullContent = "";
      return;
    }
    try {
      const r = await fetch(`/api/codex/skills-md/raw?hash=${encodeURIComponent(currentSkillsHash)}`);
      if (!r.ok) throw new Error("raw fetch failed");
      const j = await r.json();
      codexSkillsLastFullContent = j.content || "";
      pre.classList.add("codex-md-rendered");
      pre.innerHTML = renderMiniMd(codexSkillsLastFullContent);
      pre.removeAttribute("data-empty-hint");
      ta.value = codexSkillsLastFullContent;
    } catch (e) {
      pre.classList.remove("codex-md-rendered");
      pre.textContent = "";
      pre.setAttribute("data-empty-hint", `读取失败: ${e.message || e}`);
    }
  }

  function codexSkillsSwitchMode(mode) {
    codexSkillsMode = mode;
    const pre = $("#codexSkillsPreview");
    const ta = $("#codexSkillsEdit");
    const editBtn = $("#codexSkillsEditBtn");
    const backupBtn = $("#codexSkillsBackupBtn");
    const applyBtn = $("#codexSkillsApplyBtn");
    const cancelBtn = $("#codexSkillsCancelBtn");
    if (mode === "edit") {
      if (pre) pre.hidden = true;
      if (ta) { ta.hidden = false; ta.value = codexSkillsLastFullContent; }
      if (editBtn) editBtn.hidden = true;
      if (backupBtn) backupBtn.hidden = true;
      if (applyBtn) applyBtn.hidden = false;
      if (cancelBtn) cancelBtn.hidden = false;
    } else {
      if (pre) pre.hidden = false;
      if (ta) ta.hidden = true;
      if (editBtn) editBtn.hidden = false;
      if (backupBtn) backupBtn.hidden = false;
      if (applyBtn) applyBtn.hidden = true;
      if (cancelBtn) cancelBtn.hidden = true;
    }
  }

  async function codexSkillsOnEditStart() {
    if (!currentSkillsHash) { showToast(t("codex.skillsEmpty")); return; }
    codexSkillsSwitchMode("edit");
    setTimeout(() => $("#codexSkillsEdit")?.focus(), 50);
  }

  async function codexSkillsOnApply() {
    if (!currentSkillsHash) return;
    const ta = $("#codexSkillsEdit");
    const content = ta?.value ?? "";
    try {
      const r = await fetch(`/api/codex/skills-md/raw?hash=${encodeURIComponent(currentSkillsHash)}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content }),
      });
      if (!r.ok) { const err = await r.json().catch(() => ({})); throw new Error(err.error || "write failed"); }
      showToast(t("codex.agentsApplyOk"));
      await codexSkillsRawLoadAndRender();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  function codexSkillsOnCancel() { codexSkillsSwitchMode("preview"); }

  async function codexSkillsOnBackup() {
    if (!currentSkillsHash) { showToast(t("codex.skillsEmpty")); return; }
    try {
      const r = await fetch(`/api/codex/skills-md/backup?hash=${encodeURIComponent(currentSkillsHash)}`, { method: "POST" });
      if (!r.ok) { const err = await r.json().catch(() => ({})); throw new Error(err.error || "backup failed"); }
      showToast(t("codex.agentsBackupOk"));
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexSkillsToggleHistory() {
    codexDocActiveResource = "skills";
    await codexHistoryOpen();
  }

  /** 打开当前 skill 所在目录(macOS:open / Linux:xdg-open / Windows:explorer)*/
  async function codexSkillsOnReveal() {
    if (!currentSkillsHash) { showToast(t("codex.skillsEmpty")); return; }
    try {
      const r = await fetch(`/api/codex/skills-md/reveal?hash=${encodeURIComponent(currentSkillsHash)}`, { method: "POST" });
      if (!r.ok) { const err = await r.json().catch(() => ({})); throw new Error(err.error || "open dir failed"); }
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  // ── MCP tab: Servers form + Plugins + Marketplace + Deeplink ──

  let codexMcpCurrentSubpane = "servers";
  let codexMcpServersCache = [];
  let codexMcpCurrentServerName = null;
  let codexMcpFormDirty = false;
  let codexMcpPluginsCache = [];
  let codexMcpSourcesCache = [];
  let codexMcpMarketIndex = { servers: [], plugins: [], errors: {} };
  let codexMcpMarketFilter = "";
  let codexMcpRawSnapshot = "";
  let codexMcpPendingDeeplink = null;

  function codexMcpSetSubpaneVisible(sub) {
    codexMcpCurrentSubpane = sub;
    $all("#codexMcpSubnav .codex-mcp-subnav-item").forEach((btn) => {
      btn.classList.toggle("active", btn.dataset.mcpSub === sub);
    });
    $all('#codexMcpTab .codex-mcp-subpane').forEach((pane) => {
      const match = pane.dataset.mcpSubPane === sub;
      pane.hidden = !match;
      pane.classList.toggle("active", match);
    });
    const rawWrap = $("#codexMcpRawWrap");
    if (rawWrap && sub !== "servers") rawWrap.hidden = true;
  }

  async function codexMcpOpenSubpane(sub) {
    codexMcpSetSubpaneVisible(sub);
    if (sub === "servers") {
      await codexMcpReloadServers();
    } else if (sub === "plugins") {
      await codexMcpReloadPlugins();
    } else if (sub === "marketplace") {
      await codexMcpReloadSources();
      await codexMcpReloadMarketIndex(false);
    }
  }

  // ── Servers ──

  async function codexMcpReloadServers() {
    try {
      const r = await fetch("/api/codex/mcp/servers");
      if (!r.ok) throw new Error("list servers failed");
      const j = await r.json();
      codexMcpServersCache = j.servers || [];
      if (
        codexMcpCurrentServerName &&
        !codexMcpServersCache.some((s) => s.name === codexMcpCurrentServerName)
      ) {
        codexMcpCurrentServerName = null;
      }
      codexMcpRenderServersList();
      codexMcpRenderForm();
    } catch (e) {
      console.error("codexMcpReloadServers:", e);
      codexMcpServersCache = [];
      codexMcpRenderServersList();
    }
  }

  function codexMcpRenderServersList() {
    const wrap = $("#codexMcpServersList");
    if (!wrap) return;
    if (codexMcpServersCache.length === 0) {
      wrap.innerHTML = `<div class="codex-mcp-empty-form">${escapeHtml(t("codex.mcp.serversEmpty"))}</div>`;
      return;
    }
    wrap.innerHTML = codexMcpServersCache
      .map((s) => {
        const active = s.name === codexMcpCurrentServerName ? " active" : "";
        const disabled = s.enabled === false ? " disabled" : "";
        const chip = s.transport === "stdio"
          ? `<span class="codex-mcp-chip stdio">本机</span>`
          : `<span class="codex-mcp-chip http">远程</span>`;
        const offChip = s.enabled === false ? `<span class="codex-mcp-chip disabled">disabled</span>` : "";
        return `<div class="codex-mcp-list-item${active}${disabled}" data-server="${escapeHtml(s.name)}">
          <div class="codex-mcp-list-item-name">${chip}${offChip}${escapeHtml(s.name)}</div>
        </div>`;
      })
      .join("");
  }

  function codexMcpEmptyServerSpec() {
    return {
      name: "",
      transport: "stdio",
      command: "",
      args: [],
      env: {},
      cwd: null,
      url: null,
      bearerTokenEnvVar: null,
      httpHeaders: {},
      envHttpHeaders: {},
      enabled: true,
      required: false,
      supportsParallelToolCalls: false,
      experimentalEnvironment: null,
      startupTimeoutSec: null,
      toolTimeoutSec: null,
      defaultToolsApprovalMode: null,
      enabledTools: null,
      disabledTools: null,
      _isNew: true,
    };
  }

  /** JSON 编辑模式 — 当前选中 server / 新增的 JSON read-only pre + 编辑 textarea 切换 */
  let codexMcpJsonEditMode = false;
  let codexMcpJsonDraft = "";

  function codexMcpServerSpecToJsonText(spec) {
    // 清掉内部字段 _isNew + null/undefined,然后 JSON.stringify pretty
    const out = {};
    const skipKeys = new Set(["_isNew", "name", "disabledReason", "transport"]);
    // 保留 transport 字段在输出
    if (spec.transport) out.transport = spec.transport;
    for (const [k, v] of Object.entries(spec)) {
      if (skipKeys.has(k)) continue;
      if (v == null) continue;
      if (Array.isArray(v) && v.length === 0) continue;
      if (typeof v === "object" && !Array.isArray(v) && Object.keys(v).length === 0) continue;
      out[k] = v;
    }
    return JSON.stringify(out, null, 2);
  }

  function codexMcpRenderForm() {
    const wrap = $("#codexMcpServerForm");
    const editBtnText = $("#codexMcpEditBtnText");
    const editBtn = $("#codexMcpEditBtn");
    if (!wrap) return;
    let spec = null;
    let isNew = false;
    if (codexMcpCurrentServerName === "__new__") {
      spec = codexMcpEmptyServerSpec();
      isNew = true;
    } else if (codexMcpCurrentServerName) {
      spec = codexMcpServersCache.find((s) => s.name === codexMcpCurrentServerName);
    }
    if (!spec) {
      wrap.innerHTML = `<div class="codex-mcp-empty-form">从左侧列表选一个 server,或点底部「新增」</div>`;
      if (editBtn) editBtn.disabled = true;
      if (editBtnText) editBtnText.textContent = "编辑";
      codexMcpJsonEditMode = false;
      return;
    }
    if (editBtn) editBtn.disabled = false;
    const jsonText = codexMcpJsonEditMode && codexMcpJsonDraft
      ? codexMcpJsonDraft
      : codexMcpServerSpecToJsonText(spec);
    const title = `<div class="codex-mcp-json-header">
      <span class="codex-mcp-json-name">${escapeHtml(spec.name || "(新)")}</span>
      ${isNew ? "" : `<button class="btn-icon-only codex-mcp-json-delete" type="button" data-action="codex-mcp-server-delete" title="删除"><i class="bi bi-trash"></i></button>`}
    </div>`;
    wrap.innerHTML = `
      ${title}
      ${codexMcpJsonEditMode
        ? `<textarea class="form-control codex-mcp-json-area" id="codexMcpJsonTextarea" spellcheck="false">${escapeHtml(jsonText)}</textarea>
           <div id="codexMcpJsonError" class="codex-mcp-json-error" hidden></div>`
        : `<pre class="codex-mcp-json-pre" id="codexMcpJsonPre">${escapeHtml(jsonText)}</pre>`
      }
    `;
    if (editBtnText) editBtnText.textContent = codexMcpJsonEditMode ? (isNew ? "确认创建" : "保存") : "编辑";
    if (editBtn) {
      const icon = editBtn.querySelector("i");
      if (icon) icon.className = codexMcpJsonEditMode ? "bi bi-check2-circle" : "bi bi-pencil";
    }
  }

  /** args 列表 — 每个 1 个 input row,带删除按钮(legacy,JSON 模式不用) */
  function codexMcpRenderArgRows(args) {
    const list = args || [];
    if (list.length === 0) {
      return `<div class="codex-mcp-arg-list" id="codexMcpArgList"></div>`;
    }
    return `<div class="codex-mcp-arg-list" id="codexMcpArgList">${list
      .map(
        (a, i) => `<div class="codex-mcp-arg-row" data-arg-idx="${i}">
          <input type="text" class="form-control codex-mcp-arg-input" value="${escapeHtml(a)}" placeholder="-y" />
          <button type="button" class="btn-icon-only" data-action="codex-mcp-form-remove-arg" data-idx="${i}"><i class="bi bi-x-lg"></i></button>
        </div>`,
      )
      .join("")}</div>`;
  }

  /** env / headers — KEY=VALUE row pair list */
  function codexMcpRenderKvRows(prefix, map) {
    const entries = map ? Object.entries(map) : [];
    if (entries.length === 0) {
      return `<div class="codex-mcp-kv-list" id="codexMcpKvList-${prefix}"></div>`;
    }
    return `<div class="codex-mcp-kv-list" id="codexMcpKvList-${prefix}">${entries
      .map(
        ([k, v], i) => `<div class="codex-mcp-kv-row" data-kv-prefix="${prefix}" data-kv-idx="${i}">
          <input type="text" class="form-control codex-mcp-kv-key" value="${escapeHtml(k)}" placeholder="KEY" />
          <span class="codex-mcp-kv-eq">=</span>
          <input type="text" class="form-control codex-mcp-kv-val" value="${escapeHtml(v)}" placeholder="VALUE" />
          <button type="button" class="btn-icon-only" data-action="codex-mcp-form-remove-kv" data-prefix="${prefix}" data-idx="${i}"><i class="bi bi-x-lg"></i></button>
        </div>`,
      )
      .join("")}</div>`;
  }

  /** add row button helpers — 直接 append DOM,不重 render 整个 form */
  function codexMcpAddArgRow() {
    const list = $("#codexMcpArgList");
    if (!list) return;
    const idx = list.children.length;
    const row = document.createElement("div");
    row.className = "codex-mcp-arg-row";
    row.dataset.argIdx = String(idx);
    row.innerHTML = `<input type="text" class="form-control codex-mcp-arg-input" placeholder="-y" />
      <button type="button" class="btn-icon-only" data-action="codex-mcp-form-remove-arg" data-idx="${idx}"><i class="bi bi-x-lg"></i></button>`;
    list.appendChild(row);
    row.querySelector("input")?.focus();
  }
  function codexMcpAddKvRow(prefix) {
    const list = $(`#codexMcpKvList-${prefix}`);
    if (!list) return;
    const idx = list.children.length;
    const row = document.createElement("div");
    row.className = "codex-mcp-kv-row";
    row.dataset.kvPrefix = prefix;
    row.dataset.kvIdx = String(idx);
    row.innerHTML = `<input type="text" class="form-control codex-mcp-kv-key" placeholder="KEY" />
      <span class="codex-mcp-kv-eq">=</span>
      <input type="text" class="form-control codex-mcp-kv-val" placeholder="VALUE" />
      <button type="button" class="btn-icon-only" data-action="codex-mcp-form-remove-kv" data-prefix="${prefix}" data-idx="${idx}"><i class="bi bi-x-lg"></i></button>`;
    list.appendChild(row);
    row.querySelector("input")?.focus();
  }
  function codexMcpRemoveArgRow(idx) {
    const row = document.querySelector(`#codexMcpArgList .codex-mcp-arg-row[data-arg-idx="${idx}"]`);
    if (row) row.remove();
  }
  function codexMcpRemoveKvRow(prefix, idx) {
    const row = document.querySelector(`#codexMcpKvList-${prefix} .codex-mcp-kv-row[data-kv-idx="${idx}"]`);
    if (row) row.remove();
  }

  function codexMcpCollectKvRows(prefix) {
    const rows = $all(`#codexMcpKvList-${prefix} .codex-mcp-kv-row`);
    const out = {};
    for (const row of rows) {
      const k = row.querySelector(".codex-mcp-kv-key")?.value?.trim();
      const v = row.querySelector(".codex-mcp-kv-val")?.value?.trim() ?? "";
      if (k) out[k] = v;
    }
    return out;
  }

  function codexMcpRenderKvLines(map) {
    if (!map || Object.keys(map).length === 0) return "";
    return Object.entries(map).map(([k, v]) => `${k}=${v}`).join("\n");
  }
  function codexMcpParseKvLines(text) {
    const out = {};
    for (const line of (text || "").split(/\r?\n/)) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const idx = trimmed.indexOf("=");
      if (idx <= 0) continue;
      const k = trimmed.slice(0, idx).trim();
      const v = trimmed.slice(idx + 1).trim();
      if (k) out[k] = v;
    }
    return out;
  }
  function codexMcpParseCsvList(text) {
    if (!text) return null;
    const arr = text
      .split(",")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    return arr.length === 0 ? null : arr;
  }

  function codexMcpCollectFormSpec() {
    const transport = (document.querySelector('input[name="codexMcpTransport"]:checked')?.value) || "stdio";
    const name = ($("#codexMcpFormName")?.value || "").trim();
    const spec = {
      name,
      transport,
      enabled: !!$("#codexMcpFormEnabled")?.checked,
      required: !!$("#codexMcpFormRequired")?.checked,
      supportsParallelToolCalls: !!$("#codexMcpFormParallel")?.checked,
    };
    if (transport === "stdio") {
      spec.command = ($("#codexMcpFormCommand")?.value || "").trim();
      const argInputs = $all("#codexMcpArgList .codex-mcp-arg-input");
      spec.args = argInputs
        .map((el) => (el.value || "").trim())
        .filter((s) => s.length > 0);
      const envMap = codexMcpCollectKvRows("env");
      spec.env = Object.keys(envMap).length > 0 ? envMap : null;
      const cwd = ($("#codexMcpFormCwd")?.value || "").trim();
      spec.cwd = cwd || null;
    } else {
      spec.url = ($("#codexMcpFormUrl")?.value || "").trim();
      const bearer = ($("#codexMcpFormBearerEnv")?.value || "").trim();
      spec.bearerTokenEnvVar = bearer || null;
      const hh = codexMcpCollectKvRows("hh");
      spec.httpHeaders = Object.keys(hh).length > 0 ? hh : null;
      const ehh = codexMcpCollectKvRows("ehh");
      spec.envHttpHeaders = Object.keys(ehh).length > 0 ? ehh : null;
    }
    const startup = parseInt($("#codexMcpFormStartupTimeout")?.value, 10);
    spec.startupTimeoutSec = isFinite(startup) && startup >= 0 ? startup : null;
    const toolTo = parseInt($("#codexMcpFormToolTimeout")?.value, 10);
    spec.toolTimeoutSec = isFinite(toolTo) && toolTo >= 0 ? toolTo : null;
    const mode = ($("#codexMcpFormApprovalMode")?.value || "").trim();
    spec.defaultToolsApprovalMode = mode || null;
    spec.enabledTools = codexMcpParseCsvList($("#codexMcpFormEnabledTools")?.value);
    spec.disabledTools = codexMcpParseCsvList($("#codexMcpFormDisabledTools")?.value);
    const expEnv = ($("#codexMcpFormExperimental")?.value || "").trim();
    spec.experimentalEnvironment = expEnv || null;
    return spec;
  }

  /** 进入 / 退出编辑模式 — toggle JSON read-only ↔ textarea */
  function codexMcpServerEditToggle() {
    if (codexMcpJsonEditMode) {
      // 当前编辑模式 → 保存
      codexMcpJsonSave();
    } else {
      // 进入编辑模式 — draft 用 nul,让 render 读 current spec
      codexMcpJsonDraft = "";
      codexMcpJsonEditMode = true;
      codexMcpRenderForm();
    }
  }

  function codexMcpJsonErrShow(msg) {
    const el = $("#codexMcpJsonError");
    if (!el) { showToast(msg); return; }
    el.textContent = msg;
    el.hidden = false;
  }

  function codexMcpJsonErrClear() {
    const el = $("#codexMcpJsonError");
    if (el) el.hidden = true;
  }

  async function codexMcpJsonSave() {
    const ta = $("#codexMcpJsonTextarea");
    if (!ta) return;
    codexMcpJsonErrClear();
    let parsed;
    try {
      parsed = JSON.parse(ta.value || "{}");
    } catch (e) {
      codexMcpJsonErrShow("JSON 解析失败:" + (e.message || e));
      return;
    }
    if (typeof parsed !== "object" || Array.isArray(parsed) || parsed === null) {
      codexMcpJsonErrShow("JSON 必须是一个 object(花括号 {...})");
      return;
    }
    const isNew = codexMcpCurrentServerName === "__new__";
    const name = isNew ? codexMcpPendingNewName : codexMcpCurrentServerName;
    if (!name) {
      codexMcpJsonErrShow("server 名缺失");
      return;
    }
    // 推断 transport:JSON 里有 transport 字段优先;否则按 command/url 启发判断
    let transport = parsed.transport;
    if (!transport) {
      if (typeof parsed.command === "string" && parsed.command.length > 0) transport = "stdio";
      else if (typeof parsed.url === "string" && parsed.url.length > 0) transport = "streamable_http";
      else transport = "stdio";
    }
    if (transport !== "stdio" && transport !== "streamable_http") {
      codexMcpJsonErrShow(`transport 仅支持 "stdio" 跟 "streamable_http",收到:${transport}`);
      return;
    }
    const spec = {
      name,
      transport,
      command: parsed.command ?? null,
      args: Array.isArray(parsed.args) ? parsed.args : null,
      env: parsed.env && typeof parsed.env === "object" ? parsed.env : null,
      cwd: parsed.cwd ?? null,
      url: parsed.url ?? null,
      bearerTokenEnvVar: parsed.bearerTokenEnvVar ?? parsed.bearer_token_env_var ?? null,
      httpHeaders: parsed.httpHeaders ?? parsed.http_headers ?? null,
      envHttpHeaders: parsed.envHttpHeaders ?? parsed.env_http_headers ?? null,
      enabled: parsed.enabled !== false,
      required: !!parsed.required,
      supportsParallelToolCalls: !!(parsed.supportsParallelToolCalls ?? parsed.supports_parallel_tool_calls),
      experimentalEnvironment: parsed.experimentalEnvironment ?? parsed.experimental_environment ?? null,
      startupTimeoutSec: parsed.startupTimeoutSec ?? parsed.startup_timeout_sec ?? null,
      toolTimeoutSec: parsed.toolTimeoutSec ?? parsed.tool_timeout_sec ?? null,
      defaultToolsApprovalMode: parsed.defaultToolsApprovalMode ?? parsed.default_tools_approval_mode ?? null,
      enabledTools: Array.isArray(parsed.enabledTools ?? parsed.enabled_tools) ? (parsed.enabledTools ?? parsed.enabled_tools) : null,
      disabledTools: Array.isArray(parsed.disabledTools ?? parsed.disabled_tools) ? (parsed.disabledTools ?? parsed.disabled_tools) : null,
    };
    try {
      const r = await fetch("/api/codex/mcp/servers", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(spec),
      });
      if (!r.ok) {
        const j = await r.json().catch(() => ({}));
        codexMcpJsonErrShow(j.error || "save failed");
        return;
      }
      showToast(t("codex.mcp.saveOk"));
      codexMcpCurrentServerName = name;
      codexMcpPendingNewName = null;
      codexMcpJsonEditMode = false;
      codexMcpJsonDraft = "";
      await codexMcpReloadServers();
    } catch (e) { codexMcpJsonErrShow(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpServerDelete() {
    if (!codexMcpCurrentServerName || codexMcpCurrentServerName === "__new__") return;
    if (!confirm(`确认删除 server "${codexMcpCurrentServerName}"?(会同步删 ~/.codex/config.toml 对应节)`)) return;
    try {
      const r = await fetch("/api/codex/mcp/servers/delete", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: codexMcpCurrentServerName }),
      });
      if (!r.ok) {
        const j = await r.json().catch(() => ({}));
        throw new Error(j.error || "delete failed");
      }
      codexMcpCurrentServerName = null;
      await codexMcpReloadServers();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  let codexMcpPendingNewName = null;

  function codexMcpServerNew() {
    // 弹 inline modal 收 name
    const modal = $("#codexMcpNewServerModal");
    const input = $("#codexMcpNewServerNameInput");
    if (!modal || !input) return;
    input.value = "";
    modal.hidden = false;
    setTimeout(() => input.focus(), 50);
  }

  function codexMcpServerNewCancel() {
    const modal = $("#codexMcpNewServerModal");
    if (modal) modal.hidden = true;
    codexMcpPendingNewName = null;
  }

  function codexMcpServerNewConfirm() {
    const input = $("#codexMcpNewServerNameInput");
    const name = ((input?.value) || "").trim();
    if (!name) { showToast("名字不能为空"); return; }
    if (!/^[A-Za-z0-9_.\-]+$/.test(name)) {
      showToast("名字仅允许字母数字 / 短横 / 下划线 / 点");
      return;
    }
    if (codexMcpServersCache.some((s) => s.name === name)) {
      showToast(`server "${name}" 已存在`);
      return;
    }
    codexMcpPendingNewName = name;
    codexMcpCurrentServerName = "__new__";
    codexMcpJsonEditMode = true;
    codexMcpJsonDraft = JSON.stringify({
      transport: "stdio",
      command: "npx",
      args: [],
      enabled: true,
    }, null, 2);
    const modal = $("#codexMcpNewServerModal");
    if (modal) modal.hidden = true;
    codexMcpRenderServersList();
    codexMcpRenderForm();
  }

  async function codexMcpServersBackup() {
    try {
      const r = await fetch("/api/codex/mcp/servers/backup", { method: "POST" });
      if (!r.ok) {
        const j = await r.json().catch(() => ({}));
        throw new Error(j.error || "backup failed");
      }
      showToast(t("codex.agentsBackupOk"));
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpServersOpenHistory() {
    codexDocActiveResource = "mcp";
    try {
      const r = await fetch("/api/codex/mcp/servers/history");
      if (!r.ok) throw new Error("history failed");
      const j = await r.json();
      const entries = j.history || [];
      codexHistoryEntries = entries.slice().reverse();
      codexHistorySelectedIdx = codexHistoryEntries.length > 0 ? 0 : null;
      codexHistoryRenderToggle();
      codexHistoryRenderMenu();
      codexHistoryRenderDiff();
      const modal = $("#codexHistoryModal");
      if (modal) modal.hidden = false;
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpRawToggle() {
    const wrap = $("#codexMcpRawWrap");
    const ta = $("#codexMcpRawTextarea");
    if (!wrap || !ta) return;
    if (wrap.hidden) {
      try {
        const r = await fetch("/api/codex/mcp/config/raw");
        if (!r.ok) throw new Error("raw fetch failed");
        const j = await r.json();
        codexMcpRawSnapshot = j.content || "";
        ta.value = codexMcpRawSnapshot;
        wrap.hidden = false;
      } catch (e) { showToast(e.message || t("toast.requestFailed")); }
    } else {
      wrap.hidden = true;
    }
  }

  async function codexMcpRawApply() {
    const ta = $("#codexMcpRawTextarea");
    if (!ta) return;
    try {
      const r = await fetch("/api/codex/mcp/config/raw", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content: ta.value }),
      });
      if (!r.ok) {
        const j = await r.json().catch(() => ({}));
        throw new Error(j.error || "apply raw failed");
      }
      showToast(t("codex.mcp.saveOk"));
      $("#codexMcpRawWrap").hidden = true;
      await codexMcpReloadServers();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  function codexMcpRawCancel() {
    const ta = $("#codexMcpRawTextarea");
    if (ta) ta.value = codexMcpRawSnapshot;
    $("#codexMcpRawWrap").hidden = true;
  }

  // ── Plugins ──

  async function codexMcpReloadPlugins() {
    try {
      const r = await fetch("/api/codex/mcp/plugins");
      if (!r.ok) throw new Error("list plugins failed");
      const j = await r.json();
      codexMcpPluginsCache = j.plugins || [];
      codexMcpRenderPlugins();
    } catch (e) {
      console.error("codexMcpReloadPlugins:", e);
      codexMcpPluginsCache = [];
      codexMcpRenderPlugins();
    }
  }

  function codexMcpRenderPlugins() {
    const wrap = $("#codexMcpPluginsList");
    if (!wrap) return;
    if (codexMcpPluginsCache.length === 0) {
      wrap.innerHTML = `<li class="codex-mcp-empty-form">${escapeHtml(t("codex.mcp.pluginsEmpty"))}</li>`;
      return;
    }
    wrap.innerHTML = codexMcpPluginsCache
      .map((p) => {
        const enableIcon = p.enabled ? "bi-check2-square" : "bi-square";
        const enableLabel = p.enabled ? "已启用" : "已关闭";
        return `<li class="codex-mcp-plugin-item" data-plugin-key="${escapeHtml(p.key)}">
          <div class="codex-mcp-plugin-item-head">
            <span class="codex-mcp-plugin-name">${escapeHtml(p.name)}</span>
            <span class="codex-mcp-plugin-version">@${escapeHtml(p.marketplace)} · v${escapeHtml(p.version)}</span>
          </div>
          <div class="codex-mcp-plugin-actions">
            <button class="btn btn-outline-primary" type="button" data-action="codex-mcp-plugin-toggle-btn" data-key="${escapeHtml(p.key)}" data-enabled="${p.enabled}"><i class="bi ${enableIcon}"></i>${enableLabel}</button>
            <button class="btn btn-outline-danger" type="button" data-action="codex-mcp-plugin-uninstall" data-key="${escapeHtml(p.key)}"><i class="bi bi-trash"></i>卸载</button>
          </div>
        </li>`;
      })
      .join("");
  }

  async function codexMcpPluginToggle(key, enabled) {
    try {
      const r = await fetch("/api/codex/mcp/plugins/toggle", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ key, enabled }),
      });
      if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "toggle failed"); }
      await codexMcpReloadPlugins();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpPluginUninstall(key) {
    if (!confirm(`确认卸载 plugin "${key}"?会同步删除 ~/.codex/plugins/cache/ 下整个目录`)) return;
    try {
      const r = await fetch("/api/codex/mcp/plugins/uninstall", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ key }),
      });
      if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "uninstall failed"); }
      showToast(t("codex.mcp.uninstallOk"));
      await codexMcpReloadPlugins();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  // ── Marketplace ──

  async function codexMcpReloadSources() {
    try {
      const r = await fetch("/api/codex/mcp/marketplace/sources");
      if (!r.ok) throw new Error("sources failed");
      const j = await r.json();
      codexMcpSourcesCache = j.sources || [];
      codexMcpRenderSources();
    } catch (e) {
      console.error("codexMcpReloadSources:", e);
      codexMcpSourcesCache = [];
      codexMcpRenderSources();
    }
  }

  function codexMcpRenderSources() {
    const wrap = $("#codexMcpSourcesRow");
    if (!wrap) return;
    wrap.innerHTML = codexMcpSourcesCache
      .map((s) => {
        const active = s.enabled ? " active" : "";
        const disabled = s.enabled ? "" : " disabled";
        const removeBtn = s.official
          ? ""
          : `<button class="codex-mcp-source-remove" data-action="codex-mcp-source-remove" data-id="${escapeHtml(s.id)}" title="删除该源"><i class="bi bi-x"></i></button>`;
        return `<span class="codex-mcp-source-chip${active}${disabled}" data-action="codex-mcp-source-toggle" data-id="${escapeHtml(s.id)}" data-enabled="${!s.enabled}">
          <i class="bi bi-${s.official ? "patch-check-fill" : "globe2"}"></i>
          ${escapeHtml(s.name)}
          ${removeBtn}
        </span>`;
      })
      .join("");
  }

  async function codexMcpSourceToggle(id, enabled) {
    try {
      const r = await fetch("/api/codex/mcp/marketplace/sources/toggle", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id, enabled }),
      });
      if (!r.ok) throw new Error("toggle source failed");
      await codexMcpReloadSources();
      await codexMcpReloadMarketIndex(true);
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpSourceRemove(id) {
    if (!confirm("删除该 marketplace 源?(官方源不可删)")) return;
    try {
      const r = await fetch("/api/codex/mcp/marketplace/sources/remove", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id }),
      });
      if (!r.ok) throw new Error("remove source failed");
      await codexMcpReloadSources();
      await codexMcpReloadMarketIndex(true);
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  function codexMcpSourceAddOpen() {
    const modal = $("#codexMcpAddSourceModal");
    const nameInp = $("#codexMcpSourceNameInput");
    const urlInp = $("#codexMcpSourceUrlInput");
    if (!modal || !nameInp || !urlInp) return;
    nameInp.value = "";
    urlInp.value = "";
    modal.hidden = false;
    setTimeout(() => nameInp.focus(), 50);
  }

  function codexMcpSourceAddClose() {
    const modal = $("#codexMcpAddSourceModal");
    if (modal) modal.hidden = true;
  }

  async function codexMcpSourceAddConfirm() {
    const name = ($("#codexMcpSourceNameInput")?.value || "").trim();
    const url = ($("#codexMcpSourceUrlInput")?.value || "").trim();
    if (!name || !url) { showToast("name 跟 url 都必填"); return; }
    try {
      const r = await fetch("/api/codex/mcp/marketplace/sources/add", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name, url }),
      });
      if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "add source failed"); }
      codexMcpSourceAddClose();
      await codexMcpReloadSources();
      await codexMcpReloadMarketIndex(true);
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpReloadMarketIndex(forceRefresh) {
    try {
      const r = await fetch(`/api/codex/mcp/marketplace/index${forceRefresh ? "?force_refresh=true" : ""}`);
      if (!r.ok) throw new Error("market index failed");
      const j = await r.json();
      codexMcpMarketIndex = j.index || { servers: [], plugins: [], errors: {} };
      codexMcpRenderMarketIndex();
    } catch (e) {
      console.error("codexMcpReloadMarketIndex:", e);
      codexMcpMarketIndex = { servers: [], plugins: [], errors: {} };
      codexMcpRenderMarketIndex();
    }
  }

  function codexMcpRenderMarketIndex() {
    const serversWrap = $("#codexMcpMarketServersList");
    const pluginsWrap = $("#codexMcpMarketPluginsList");
    const filter = (codexMcpMarketFilter || "").trim().toLowerCase();
    const matches = (txt) => !filter || (txt || "").toLowerCase().includes(filter);

    const errEntries = Object.entries(codexMcpMarketIndex.errors || {});
    let errHtml = "";
    if (errEntries.length > 0) {
      errHtml = errEntries
        .map(([id, msg]) => `<div class="codex-mcp-market-error">源 <code>${escapeHtml(id)}</code> fetch 失败:${escapeHtml(msg)}</div>`)
        .join("");
    }

    if (serversWrap) {
      const filtered = (codexMcpMarketIndex.servers || []).filter(
        (s) => matches(s.id) || matches(s.name) || matches(s.description) || matches(s.transport),
      );
      const html = filtered
        .map((s) => {
          const chip = s.transport === "stdio"
            ? `<span class="codex-mcp-chip stdio">Stdio</span>`
            : `<span class="codex-mcp-chip http">HTTP</span>`;
          return `<li class="codex-mcp-market-item">
            <div class="codex-mcp-market-item-body">
              <div class="codex-mcp-market-item-name">${chip}<span>${escapeHtml(s.name || s.id)}</span><span class="codex-mcp-market-source-tag">${escapeHtml(s.source || "?")}</span></div>
              ${s.description ? `<div class="codex-mcp-market-item-desc">${escapeHtml(s.description)}</div>` : ""}
            </div>
            <div class="codex-mcp-market-item-action">
              <button class="btn btn-outline-primary btn-sm" type="button" data-action="codex-mcp-market-install-server" data-id="${escapeHtml(s.id)}"><i class="bi bi-download"></i>添加到 Servers</button>
            </div>
          </li>`;
        })
        .join("");
      serversWrap.innerHTML = errHtml + (filtered.length === 0
        ? `<li class="codex-mcp-empty-form">${escapeHtml(t("codex.mcp.marketEmpty"))}</li>`
        : html);
    }
    if (pluginsWrap) {
      const filtered = (codexMcpMarketIndex.plugins || []).filter(
        (p) => matches(p.id) || matches(p.description) || matches(p.marketplace),
      );
      const html = filtered
        .map((p) => {
          const caps = p.capabilities ? `mcp:${p.capabilities.mcpServers || 0} skills:${p.capabilities.skills || 0} apps:${p.capabilities.apps || 0}` : "";
          return `<li class="codex-mcp-market-item">
            <div class="codex-mcp-market-item-body">
              <div class="codex-mcp-market-item-name"><span>${escapeHtml(p.id)}</span><span class="codex-mcp-plugin-version">@${escapeHtml(p.marketplace)} v${escapeHtml(p.version)}</span><span class="codex-mcp-market-source-tag">${escapeHtml(p.source || "?")}</span></div>
              ${p.description ? `<div class="codex-mcp-market-item-desc">${escapeHtml(p.description)}</div>` : ""}
              ${caps ? `<div class="codex-mcp-plugin-caps">${caps}</div>` : ""}
            </div>
            <div class="codex-mcp-market-item-action">
              <button class="btn btn-outline-primary btn-sm" type="button" data-action="codex-mcp-market-install-plugin" data-id="${escapeHtml(p.id)}" data-marketplace="${escapeHtml(p.marketplace)}"><i class="bi bi-download"></i>安装</button>
            </div>
          </li>`;
        })
        .join("");
      pluginsWrap.innerHTML = filtered.length === 0
        ? `<li class="codex-mcp-empty-form">${escapeHtml(t("codex.mcp.marketEmpty"))}</li>`
        : html;
    }
  }

  async function codexMcpMarketInstallServer(id) {
    const item = (codexMcpMarketIndex.servers || []).find((s) => s.id === id);
    if (!item) return;
    const spec = {
      name: item.id,
      transport: item.transport === "stdio" ? "stdio" : "streamable_http",
      enabled: true,
      required: false,
      supportsParallelToolCalls: false,
    };
    if (item.transport === "stdio") {
      spec.command = item.command || "";
      spec.args = item.args || [];
    } else {
      spec.url = item.url || "";
      spec.bearerTokenEnvVar = item.bearerTokenEnvVar || null;
    }
    try {
      const r = await fetch("/api/codex/mcp/servers", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(spec),
      });
      if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "install server failed"); }
      showToast(t("codex.mcp.installServerOk"));
      codexMcpCurrentServerName = item.id;
      codexMcpSetSubpaneVisible("servers");
      await codexMcpReloadServers();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  async function codexMcpMarketInstallPlugin(id, marketplace) {
    const item = (codexMcpMarketIndex.plugins || []).find(
      (p) => p.id === id && p.marketplace === marketplace,
    );
    if (!item) return;
    if (!confirm(`下载并安装 plugin "${id}@${marketplace}" v${item.version}?\n\n来源:${item.tarballUrl}\n会解压到 ~/.codex/plugins/cache/${marketplace}/${id}/${item.version}/`)) return;
    try {
      showToast("正在下载 + 解压…");
      const r = await fetch("/api/codex/mcp/plugins/install", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          name: id,
          marketplace,
          version: item.version,
          tarballUrl: item.tarballUrl,
        }),
      });
      if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "install plugin failed"); }
      showToast(t("codex.mcp.installPluginOk"));
      codexMcpSetSubpaneVisible("plugins");
      await codexMcpReloadPlugins();
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  // ── Deeplink import ──
  // 处理 codex-app-transfer://v1/import?resource=mcp-server|plugin&...

  function codexMcpDeeplinkOpenConfirm(payload) {
    codexMcpPendingDeeplink = payload;
    const modal = $("#codexMcpDeeplinkModal");
    const pre = $("#codexMcpDeeplinkPreview");
    if (!modal || !pre) return;
    pre.textContent = JSON.stringify(payload, null, 2);
    modal.hidden = false;
  }

  function codexMcpDeeplinkCancel() {
    codexMcpPendingDeeplink = null;
    const modal = $("#codexMcpDeeplinkModal");
    if (modal) modal.hidden = true;
  }

  async function codexMcpDeeplinkConfirm() {
    const p = codexMcpPendingDeeplink;
    codexMcpDeeplinkCancel();
    if (!p) return;
    try {
      if (p.resource === "mcp-server") {
        const r = await fetch("/api/codex/mcp/servers", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(p.spec),
        });
        if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "deeplink install server failed"); }
        showToast(t("codex.mcp.deeplinkInstallOk"));
        window.location.hash = "codex";
        codexMcpSetSubpaneVisible("servers");
        await codexMcpReloadServers();
      } else if (p.resource === "plugin") {
        const r = await fetch("/api/codex/mcp/plugins/install", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(p.input),
        });
        if (!r.ok) { const j = await r.json().catch(() => ({})); throw new Error(j.error || "deeplink install plugin failed"); }
        showToast(t("codex.mcp.deeplinkInstallOk"));
        window.location.hash = "codex";
        codexMcpSetSubpaneVisible("plugins");
        await codexMcpReloadPlugins();
      }
    } catch (e) { showToast(e.message || t("toast.requestFailed")); }
  }

  /** 解析 deeplink URL,弹 confirmation modal。供 Tauri deep-link plugin / 也支持手动 paste 触发。 */
  function codexMcpHandleDeeplink(url) {
    try {
      const u = new URL(url);
      if (u.protocol !== "codex-app-transfer:") return false;
      const action = (u.pathname || "").replace(/^\/+/, "") || u.host;
      // 形态 1:/v1/import?...   形态 2:host=v1/import
      if (!action.includes("import")) return false;
      const resource = u.searchParams.get("resource");
      if (resource === "mcp-server") {
        const configB64 = u.searchParams.get("config");
        if (!configB64) { showToast("deeplink 缺 config 参数"); return false; }
        if (configB64.length > 16 * 1024) { showToast("deeplink config 过大(>16KB)"); return false; }
        let raw;
        try { raw = atob(configB64); } catch { showToast("deeplink config base64 解码失败"); return false; }
        let spec;
        try { spec = JSON.parse(raw); } catch { showToast("deeplink config 不是合法 JSON"); return false; }
        codexMcpDeeplinkOpenConfirm({ resource: "mcp-server", spec });
        return true;
      }
      if (resource === "plugin") {
        const name = u.searchParams.get("name") || u.searchParams.get("id");
        const marketplace = u.searchParams.get("marketplace") || "official";
        const version = u.searchParams.get("version") || "local";
        const tarballUrl = u.searchParams.get("tarball_url") || u.searchParams.get("url");
        if (!name || !tarballUrl) { showToast("deeplink plugin 缺 name 或 tarball_url"); return false; }
        if (!tarballUrl.startsWith("https://")) { showToast("deeplink tarball_url 必须 https"); return false; }
        codexMcpDeeplinkOpenConfirm({
          resource: "plugin",
          input: { name, marketplace, version, tarballUrl },
        });
        return true;
      }
    } catch (e) {
      console.error("codexMcpHandleDeeplink:", e);
    }
    return false;
  }
  window.codexMcpHandleDeeplink = codexMcpHandleDeeplink;

  async function codexBlockLoadAndRender(type) {
    const status = await codexBlockFetchStatus(type);
    const el = $("#codexBlockStatus");
    if (!el) return;
    const lastApply = status.lastApply
      ? new Date(status.lastApply * 1000).toLocaleString()
      : t("codex.statusNone");
    const stateLabel = status.hasManaged ? t("codex.statusManaged") : t("codex.statusEmpty");
    el.innerHTML = `<i class="bi bi-info-circle"></i><p>
      <strong>${escapeHtml(t("codex.statusBlockState"))}(${escapeHtml(type)}):</strong> ${escapeHtml(stateLabel)}<br>
      <strong>${escapeHtml(t("codex.statusUserBytes"))}:</strong> ${status.beforeUserBytes + status.afterUserBytes} ${escapeHtml(t("codex.statusBytesSuffix"))}<br>
      <strong>${escapeHtml(t("codex.statusHistoryCount"))}:</strong> ${status.historyCount} ${escapeHtml(t("codex.statusHistoryCountSuffix"))}<br>
      <strong>${escapeHtml(t("codex.statusLastApply"))}:</strong> ${escapeHtml(lastApply)}<br>
      <strong>${escapeHtml(t("codex.statusTargetFile"))}:</strong> <code>${escapeHtml(status.targetPath || "")}</code>
    </p>`;
    const ta = $("#codexBlockContent");
    if (ta) {
      if (status.outerSignature !== undefined) {
        ta.dataset.outerSignature = status.outerSignature;
      }
      if (status.managedContent !== undefined && !ta.dataset.dirty) {
        ta.value = status.managedContent || "";
      }
    }
  }

  async function codexBlockPreview(type) {
    const content = $("#codexBlockContent")?.value ?? "";
    const r = await fetch(`${codexBlockUrl(type)}/preview${codexAgentsHashSuffix(type)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ content }),
    });
    if (!r.ok) throw new Error("preview failed");
    const j = await r.json();
    const pre = $("#codexBlockPreviewArea");
    if (pre) {
      pre.textContent = j.rendered ?? "(empty)";
      pre.hidden = false;
    }
  }

  async function codexBlockApply(type) {
    const ta = $("#codexBlockContent");
    const content = ta?.value ?? "";
    const expectedOuterSignature = ta?.dataset?.outerSignature || null;
    const body = { content };
    if (expectedOuterSignature) {
      body.expectedOuterSignature = expectedOuterSignature;
    }
    const r = await fetch(`${codexBlockUrl(type)}/apply${codexAgentsHashSuffix(type)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      throw new Error(err.error || "apply failed");
    }
    if (ta) delete ta.dataset.dirty;
    await codexBlockLoadAndRender(type);
    showToast(tFmt("codex.toastApplied", { type }));
  }

  async function codexBlockClear(type) {
    const r = await fetch(`${codexBlockUrl(type)}/clear${codexAgentsHashSuffix(type)}`, { method: "POST" });
    if (!r.ok) throw new Error("clear failed");
    await codexBlockLoadAndRender(type);
    showToast(tFmt("codex.toastCleared", { type }));
  }

  async function codexBlockRollback(type, idx) {
    const r = await fetch(`${codexBlockUrl(type)}/rollback${codexAgentsHashSuffix(type)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ index: idx }),
    });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      throw new Error(err.error || "rollback failed");
    }
    await codexBlockLoadAndRender(type);
    await codexBlockRenderHistory(type);
    showToast(tFmt("codex.toastRollbacked", { type, idx }));
  }

  async function codexBlockRenderHistory(type) {
    const r = await fetch(`${codexBlockUrl(type)}/history${codexAgentsHashSuffix(type)}`);
    if (!r.ok) throw new Error("history failed");
    const j = await r.json();
    const list = $("#codexBlockHistoryList");
    if (!list) return;
    const items = (j.history || []).map((entry) => {
      const ts = new Date(entry.timestamp * 1000).toLocaleString();
      const preview = (entry.managedContent || "").slice(0, 80).replace(/\n/g, " ⏎ ");
      return `<li style="padding: 8px 12px; border: 1px solid var(--line); border-radius: 8px; margin-bottom: 6px;">
        <div style="display: flex; justify-content: space-between; align-items: center; gap: 12px;">
          <span style="font-family: monospace; font-size: 12px; color: var(--muted);">[${entry.index}] ${ts}</span>
          <button class="btn btn-outline-primary btn-sm" type="button" data-action="codex-block-rollback" data-type="${type}" data-idx="${entry.index}"><i class="bi bi-arrow-counterclockwise"></i> Rollback</button>
        </div>
        <pre style="font-size: 12px; max-height: 60px; overflow: hidden; margin: 4px 0 0; color: var(--muted);">${escapeHtml(preview) || "(empty)"}</pre>
      </li>`;
    });
    list.innerHTML =
      items.join("") || `<li><em>${escapeHtml(t("codex.historyEmpty"))}</em></li>`;
  }

  async function codexBlockToggleHistory(type) {
    const el = $("#codexBlockHistory");
    if (!el) return;
    if (el.hidden) {
      await codexBlockRenderHistory(type);
      el.hidden = false;
    } else {
      el.hidden = true;
    }
  }

  // ── Skills tab (file-snapshot backup / restore, 独立 ManagedBlock 之外) ──

  async function codexSkillsLoadAndRender() {
    const [listR, backupsR] = await Promise.all([
      fetch("/api/codex/skills/list"),
      fetch("/api/codex/skills/backups"),
    ]);
    if (!listR.ok || !backupsR.ok) throw new Error("skills load failed");
    const list = await listR.json();
    const backups = await backupsR.json();

    const statusEl = $("#codexSkillsStatus");
    if (statusEl) {
      const countSuffix = t("codex.skillsCountSuffix");
      const backupsSuffix = t("codex.skillsBackupsCountSuffix");
      statusEl.innerHTML = `<i class="bi bi-info-circle"></i><p>
        <strong>${escapeHtml(t("codex.skillsDirLabel"))}:</strong> <code>${escapeHtml(list.skillsDir || "")}</code><br>
        <strong>${escapeHtml(t("codex.skillsInstalledLabel"))}:</strong> ${list.count}${countSuffix ? " " + escapeHtml(countSuffix) : ""}<br>
        <strong>${escapeHtml(t("codex.skillsBackupDirLabel"))}:</strong> <code>${escapeHtml(backups.backupDir || "")}</code><br>
        <strong>${escapeHtml(t("codex.skillsBackupsLabel"))}:</strong> ${backups.count}${backupsSuffix ? " " + escapeHtml(backupsSuffix) : ""}
      </p>`;
    }

    const ul = $("#codexSkillsList");
    if (ul) {
      const filesSuffix = t("codex.skillsFilesSuffix");
      const rows = (list.entries || []).map((entry) => {
        const md = entry.has_skill_md
          ? t("codex.skillsHasSkillMd")
          : t("codex.skillsNoSkillMd");
        return `<li style="display: flex; justify-content: space-between; padding: 4px 8px; border-bottom: 1px dashed var(--line);">
          <span>${escapeHtml(entry.name)}</span>
          <small style="color: var(--muted);">${escapeHtml(md)} · ${entry.files_count} ${escapeHtml(filesSuffix)}</small>
        </li>`;
      });
      ul.innerHTML =
        rows.join("") || `<li><em>${escapeHtml(t("codex.skillsListEmpty"))}</em></li>`;
    }

    const backupList = $("#codexSkillsBackupsList");
    if (backupList) {
      const rows = (backups.backups || []).map((entry) => {
        const ts = new Date(entry.created_unix * 1000).toLocaleString();
        const sizeKb = (entry.size_bytes / 1024).toFixed(1);
        return `<li style="padding: 8px 12px; border: 1px solid var(--line); border-radius: 8px; margin-bottom: 6px; display: flex; justify-content: space-between; align-items: center; gap: 12px;">
          <span><strong>${escapeHtml(entry.filename)}</strong> <small style="color: var(--muted);">${sizeKb} KB · ${ts}</small></span>
          <button class="btn btn-outline-primary btn-sm" type="button" data-action="codex-skills-restore" data-filename="${escapeHtml(entry.filename)}"><i class="bi bi-arrow-counterclockwise"></i> Restore</button>
        </li>`;
      });
      backupList.innerHTML =
        rows.join("") || `<li><em>${escapeHtml(t("codex.backupsListEmpty"))}</em></li>`;
    }
  }

  async function codexSkillsBackup() {
    const r = await fetch("/api/codex/skills/backup", { method: "POST" });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      throw new Error(err.error || "backup failed");
    }
    const j = await r.json();
    showToast(tFmt("codex.toastSkillsBackedUp", { name: j.backupPath?.split("/").pop() || "ok" }));
    await codexSkillsLoadAndRender();
  }

  async function codexSkillsRestore(filename) {
    const r = await fetch("/api/codex/skills/restore", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ filename }),
    });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      throw new Error(err.error || "restore failed");
    }
    showToast(tFmt("codex.toastSkillsRestored", { filename }));
    await codexSkillsLoadAndRender();
  }

  // ── sidebar tab switch + renderCodexAssets entry (#25 sidebar + lazy + 转场) ──

  /** sidebar tab visibility:active class + fade slide pane */
  function codexShowTab(tab) {
    const agentsPane = $("#codexAgentsRawTab");
    const memoriesPane = $("#codexMemoriesRawTab");
    const skillsRawPane = $("#codexSkillsRawTab");
    const blockPane = $("#codexBlockTab");
    if (agentsPane) {
      agentsPane.hidden = tab !== "agents";
      agentsPane.classList.toggle("active", tab === "agents");
    }
    if (memoriesPane) {
      memoriesPane.hidden = tab !== "memories";
      memoriesPane.classList.toggle("active", tab === "memories");
    }
    if (skillsRawPane) {
      skillsRawPane.hidden = tab !== "skills";
      skillsRawPane.classList.toggle("active", tab === "skills");
    }
    const mcpPane = $("#codexMcpTab");
    if (mcpPane) {
      mcpPane.hidden = tab !== "mcp";
      mcpPane.classList.toggle("active", tab === "mcp");
    }
    const convPane = $("#codexConversationsTab");
    if (convPane) {
      convPane.hidden = tab !== "conversations";
      convPane.classList.toggle("active", tab === "conversations");
    }
    // 切 tab 时把非当前 tab 的 Edit 模式回退 preview
    if (tab !== "agents") codexAgentsSwitchMode("preview");
    if (tab !== "memories") codexMemoriesSwitchMode("preview");
    if (tab !== "skills") codexSkillsSwitchMode("preview");

    // sidebar item active state
    $all("#codexSidebar .codex-sidebar-item").forEach((btn) => {
      btn.classList.toggle("active", btn.dataset.codexTab === tab);
    });
  }

  /** lazy load + 状态 badge 刷新 (sidebar 上各 item 显 ✓ / 数字) */
  async function codexLoadTab(tab) {
    if (tab === "skills") {
      await codexSkillsReloadPaths();
      await codexSkillsRawLoadAndRender();
    } else if (tab === "agents") {
      await codexAgentsReloadPaths();
      await codexAgentsRawLoadAndRender();
    } else if (tab === "memories") {
      await codexMemoriesReloadPaths();
      await codexMemoriesRawLoadAndRender();
    } else if (tab === "mcp") {
      await codexMcpOpenSubpane(codexMcpCurrentSubpane || "servers");
    } else if (tab === "conversations") {
      await codexConversationsLoadAndRender();
    }
    await codexRefreshSidebarBadges();
  }

  // ── #271 cas-dropdown 自定义下拉(替代 native <select> 让 menu 锚定在
  //     toggle 正下方,不被 OS popup 移到选中项位置)─────────────────────
  /**
   * 给 `<div class="cas-dropdown" data-value="...">` 节点绑定行为:
   * - render(options): 重建 menu items + 更新当前 label
   * - getValue(): 返 data-value
   * - setValue(v): 改 data-value + 同步 label + 触发 change 回调
   * - 点击外部 / ESC → 收起
   *
   * options: `[{ value, label, title? }]`
   */
  function casDropdownBind(rootEl, { options, onChange }) {
    if (!rootEl || rootEl._casBound) {
      if (rootEl) {
        // 已绑过,只刷新 options
        casDropdownRender(rootEl, options);
      }
      return;
    }
    rootEl._casBound = true;
    rootEl._casOnChange = onChange;
    const toggle = rootEl.querySelector(".cas-dropdown-toggle");
    const menu = rootEl.querySelector(".cas-dropdown-menu");
    // devin #272 silent-failure-hunter fix: 必有子节点才绑;缺则 log + abort
    // 否则点击时 menu.hidden 抛 TypeError 静默(devtools 关 user 看不到)
    if (!toggle || !menu) {
      console.error("cas-dropdown missing required child .cas-dropdown-toggle / .cas-dropdown-menu", rootEl);
      return;
    }
    toggle.addEventListener("click", (e) => {
      e.stopPropagation();
      const isOpen = !menu.hidden;
      casDropdownCloseAll();
      if (!isOpen) {
        menu.hidden = false;
        toggle.setAttribute("aria-expanded", "true");
      }
    });
    menu?.addEventListener("click", (e) => {
      const li = e.target.closest("li[data-value]");
      if (!li) return;
      const newValue = li.dataset.value;
      casDropdownSetValue(rootEl, newValue, { fireChange: true });
      casDropdownClose(rootEl);
    });
    casDropdownRender(rootEl, options);
  }

  function casDropdownRender(rootEl, options) {
    rootEl._casOptions = options || [];
    const menu = rootEl.querySelector(".cas-dropdown-menu");
    if (!menu) return;
    menu.innerHTML = "";
    for (const opt of rootEl._casOptions) {
      const li = document.createElement("li");
      li.dataset.value = opt.value;
      li.textContent = opt.label;
      if (opt.title) li.title = opt.title;
      li.setAttribute("role", "menuitem");
      menu.appendChild(li);
    }
    // 当前 data-value 在新 options 里找不到时 → fallback 到第一个
    const cur = rootEl.dataset.value;
    if (!rootEl._casOptions.some((o) => o.value === cur)) {
      const firstVal = rootEl._casOptions[0]?.value;
      if (firstVal) casDropdownSetValue(rootEl, firstVal, { fireChange: false });
    } else {
      casDropdownSyncLabel(rootEl);
    }
  }

  function casDropdownSyncLabel(rootEl) {
    const labelEl = rootEl.querySelector(".cas-dropdown-label");
    const cur = rootEl.dataset.value;
    const opt = (rootEl._casOptions || []).find((o) => o.value === cur);
    if (labelEl && opt) labelEl.textContent = opt.label;
    // 标记 menu 内当前项
    rootEl.querySelectorAll(".cas-dropdown-menu li").forEach((li) => {
      li.classList.toggle("cas-dropdown-selected", li.dataset.value === cur);
    });
  }

  function casDropdownGetValue(rootEl) {
    return rootEl?.dataset?.value || "";
  }

  function casDropdownSetValue(rootEl, v, { fireChange } = { fireChange: true }) {
    if (!rootEl) return;
    rootEl.dataset.value = v;
    casDropdownSyncLabel(rootEl);
    if (fireChange && typeof rootEl._casOnChange === "function") {
      rootEl._casOnChange(v);
    }
  }

  function casDropdownClose(rootEl) {
    const menu = rootEl?.querySelector(".cas-dropdown-menu");
    const toggle = rootEl?.querySelector(".cas-dropdown-toggle");
    if (menu) menu.hidden = true;
    if (toggle) toggle.setAttribute("aria-expanded", "false");
  }

  function casDropdownCloseAll() {
    document.querySelectorAll(".cas-dropdown").forEach(casDropdownClose);
  }

  // 全局监听一次:外部点击 + ESC → close all
  document.addEventListener("click", (e) => {
    if (!e.target.closest(".cas-dropdown")) casDropdownCloseAll();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") casDropdownCloseAll();
  });

  // ── #271 Codex CLI rollout 对话导出 ─────────────────────────────────
  let conversationsCache = [];                  // SessionMeta[] 缓存
  let conversationsSelected = new Set();        // 多选集合
  let conversationsActiveId = null;             // 当前展开详情的 session
  let conversationsExportOptions = {
    includeReasoning: false,
    includeToolCalls: true,
    toolOutputMaxChars: 2048,
    includeSystemPrompts: false,
    redactSecrets: true,
  };

  const CAS_CONV_DEFAULT_DIR_KEY = "cas.conv.defaultExportDir";
  function codexConvLoadDefaultDir() {
    try { return localStorage.getItem(CAS_CONV_DEFAULT_DIR_KEY) || ""; }
    catch (e) { console.warn("cas: localStorage read failed for default-dir", e); return ""; }
  }
  function codexConvSaveDefaultDir(dir) {
    try {
      if (dir) localStorage.setItem(CAS_CONV_DEFAULT_DIR_KEY, dir);
      else localStorage.removeItem(CAS_CONV_DEFAULT_DIR_KEY);
    } catch (e) {
      console.warn("cas: localStorage write failed for default-dir", e);
    }
  }
  function codexConvSyncDefaultDirUI() {
    const input = $("#codexConvDefaultDir");
    const clearBtn = $("#codexConvDefaultDirClear");
    if (!input) return;
    const cur = codexConvLoadDefaultDir();
    input.value = cur;
    if (clearBtn) clearBtn.hidden = !cur;
  }
  async function codexConvPickDefaultDir() {
    const dialog = window.__TAURI__?.dialog;
    if (!dialog?.open) {
      showToast("Tauri dialog API 不可用");
      return;
    }
    try {
      const picked = await dialog.open({
        title: t("codex.conv.defaultDirPickTitle") || "选择默认导出文件夹",
        directory: true,
        multiple: false,
        defaultPath: codexConvLoadDefaultDir() || undefined,
      });
      if (!picked) return;
      const dir = Array.isArray(picked) ? picked[0] : picked;
      codexConvSaveDefaultDir(dir);
      codexConvSyncDefaultDirUI();
      showToast(tFmt("codex.conv.defaultDirSet", { path: dir }));
    } catch (e) {
      // devin #272 silent-failure-hunter MED-2: picker 失败不用 exportFailed 错误措辞
      showToast(`${t("codex.conv.defaultDirPickFailed") || "选择目录失败"}: ${e.message || e}`);
    }
  }
  function codexConvClearDefaultDir() {
    codexConvSaveDefaultDir("");
    codexConvSyncDefaultDirUI();
    showToast(t("codex.conv.defaultDirCleared") || "已清除");
  }

  let _convInitDone = false;
  function codexConversationsInitOnce() {
    if (_convInitDone) return;
    _convInitDone = true;
    codexConvSyncDefaultDirUI();
    $("#codexConvSearch")?.addEventListener("input", codexConversationsRenderList);
    // cas-dropdown: kind / format 是固定 options,在 init 时绑一次
    casDropdownBind($("#codexConvKindFilter"), {
      options: [
        { value: "all", label: t("codex.conv.kindAll") || "全部" },
        { value: "active", label: t("codex.conv.kindActive") || "Active" },
        { value: "archived", label: t("codex.conv.kindArchived") || "Archived" },
      ],
      onChange: () => codexConversationsRenderList(),
    });
    casDropdownBind($("#codexConvFormat"), {
      options: [
        { value: "markdown", label: "Markdown (.md)" },
        { value: "json", label: "JSON (.json)" },
        { value: "jsonl", label: t("codex.conv.formatJsonl") || "原始 JSONL" },
      ],
      onChange: () => {},
    });
    // cwd filter options 跟随 conversationsCache 重建,这里先绑 onChange + 空 options
    casDropdownBind($("#codexConvCwdFilter"), {
      options: [{ value: "all", label: t("codex.conv.cwdAll") || "所有项目" }],
      onChange: () => codexConversationsRenderList(),
    });
    $("#codexConvSelectAll")?.addEventListener("change", (e) => {
      const filtered = codexConversationsFiltered();
      if (e.target.checked) {
        filtered.forEach((s) => conversationsSelected.add(s.id));
      } else {
        filtered.forEach((s) => conversationsSelected.delete(s.id));
      }
      codexConversationsRenderList();
    });
  }

  async function codexConversationsLoadAndRender() {
    codexConversationsInitOnce();
    const list = $("#codexConvList");
    const summary = $("#codexConvSummary");
    if (list) list.innerHTML = `<li class="codex-conv-list-loading">${t("codex.conv.loading") || "加载中…"}</li>`;
    try {
      conversationsCache = await CCApi.listConversations();
    } catch (e) {
      conversationsCache = [];
      if (list) list.innerHTML = `<li class="codex-conv-list-loading">${e.message || e}</li>`;
      return;
    }
    if (summary) {
      summary.textContent = tFmt("codex.conv.summary", { count: conversationsCache.length });
    }
    codexConversationsPopulateCwdFilter();
    codexConversationsRenderList();
  }

  /** 把 conversationsCache 里所有 cwd 抽出来重建 cas-dropdown 选项. */
  function codexConversationsPopulateCwdFilter() {
    const root = $("#codexConvCwdFilter");
    if (!root) return;
    // 统计 cwd → count
    const counts = new Map();
    for (const s of conversationsCache) {
      if (!s.cwd) continue;
      counts.set(s.cwd, (counts.get(s.cwd) || 0) + 1);
    }
    const sorted = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
    const options = [{ value: "all", label: t("codex.conv.cwdAll") || "所有项目" }];
    for (const [cwd, count] of sorted) {
      const base = cwd.split("/").pop() || cwd;
      options.push({ value: cwd, label: `${base} (${count})`, title: cwd });
    }
    casDropdownRender(root, options);
  }

  function codexConversationsFiltered() {
    const search = ($("#codexConvSearch")?.value || "").toLowerCase().trim();
    const kindFilter = casDropdownGetValue($("#codexConvKindFilter")) || "all";
    const cwdFilter = casDropdownGetValue($("#codexConvCwdFilter")) || "all";
    return conversationsCache.filter((s) => {
      if (kindFilter !== "all" && s.kind !== kindFilter) return false;
      if (cwdFilter !== "all" && s.cwd !== cwdFilter) return false;
      if (!search) return true;
      const hay = [s.title || "", s.id, s.cwd, s.originator, s.modelProvider]
        .join(" ")
        .toLowerCase();
      return hay.includes(search);
    });
  }

  function codexConversationsRenderList() {
    const list = $("#codexConvList");
    if (!list) return;
    const filtered = codexConversationsFiltered();
    if (filtered.length === 0) {
      list.innerHTML = `<li class="codex-conv-list-loading">${t("codex.conv.noResults") || "无匹配 session"}</li>`;
      codexConvUpdateExportBtn();
      return;
    }
    list.innerHTML = filtered.map((s) => codexConversationsItemHtml(s)).join("");
    list.querySelectorAll(".codex-conv-list-item").forEach((el) => {
      el.addEventListener("click", (e) => {
        if (e.target.closest(".codex-conv-list-checkbox")) return;
        codexConversationsOpenDetail(el.dataset.sessionId);
      });
    });
    list.querySelectorAll(".codex-conv-list-checkbox").forEach((cb) => {
      cb.addEventListener("change", (e) => {
        const id = e.target.dataset.sessionId;
        if (e.target.checked) conversationsSelected.add(id);
        else conversationsSelected.delete(id);
        codexConvUpdateExportBtn();
      });
    });
    codexConvUpdateExportBtn();
  }

  function codexConversationsItemHtml(s) {
    const title = s.title || codexConvFallbackTitle(s);
    const kindCls = s.kind === "active" ? "active" : "archived";
    const date = s.createdAt ? new Date(s.createdAt).toLocaleString() : "";
    const cwdShort = s.cwd ? s.cwd.split("/").pop() || s.cwd : "";
    const isSelected = conversationsSelected.has(s.id);
    const isActive = conversationsActiveId === s.id;
    return `
      <li class="codex-conv-list-item ${isActive ? "selected" : ""}" data-session-id="${escapeHtml(s.id)}">
        <div class="codex-conv-list-item-row">
          <input type="checkbox" class="codex-conv-list-checkbox" data-session-id="${escapeHtml(s.id)}" ${isSelected ? "checked" : ""}>
          <span class="codex-conv-list-title" title="${escapeHtml(title)}">${escapeHtml(title)}</span>
          <span class="codex-conv-list-kind ${kindCls}">${kindCls}</span>
        </div>
        <div class="codex-conv-list-meta">
          <span>${escapeHtml(date)}</span>
          <span>· ${escapeHtml(cwdShort)}</span>
          <span>· ${s.turnCount} ${t("codex.conv.turns") || "turns"}</span>
          ${s.modelProvider ? `<span>· ${escapeHtml(s.modelProvider)}</span>` : ""}
        </div>
      </li>
    `;
  }

  function codexConvFallbackTitle(s) {
    // 没 title 时拿 cwd basename + 短 id 做兜底
    const cwdBase = (s.cwd || "").split("/").pop() || "";
    const shortId = (s.id || "").slice(0, 8);
    return cwdBase ? `${cwdBase} (${shortId})` : `Session ${shortId}`;
  }

  function codexConvUpdateExportBtn() {
    const exportBtn = $("#codexConvExportBtn");
    const deleteBtn = $("#codexConvDeleteBtn");
    const count = conversationsSelected.size;
    if (exportBtn) {
      exportBtn.disabled = count === 0;
      exportBtn.textContent = "";
      const icon = document.createElement("i");
      icon.className = "bi bi-download";
      exportBtn.appendChild(icon);
      const lbl = document.createElement("span");
      lbl.textContent = count > 0
        ? tFmt("codex.conv.exportSelectedN", { count })
        : t("codex.conv.exportSelected");
      exportBtn.appendChild(lbl);
    }
    if (deleteBtn) {
      deleteBtn.disabled = count === 0;
      deleteBtn.textContent = "";
      const icon = document.createElement("i");
      icon.className = "bi bi-trash";
      deleteBtn.appendChild(icon);
      const lbl = document.createElement("span");
      lbl.textContent = count > 0
        ? tFmt("codex.conv.deleteSelectedN", { count })
        : t("codex.conv.deleteSelected");
      deleteBtn.appendChild(lbl);
    }
  }

  async function codexConversationsOpenDetail(id) {
    conversationsActiveId = id;
    codexConversationsRenderList();
    const detail = $("#codexConvDetail");
    if (!detail) return;
    detail.innerHTML = `<p class="codex-conv-detail-empty">${t("codex.conv.loading") || "加载中…"}</p>`;
    let session;
    try {
      session = await CCApi.getConversation(id);
    } catch (e) {
      detail.innerHTML = `<p class="codex-conv-detail-empty">${escapeHtml(e.message || String(e))}</p>`;
      return;
    }
    detail.innerHTML = codexConversationsDetailHtml(session);
  }

  function codexConversationsDetailHtml(session) {
    const meta = session.meta || {};
    const headerTitle = meta.title || codexConvFallbackTitle(meta);
    let html = `<h3>${escapeHtml(headerTitle)}</h3>
      <div class="codex-conv-detail-meta">
        <div>ID: <code>${escapeHtml(meta.id || "")}</code></div>
        <div>${escapeHtml(meta.cwd || "")} · ${escapeHtml(meta.originator || "")} · ${escapeHtml(meta.modelProvider || "")}</div>
      </div>`;
    for (let i = 0; i < (session.turns || []).length; i += 1) {
      const turn = session.turns[i];
      html += `<div class="codex-conv-turn"><div class="codex-conv-turn-header">Turn ${i + 1}</div>`;
      for (const item of turn.items || []) {
        html += codexConversationsItemDetailHtml(item);
      }
      html += `</div>`;
    }
    return html;
  }

  function codexConversationsItemDetailHtml(item) {
    if (!item || !item.type) return "";
    switch (item.type) {
      case "User":
      case "user":
        // 用户输入通常是纯文本,但有的 IDE 会贴 markdown — 都按 md 渲染
        return `<div class="codex-conv-item"><div class="codex-conv-item-role">${t("codex.conv.roleUser") || "用户"}</div><div class="codex-conv-item-text codex-conv-md">${renderMiniMd(item.text || "")}</div></div>`;
      case "Assistant":
      case "assistant":
        return `<div class="codex-conv-item"><div class="codex-conv-item-role">${t("codex.conv.roleAssistant") || "助手"}</div><div class="codex-conv-item-text codex-conv-md">${renderMiniMd(item.text || "")}</div></div>`;
      case "Reasoning":
      case "reasoning":
        return `<details class="codex-conv-item"><summary>${t("codex.conv.reasoning") || "Reasoning"}</summary><div class="codex-conv-item-text codex-conv-md">${renderMiniMd(item.text || "")}</div></details>`;
      case "ToolCall":
      case "toolCall":
        // tool call 是机读 JSON/cmd,保持等宽不渲染 md
        return `<details class="codex-conv-item"><summary>🔧 ${escapeHtml(item.name || "")}</summary><div class="codex-conv-item-text codex-conv-tool">${escapeHtml(item.arguments || "")}</div></details>`;
      case "ToolOutput":
      case "toolOutput":
        return `<details class="codex-conv-item"><summary>↳ output</summary><div class="codex-conv-item-text codex-conv-tool">${escapeHtml(truncateString(item.output || "", 4000))}</div></details>`;
      case "Compacted":
      case "compacted":
        return `<div class="codex-conv-item codex-conv-compacted">📦 ${t("codex.conv.compacted") || "Autocompact 切点"}: ${renderMiniMd(item.summary || "")}</div>`;
      case "System":
      case "system":
        return `<details class="codex-conv-item"><summary>[${escapeHtml(item.role || "system")}]</summary><div class="codex-conv-item-text codex-conv-md">${renderMiniMd(item.text || "")}</div></details>`;
      default:
        return "";
    }
  }

  /**
   * #271 极简 markdown 渲染(避免外部依赖 + XSS 安全)。
   *
   * 支持:fenced code block / inline code / headings (# .. ######) / bold /
   * italic / unordered & ordered list / blockquote / link (仅 http(s)) /
   * 段落 + 软换行。先 escape HTML,再按 block 状态机渲染,inline 替换在
   * 已 escape 文本上跑。
   */
  function renderMiniMd(input) {
    if (!input) return "";
    const src = String(input).replace(/\r\n?/g, "\n");
    // 1. 抽走 fenced code block,placeholder 占位避免 inline rule 污染
    const codeBlocks = [];
    let body = src.replace(/```([a-zA-Z0-9_+-]*)\n([\s\S]*?)```/g, (_, lang, code) => {
      const idx = codeBlocks.push({ lang, code }) - 1;
      return `\x00CODEBLOCK${idx}\x00`;
    });
    // 2. 行级 + paragraph 渲染
    const lines = body.split("\n");
    const out = [];
    let paragraphBuf = [];
    let listBuf = [];   // {ord: bool, items: []}
    const flushParagraph = () => {
      if (paragraphBuf.length === 0) return;
      const text = paragraphBuf.join("\n");
      out.push(`<p>${applyInlineMd(text)}</p>`);
      paragraphBuf = [];
    };
    const flushList = () => {
      if (!listBuf.length) return;
      const ord = listBuf._ord;
      const tag = ord ? "ol" : "ul";
      out.push(`<${tag}>${listBuf.map((i) => `<li>${applyInlineMd(i)}</li>`).join("")}</${tag}>`);
      listBuf = [];
      listBuf._ord = false;
    };
    for (const line of lines) {
      // placeholder 行 → 直接放
      const phMatch = line.match(/^\x00CODEBLOCK(\d+)\x00$/);
      if (phMatch) {
        flushParagraph();
        flushList();
        const cb = codeBlocks[Number(phMatch[1])];
        out.push(`<pre class="codex-conv-md-code"><code>${escapeHtml(cb.code)}</code></pre>`);
        continue;
      }
      if (/^\s*$/.test(line)) {
        flushParagraph();
        flushList();
        continue;
      }
      // headings (#~######)
      const head = line.match(/^(#{1,6})\s+(.*)$/);
      if (head) {
        flushParagraph();
        flushList();
        const level = head[1].length;
        out.push(`<h${level}>${applyInlineMd(head[2])}</h${level}>`);
        continue;
      }
      // unordered / ordered list
      const ul = line.match(/^\s*[-*]\s+(.*)$/);
      const ol = line.match(/^\s*\d+\.\s+(.*)$/);
      if (ul || ol) {
        flushParagraph();
        const wantOrd = !!ol;
        if (listBuf._ord !== wantOrd && listBuf.length) {
          flushList();
        }
        listBuf._ord = wantOrd;
        listBuf.push((ul || ol)[1]);
        continue;
      }
      // blockquote
      const bq = line.match(/^>\s?(.*)$/);
      if (bq) {
        flushParagraph();
        flushList();
        out.push(`<blockquote>${applyInlineMd(bq[1])}</blockquote>`);
        continue;
      }
      // horizontal rule
      if (/^\s*(---|\*\*\*|___)\s*$/.test(line)) {
        flushParagraph();
        flushList();
        out.push("<hr>");
        continue;
      }
      // 默认聚合到段落
      flushList();
      paragraphBuf.push(line);
    }
    flushParagraph();
    flushList();
    return out.join("");
  }

  function applyInlineMd(text) {
    // 先 escape HTML,再在 escape 后的文本上跑 inline rule(安全)
    let s = escapeHtml(text);
    // inline code `code` — 占位防止内部 ** _ 被吃
    const inlineCodes = [];
    s = s.replace(/`([^`\n]+)`/g, (_, c) => {
      const idx = inlineCodes.push(c) - 1;
      return `\x01IC${idx}\x01`;
    });
    // links [text](url) — 仅 http(s)
    s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, (_, text, url) => {
      const safeUrl = url.replace(/"/g, "%22");
      return `<a href="${safeUrl}" target="_blank" rel="noreferrer noopener">${text}</a>`;
    });
    // bold **text** (non-greedy 防 `**a** **b**` 折叠成一段)
    s = s.replace(/\*\*([^*\n]+?)\*\*/g, "<strong>$1</strong>");
    // italic *text* / _text_(避开 ** 已处理后剩下的孤立 * + non-greedy
    // 让 `*a* and *b*` 渲染成两个 em 而非一段;devin #272 code-reviewer fix)
    s = s.replace(/(^|[\s(])\*([^*\n]+?)\*(?=[\s).,!?:;]|$)/g, "$1<em>$2</em>");
    s = s.replace(/(^|[\s(])_([^_\n]+?)_(?=[\s).,!?:;]|$)/g, "$1<em>$2</em>");
    // restore inline code
    // **devin #272 review fix**:inlineCodes 里的 content 是从 `escapeHtml(text)`
    // 输出的串里捕获的,已经是 escape 后的形态(`&lt;` `&amp;` 等)。再 escape
    // 一次会让 `&lt;` 变 `&amp;lt;`,用户看到 literal `&lt;` 而不是 `<`。
    // 直接拼回去即可,不可二次 escape。
    s = s.replace(/\x01IC(\d+)\x01/g, (_, i) => `<code>${inlineCodes[Number(i)]}</code>`);
    return s;
  }

  function truncateString(s, n) {
    if (!s || s.length <= n) return s || "";
    return `${s.slice(0, n)}\n… [前端预览截断,导出文件含完整内容]`;
  }

  async function codexConversationsExportSelected() {
    if (conversationsSelected.size === 0) return;
    const format = casDropdownGetValue($("#codexConvFormat")) || "markdown";
    const ids = Array.from(conversationsSelected);
    const isMulti = ids.length > 1;

    // 生成默认文件名
    const tsTag = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
    let defaultName;
    let extFilter;
    if (isMulti) {
      defaultName = `codex-conversations-${tsTag}.zip`;
      extFilter = { name: "Zip", extensions: ["zip"] };
    } else {
      const meta = conversationsCache.find((s) => s.id === ids[0]);
      const baseName = meta?.path?.split("/").pop()?.replace(/\.jsonl$/, "") || `session-${ids[0].slice(0, 8)}`;
      const ext = format === "markdown" ? "md" : format === "jsonl" ? "jsonl" : "json";
      defaultName = `${baseName}.${ext}`;
      extFilter = { name: ext.toUpperCase(), extensions: [ext] };
    }

    // 优先用「默认导出文件夹」(localStorage 持久化的);留空才弹 Tauri dialog.save()
    const defaultDir = codexConvLoadDefaultDir();
    let targetPath;
    if (defaultDir) {
      const sep = defaultDir.endsWith("/") || defaultDir.endsWith("\\") ? "" : "/";
      targetPath = `${defaultDir}${sep}${defaultName}`;
    } else {
      const dialog = window.__TAURI__?.dialog;
      if (!dialog?.save) {
        showToast(t("codex.conv.exportFailed") + ": Tauri dialog API 不可用");
        return;
      }
      try {
        targetPath = await dialog.save({
          title: isMulti ? (t("codex.conv.saveDialogMulti") || "保存对话 zip") : (t("codex.conv.saveDialogSingle") || "保存对话文件"),
          defaultPath: defaultName,
          filters: [extFilter],
        });
      } catch (e) {
        showToast(t("codex.conv.exportFailed") + ": " + (e.message || e));
        return;
      }
      if (!targetPath) return; // 用户取消
    }

    try {
      const resp = await fetch("/api/conversations/export", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          sessionIds: ids,
          format,
          options: conversationsExportOptions,
          targetPath,
        }),
      });
      if (!resp.ok) {
        const text = await resp.text();
        throw new Error(text || `HTTP ${resp.status}`);
      }
      // devin #272 silent-failure-hunter HIGH-5: 按 Content-Type 分支,backend
      // 在传 targetPath 时返 JSON,否则返二进制 body — 之前无脑 .json() 会
      // 把成功的二进制下载误判成"导出失败"
      const ct = resp.headers.get("content-type") || "";
      if (ct.includes("application/json")) {
        const data = await resp.json();
        showToast(tFmt("codex.conv.toastExportedTo", { count: ids.length, path: data.path }));
      } else {
        // HTTP body 下载分支(未指定 targetPath)— 浏览器 Content-Disposition 自动落盘
        showToast(tFmt("codex.conv.toastExported", { count: ids.length }));
      }
    } catch (e) {
      showToast(`${t("codex.conv.exportFailed") || "导出失败"}: ${e.message || e}`);
    }
  }

  // #271 fix #3 — 删除选中(移动到 trash,需要二次确认)
  async function codexConversationsDeleteSelected() {
    if (conversationsSelected.size === 0) return;
    const ids = Array.from(conversationsSelected);
    const confirmMsg = tFmt("codex.conv.confirmDelete", { count: ids.length });
    if (!window.confirm(confirmMsg)) return;
    try {
      const resp = await fetch("/api/conversations/delete", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ sessionIds: ids }),
      });
      if (!resp.ok) {
        const text = await resp.text();
        throw new Error(text || `HTTP ${resp.status}`);
      }
      const data = await resp.json();
      const moved = (data.deleted || []).length;
      const failedItems = data.failed || [];
      const failed = failedItems.length;
      conversationsSelected.clear();
      if (failed > 0) {
        showToast(tFmt("codex.conv.toastDeletedPartial", { moved, failed }));
        // devin #272 silent-failure-hunter MED-7: 暴露失败 reason 给用户而非
        // 只显示计数 — log + 弹 alert 列前 3 条让用户能 actionable
        console.warn("cas: delete failures", failedItems);
        const sample = failedItems.slice(0, 3)
          .map((f) => `  - ${f.sessionId}: ${f.reason}`).join("\n");
        const more = failed > 3 ? `\n  ... +${failed - 3} more (see console)` : "";
        window.alert(`${t("codex.conv.deleteFailureDetail") || "部分删除失败"}:\n${sample}${more}`);
      } else {
        showToast(tFmt("codex.conv.toastDeleted", { count: moved }));
      }
      await codexConversationsLoadAndRender();
    } catch (e) {
      showToast(`${t("codex.conv.deleteFailed") || "删除失败"}: ${e.message || e}`);
    }
  }

  function codexConversationsOpenOptionsDialog() {
    let dialog = $("#codexConvOptionsDialog");
    if (!dialog) {
      dialog = document.createElement("dialog");
      dialog.id = "codexConvOptionsDialog";
      dialog.className = "codex-conv-options-dialog";
      document.body.appendChild(dialog);
    }
    const o = conversationsExportOptions;
    dialog.innerHTML = `
      <h3>${t("codex.conv.optionsTitle") || "导出选项"}</h3>
      <label class="codex-conv-options-row">
        <input type="checkbox" id="optInclReasoning" ${o.includeReasoning ? "checked" : ""}>
        <span>${t("codex.conv.optIncludeReasoning") || "包含 reasoning 块"}</span>
      </label>
      <label class="codex-conv-options-row">
        <input type="checkbox" id="optInclTools" ${o.includeToolCalls ? "checked" : ""}>
        <span>${t("codex.conv.optIncludeToolCalls") || "包含 tool calls + outputs"}</span>
      </label>
      <label class="codex-conv-options-row">
        <input type="checkbox" id="optInclSystem" ${o.includeSystemPrompts ? "checked" : ""}>
        <span>${t("codex.conv.optIncludeSystem") || "包含 developer / system 消息"}</span>
      </label>
      <label class="codex-conv-options-row">
        <input type="checkbox" id="optRedact" ${o.redactSecrets ? "checked" : ""}>
        <span>${t("codex.conv.optRedact") || "Redact 密钥 (sk- / cas_ / JWT / Bearer)"}</span>
      </label>
      <label class="codex-conv-options-row">
        <span>${t("codex.conv.optToolMax") || "Tool output 截断字符数"}:</span>
        <input type="number" id="optToolMax" min="100" max="200000" step="256" value="${o.toolOutputMaxChars}" style="width: 100px;">
      </label>
      <div class="codex-conv-options-actions">
        <button type="button" class="btn btn-outline-primary btn-sm" id="optCancelBtn">${t("common.cancel") || "取消"}</button>
        <button type="button" class="btn btn-primary btn-sm" id="optSaveBtn">${t("common.save") || "保存"}</button>
      </div>
    `;
    $("#optCancelBtn").onclick = () => dialog.close();
    $("#optSaveBtn").onclick = () => {
      conversationsExportOptions = {
        includeReasoning: $("#optInclReasoning").checked,
        includeToolCalls: $("#optInclTools").checked,
        includeSystemPrompts: $("#optInclSystem").checked,
        redactSecrets: $("#optRedact").checked,
        toolOutputMaxChars: Math.max(100, Number($("#optToolMax").value) || 2048),
      };
      dialog.close();
      showToast(t("codex.conv.optionsSaved") || "选项已保存");
    };
    dialog.showModal();
  }

  // escapeHtml 复用 IIFE 顶部 line 107 的实现

  /** sidebar badge: 'ON' (managed) / 'OFF' / 数字(skills 数) */
  async function codexRefreshSidebarBadges() {
    try {
      const [agentsPaths, memPaths, mcpServers, mcpPlugins, skillsPaths, convs] = await Promise.all([
        fetch("/api/codex/agents-md/paths").then((r) => (r.ok ? r.json() : null)),
        fetch("/api/codex/memories-md/paths").then((r) => (r.ok ? r.json() : null)),
        fetch("/api/codex/mcp/servers").then((r) => (r.ok ? r.json() : null)),
        fetch("/api/codex/mcp/plugins").then((r) => (r.ok ? r.json() : null)),
        fetch("/api/codex/skills-md/paths").then((r) => (r.ok ? r.json() : null)),
        fetch("/api/conversations/list").then((r) => (r.ok ? r.json() : null)),
      ]);
      const setBadge = (id, text) => {
        const el = $(id);
        if (el) el.textContent = text;
      };
      const agentsCount = agentsPaths?.entries?.length || 0;
      const memCount = memPaths?.entries?.length || 0;
      const mcpServersCount = mcpServers?.servers?.length || 0;
      const mcpPluginsCount = mcpPlugins?.plugins?.length || 0;
      const mcpTotal = mcpServersCount + mcpPluginsCount;
      const skillsCount = skillsPaths?.entries?.length || 0;
      const convCount = convs?.sessions?.length || 0;
      setBadge("#codexSidebarBadge-agents", agentsCount > 0 ? String(agentsCount) : "—");
      setBadge("#codexSidebarBadge-memories", memCount > 0 ? String(memCount) : "—");
      setBadge("#codexSidebarBadge-mcp", mcpTotal > 0 ? String(mcpTotal) : "—");
      setBadge("#codexSidebarBadge-skills", skillsCount > 0 ? String(skillsCount) : "—");
      setBadge("#codexSidebarBadge-conversations", convCount > 0 ? String(convCount) : "—");
    } catch (e) {
      console.warn("cas: sidebar badges fetch failed", e);
    }
  }

  // ── #264 Codex Desktop Theme page ─────────────────────────────────
  let themeListCache = null;
  let selectedThemeId = null;

  /**
   * 1:1 crop 弹窗 — user 上传图后用来选 crop 区域。
   *
   * UI:全屏暗背景 modal + 中央"舞台"显示原图(等比 fit 进 stage),叠一个
   * 居中的方形 selection box(初始为 stage 短边 × 0.9)。
   * 交互:
   *   - 拖动:mousedown 在 stage 任意位置即开始(不限 box 内)→ 拖动 box 位置
   *     (clamp 到 stage 内)
   *   - 滚轮缩放:wheel up/down → box 边长 ±5%(min 40px 绝对值 / max stage 短边)
   *   - 确认:canvas.drawImage 把选区缩到 `min(2048, selectionPixels)` 方形 →
   *     toDataURL JPEG 92%
   *   - 取消 / 点遮罩 / 图片 decode 失败:resolve(null)
   *
   * @param {string} srcDataUri  原图 data:image/...;base64,...
   * @returns {Promise<string|null>}  cropped JPEG data URI;null = user 取消
   */
  function openCropModal(srcDataUri) {
    return new Promise((resolve) => {
      const lang = CCI18n && CCI18n.language === "en" ? "en" : "zh";
      const overlay = document.createElement("div");
      overlay.style.cssText = "position:fixed;inset:0;background:rgba(0,0,0,0.78);z-index:99999;display:flex;align-items:center;justify-content:center;flex-direction:column;";
      const panel = document.createElement("div");
      panel.style.cssText = "background:#1a1a1a;border:1px solid #444;border-radius:12px;padding:18px;max-width:90vw;max-height:90vh;display:flex;flex-direction:column;gap:12px;";
      const title = document.createElement("div");
      title.style.cssText = "color:#eee;font-size:15px;font-weight:600;";
      title.textContent = lang === "en"
        ? "Crop 1:1 (drag to move, scroll to zoom)"
        : "1:1 截取(拖动调整位置,滚轮缩放)";
      const stage = document.createElement("div");
      stage.style.cssText = "position:relative;background:#000;border-radius:6px;overflow:hidden;cursor:move;user-select:none;";
      const img = new Image();
      img.style.cssText = "display:block;max-width:70vw;max-height:65vh;width:auto;height:auto;pointer-events:none;";
      const box = document.createElement("div");
      box.style.cssText = "position:absolute;border:2px solid rgba(255,255,255,0.95);box-shadow:0 0 0 9999px rgba(0,0,0,0.55);box-sizing:border-box;pointer-events:none;";
      stage.appendChild(img);
      stage.appendChild(box);
      const btnRow = document.createElement("div");
      btnRow.style.cssText = "display:flex;justify-content:flex-end;gap:10px;";
      const cancelBtn = document.createElement("button");
      cancelBtn.className = "btn btn-outline-secondary btn-sm";
      cancelBtn.type = "button";
      cancelBtn.textContent = lang === "en" ? "Cancel" : "取消";
      const okBtn = document.createElement("button");
      okBtn.className = "btn btn-primary btn-sm";
      okBtn.type = "button";
      okBtn.textContent = lang === "en" ? "Use this crop" : "使用此截取";
      btnRow.appendChild(cancelBtn);
      btnRow.appendChild(okBtn);
      panel.appendChild(title);
      panel.appendChild(stage);
      panel.appendChild(btnRow);
      overlay.appendChild(panel);
      document.body.appendChild(overlay);

      // box 状态(相对 stage 像素 — 显示坐标),img.naturalW/H = 原始像素
      let boxX = 0, boxY = 0, boxSize = 0;
      let stageW = 0, stageH = 0;

      function clampBox() {
        if (boxSize > Math.min(stageW, stageH)) boxSize = Math.min(stageW, stageH);
        if (boxSize < 40) boxSize = 40;
        if (boxX < 0) boxX = 0;
        if (boxY < 0) boxY = 0;
        if (boxX + boxSize > stageW) boxX = stageW - boxSize;
        if (boxY + boxSize > stageH) boxY = stageH - boxSize;
      }
      function applyBox() {
        clampBox();
        box.style.left = boxX + "px";
        box.style.top = boxY + "px";
        box.style.width = boxSize + "px";
        box.style.height = boxSize + "px";
      }

      // OK 默认 disabled,等 img.onload 才放行 — 防 0x0 canvas 路径
      okBtn.disabled = true;
      okBtn.style.opacity = "0.5";
      img.onload = () => {
        stage.style.width = img.offsetWidth + "px";
        stage.style.height = img.offsetHeight + "px";
        stageW = img.offsetWidth;
        stageH = img.offsetHeight;
        boxSize = Math.min(stageW, stageH) * 0.9;
        boxX = (stageW - boxSize) / 2;
        boxY = (stageH - boxSize) / 2;
        applyBox();
        okBtn.disabled = false;
        okBtn.style.opacity = "";
      };
      img.onerror = () => {
        showToast(`${t("theme.uploadFailed") || "上传失败"}: ${lang === "en" ? "Image could not be decoded — try a different file" : "图片无法解码,请换一张"}`);
        done(null);
      };
      img.src = srcDataUri;

      // 拖动 + 滚轮缩放 — listener 显式 remove 在 done() 防 modal 多次打开累积 leak
      let dragging = false, dragOX = 0, dragOY = 0;
      const onMouseDown = (e) => {
        dragging = true;
        const r = stage.getBoundingClientRect();
        dragOX = e.clientX - r.left - boxX;
        dragOY = e.clientY - r.top - boxY;
        e.preventDefault();
      };
      const onMouseMove = (e) => {
        if (!dragging) return;
        const r = stage.getBoundingClientRect();
        boxX = e.clientX - r.left - dragOX;
        boxY = e.clientY - r.top - dragOY;
        applyBox();
      };
      const onMouseUp = () => { dragging = false; };
      const onWheel = (e) => {
        e.preventDefault();
        const cx = boxX + boxSize / 2;
        const cy = boxY + boxSize / 2;
        const delta = e.deltaY < 0 ? 1.05 : 0.95;
        boxSize = boxSize * delta;
        boxX = cx - boxSize / 2;
        boxY = cy - boxSize / 2;
        applyBox();
      };
      stage.addEventListener("mousedown", onMouseDown);
      window.addEventListener("mousemove", onMouseMove);
      window.addEventListener("mouseup", onMouseUp);
      stage.addEventListener("wheel", onWheel, { passive: false });

      function done(result) {
        window.removeEventListener("mousemove", onMouseMove);
        window.removeEventListener("mouseup", onMouseUp);
        overlay.remove();
        resolve(result);
      }
      cancelBtn.addEventListener("click", () => done(null));
      overlay.addEventListener("click", (e) => { if (e.target === overlay) done(null); });
      okBtn.addEventListener("click", () => {
        // 显示坐标 → 原图坐标
        const scaleX = img.naturalWidth / stageW;
        const scaleY = img.naturalHeight / stageH;
        const sx = boxX * scaleX;
        const sy = boxY * scaleY;
        const ssize = boxSize * scaleX; // 1:1 所以 X/Y scale 相同
        const outSize = Math.min(2048, Math.round(ssize)); // 不放大,只缩(或保持)
        const canvas = document.createElement("canvas");
        canvas.width = outSize;
        canvas.height = outSize;
        const ctx = canvas.getContext("2d");
        ctx.imageSmoothingQuality = "high";
        ctx.drawImage(img, sx, sy, ssize, ssize, 0, 0, outSize, outSize);
        const out = canvas.toDataURL("image/jpeg", 0.92);
        done(out);
      });
    });
  }

  async function renderTheme() {
    const container = $("#themeListContainer");
    const toggle = $("#codexUiThemeEnabled");
    const badge = $("#themeStatusBadge");
    if (!container || !toggle) return;

    // 1. 读 settings.codexUiThemeEnabled + codexUiTheme
    let settings;
    try {
      settings = await CCApi.getSettings();
    } catch (e) {
      settings = {};
    }
    toggle.checked = settings.codexUiThemeEnabled === true;
    selectedThemeId = settings.codexUiTheme || null;
    const hiddenIds = Array.isArray(settings.themeHiddenIds) ? settings.themeHiddenIds : [];

    // 2. 拉主题列表 — **每次 renderTheme 都重拉**(不缓存):避免 v1 cache-empty
    //    bug(一旦失败 set 成 [],之后永不重试)+ 主题列表 5-6 项;响应只含 640px
    //    preview base64(~40KB/张,5-6 张合计 ~250KB),走 webview 本地 IPC 延迟可忽略
    try {
      const res = await CCApi.theme.list();
      themeListCache = res.themes || [];
      if (themeListCache.length === 0) {
        console.warn("[theme] list returned empty:", res);
        showToast("主题列表为空 — 检查 backend route 是否注册");
      }
    } catch (e) {
      themeListCache = [];
      console.error("[theme] list failed:", e);
      showToast(`${t("theme.loadFailed") || "主题列表加载失败"}: ${e.message}`);
    }
    const lang = CCI18n && CCI18n.language === "en" ? "en" : "zh";

    // 3. 渲染主题卡(grid 4 列 + 缩略图)。inline onclick → 全局
    //    window.__themePickHandler,absolutely reliable(避开任何 event
    //    delegation / closure / listener-loss bug)。
    container.style.display = "grid";
    container.style.gridTemplateColumns = "repeat(4, 1fr)";
    container.style.gap = "14px";
    // 过滤掉已隐藏(themeHiddenIds 内的)— custom 不能被隐藏,只能 delete
    const visibleThemes = themeListCache.filter((th) => !hiddenIds.includes(th.id));

    // 顶部"已隐藏 N" + "恢复"入口(仅 N > 0 显示)
    const hiddenBadge = $("#themeHiddenBadge");
    const restoreBtn = $("#themeRestoreHidden");
    const hiddenCount = hiddenIds.length;
    if (hiddenBadge && restoreBtn) {
      if (hiddenCount > 0) {
        hiddenBadge.textContent = lang === "en"
          ? `${hiddenCount} hidden`
          : `已隐藏 ${hiddenCount} 个`;
        hiddenBadge.style.display = "";
        restoreBtn.style.display = "";
      } else {
        hiddenBadge.style.display = "none";
        restoreBtn.style.display = "none";
      }
    }
    const cards = visibleThemes.map((th) => {
      const displayName = lang === "en" ? th.displayNameEn : th.displayNameZh;
      const checked = th.id === selectedThemeId;
      const borderStyle = checked
        ? "border:2px solid var(--bs-primary);box-shadow:0 0 0 3px rgba(13,110,253,0.18);"
        : "border:1px solid var(--bs-border-color);";
      const checkBadge = checked ? `<span style="position:absolute;top:6px;left:8px;background:var(--bs-primary);color:#fff;font-size:11px;padding:2px 8px;border-radius:8px;pointer-events:none;z-index:2;">✓</span>` : "";
      const idEscaped = String(th.id).replace(/'/g, "\\'");
      const isCustom = th.id === "custom";
      // 右上"替换"小角标 — 仅 custom
      const replaceBadge = isCustom
        ? `<span class="theme-custom-replace" title="${escapeHtml(lang === "en" ? "Replace image" : "替换图片")}" style="position:absolute;top:6px;right:36px;background:rgba(0,0,0,0.55);color:#fff;font-size:11px;padding:2px 8px;border-radius:8px;cursor:pointer;z-index:3;" onclick="event.stopPropagation();window.__themeUploadHandler && window.__themeUploadHandler();">${escapeHtml(lang === "en" ? "Replace" : "替换")}</span>`
        : "";
      // 右上 X 删除按钮 — 每张都有。内置 = 隐藏(持久化 themeHiddenIds);custom = 真删 disk。
      const deleteBtn = `<span class="theme-delete-btn" title="${escapeHtml(isCustom ? (lang === "en" ? "Delete" : "删除") : (lang === "en" ? "Hide" : "隐藏"))}" style="position:absolute;top:6px;right:8px;background:rgba(0,0,0,0.55);color:#fff;font-size:14px;line-height:1;width:22px;height:22px;border-radius:11px;display:inline-flex;align-items:center;justify-content:center;cursor:pointer;z-index:3;" onclick="event.stopPropagation();window.__themeDeleteHandler && window.__themeDeleteHandler('${idEscaped}', ${isCustom});">×</span>`;
      return `
        <div class="card-theme-pick" style="position:relative;${borderStyle}border-radius:10px;overflow:hidden;cursor:pointer;display:flex;flex-direction:column;background:var(--bs-body-bg);transition:transform 0.12s ease, box-shadow 0.12s ease;user-select:none;" onclick="window.__themePickHandler && window.__themePickHandler('${idEscaped}')">
          ${checkBadge}
          ${replaceBadge}
          ${deleteBtn}
          <img src="${th.previewDataUri}" alt="${escapeHtml(displayName)}" style="width:100%;aspect-ratio:16/9;object-fit:cover;display:block;pointer-events:none;background:#1a1010;">
          <div style="padding:8px 10px;pointer-events:none;text-align:center;">
            <div style="font-weight:600;font-size:14px;">${escapeHtml(displayName)}</div>
          </div>
        </div>
      `;
    });

    // 末尾追加"+ 添加自定义"上传卡(仅当 visible 列表里还没 custom 时显示)
    const hasCustom = visibleThemes.some((th) => th.id === "custom");
    if (!hasCustom) {
      cards.push(`
        <div class="card-theme-add" style="position:relative;border:1.5px dashed var(--bs-border-color);border-radius:10px;overflow:hidden;cursor:pointer;display:flex;flex-direction:column;background:var(--bs-body-bg);user-select:none;" onclick="window.__themeUploadHandler && window.__themeUploadHandler();">
          <div style="width:100%;aspect-ratio:16/9;display:flex;align-items:center;justify-content:center;color:var(--bs-secondary-color);font-size:42px;background:linear-gradient(135deg,#1f1414,#2a1818);"><i class="bi bi-plus-circle"></i></div>
          <div style="padding:8px 10px;text-align:center;">
            <div style="font-weight:600;font-size:14px;color:var(--bs-secondary-color);">${escapeHtml(lang === "en" ? "Add custom" : "添加自定义")}</div>
          </div>
        </div>
      `);
    }
    container.innerHTML = cards.join("");

    // 5. 刷新 status badge
    try {
      const st = await CCApi.theme.status();
      const sObj = st.status;
      const autoReapplySelectedTheme = async () => {
        try {
          await CCApi.theme.apply(selectedThemeId);
          badge.textContent = `${t("theme.applied") || "已应用"}: ${selectedThemeId}`;
        } catch (err) {
          console.warn("[theme] auto-re-apply failed:", err);
          badge.textContent = `${t("theme.failed") || "失败"}: ${err.message || err}`;
        }
      };
      if (sObj && typeof sObj === "object") {
        if (sObj.Applied) {
          badge.textContent = `${t("theme.applied") || "已应用"}: ${sObj.Applied.theme_id}`;
          // 用户选了别的主题但后端还报旧 theme_id(切换 race / 重启后状态错位)→ 自动 re-apply。
          if (toggle.checked && selectedThemeId && sObj.Applied.theme_id !== selectedThemeId) {
            await autoReapplySelectedTheme();
          }
        } else if (sObj.Failed) {
          badge.textContent = `${t("theme.failed") || "失败"}: ${sObj.Failed.error}`;
          // 上一次 apply 失败但用户仍开着主题 + 有选中 → 自动重试一次。
          if (toggle.checked && selectedThemeId) {
            await autoReapplySelectedTheme();
          }
        } else badge.textContent = "";
      } else if (sObj === "Disabled") {
        badge.textContent = t("theme.disabled") || "未启用";
        // Auto re-apply: settings say enabled + theme selected, but backend
        // status is Disabled (e.g. after transfer app restart / Codex restart).
        // Apply immediately so user doesn't have to manually toggle.
        if (toggle.checked && selectedThemeId) {
          await autoReapplySelectedTheme();
        }
      }
    } catch (e) {
      badge.textContent = "";
    }
  }

  // bind toggle + reload/restart + card click(delegation)一次性,避免 renderTheme 反复绑定丢
  let themeEventsBound = false;
  function bindThemeEvents() {
    if (themeEventsBound) return;
    themeEventsBound = true;

    // toggle 直接 apply/clear,不需要按 Apply 按钮。
    //
    // **对称语义**(R1 / R-NEW-2):on / off 两条路径都遵守"先做副作用(apply/clear),
    // success 才 persist settings;失败 rollback toggle DOM 状态"— 否则 settings
    // 跟 Codex CSS / toggle DOM 三方会 desync。
    $("#codexUiThemeEnabled")?.addEventListener("change", async (e) => {
      if (e.target.checked) {
        if (!selectedThemeId) {
          showToast(t("theme.pickFirst") || "请先选一个主题再开启");
          e.target.checked = false;
          return;
        }
        // apply 先,success 后才 saveSettings — 失败则 toggle 弹回 off,settings 保留不变
        try {
          await CCApi.theme.apply(selectedThemeId);
          await CCApi.saveSettings({ codexUiThemeEnabled: true, codexUiTheme: selectedThemeId });
          showToast(t("theme.appliedToast") || "主题已应用");
        } catch (err) {
          e.target.checked = false;
          showToast(`${t("theme.applyFailed") || "应用失败"}: ${err.message}`);
        }
      } else {
        // **clear 先,success 后才 saveSettings** — clear 失败(Codex 不在跑 / CDP 不可达)
        // 时,如果先把 enabled=false 写进 settings,user 看见 toast 失败但 toggle
        // 已弹回开,设置实际已变 disabled → 下次启动 Codex 不会 auto-apply,旧 CSS
        // 残留在 Codex DOM 直到 Codex 重启;state 跟 settings 长期不同步。
        try {
          await CCApi.theme.clear();
          await CCApi.saveSettings({ codexUiThemeEnabled: false });
          showToast(t("theme.clearedToast") || "主题已清除");
        } catch (err) {
          // clear 失败 → settings 保留 enabled=true,toggle 弹回 checked 状态保持一致
          e.target.checked = true;
          showToast(`${t("theme.clearFailed") || "清除失败"}: ${err.message}`);
        }
      }
      await renderTheme();
    });


    // Restart Codex.app(完全 quit + 重启,走 transfer 已有 endpoint)
    $("[data-action=theme-restart-codex]")?.addEventListener("click", async () => {
      try {
        await CCApi.theme.restartCodex();
        showToast(t("theme.restartToast") || "已请求重启 Codex");
      } catch (err) {
        showToast(`${t("theme.restartFailed") || "重启失败"}: ${err.message}`);
      }
    });

    // "+ 添加自定义" / "替换" — 全局 fn `window.__themeUploadHandler`。
    // 流程:file picker → FileReader.readAsDataURL → **弹 1:1 crop 弹窗**让 user
    // 选 crop 区域 → canvas crop 出方形 JPEG → POST 给后端 → renderTheme + 自动
    // 选中 custom + apply。
    //
    // crop 在前端完成(canvas):后端 save_custom_theme 收到已是方形图,只做
    // resize + JPEG encode 不再二次 crop;user 可拖框 + 滚轮 zoom 自由选定。
    window.__themeUploadHandler = async () => {
      const input = document.createElement("input");
      input.type = "file";
      input.accept = "image/jpeg,image/png,image/jpg";
      input.style.display = "none";
      document.body.appendChild(input);
      input.addEventListener("change", async () => {
        const file = input.files && input.files[0];
        input.remove();
        if (!file) return;
        if (file.size > 20 * 1024 * 1024) {
          showToast(t("theme.uploadTooLarge") || "图片过大(>20MB)");
          return;
        }
        try {
          const srcDataUri = await new Promise((resolve, reject) => {
            const r = new FileReader();
            r.onload = () => resolve(r.result);
            r.onerror = () => reject(r.error);
            r.readAsDataURL(file);
          });
          // 弹 1:1 crop modal,user 确认后返 cropped data URI(JPEG)
          const croppedDataUri = await openCropModal(srcDataUri);
          if (!croppedDataUri) return; // user cancel
          await CCApi.theme.uploadCustom(croppedDataUri);
          showToast(t("theme.uploadOk") || "自定义主题已保存");
          themeListCache = null;
          await renderTheme();
          await window.__themePickHandler("custom");
        } catch (err) {
          console.error("[theme] upload failed:", err);
          showToast(`${t("theme.uploadFailed") || "上传失败"}: ${err.message || err}`);
        }
      });
      input.click();
    };

    // 删除 / 隐藏卡 — 全局 fn `window.__themeDeleteHandler`。内置 = 隐藏(写
    // settings.themeHiddenIds),custom = 真删 disk(API + 切默认主题)。
    window.__themeDeleteHandler = async (themeId, isCustom) => {
      const lang2 = CCI18n && CCI18n.language === "en" ? "en" : "zh";
      const confirmMsg = isCustom
        ? (lang2 === "en" ? "Delete custom theme image? This cannot be undone." : "确认删除自定义主题图片?此操作不可恢复。")
        : (lang2 === "en" ? `Hide theme "${themeId}"? You can restore from the top of the page.` : `隐藏主题"${themeId}"?顶部可"恢复隐藏"。`);
      if (!confirm(confirmMsg)) return;
      try {
        let curSettings;
        try { curSettings = await CCApi.getSettings(); } catch { curSettings = {}; }
        const curSelected = curSettings.codexUiTheme;
        const curEnabled = curSettings.codexUiThemeEnabled === true;
        // 共用 fallback 选择器:返 `{ id, unhide }` 二元组。优先找已 visible 的内置;
        // 找不到(carton 都被隐藏 + 删 custom)→ 强制选第一个内置 + 把它从 hidden 列表
        // 移除(`unhide=true`),确保 selected card 在 grid 可见。**绝不**返个还在
        // hidden 列表里的 id 给 caller — 那会让 selected 卡在不可见状态。
        const pickFallback = (hiddenList) => {
          const visible = themeListCache.find(th =>
            th.id !== "custom" && th.id !== themeId && !hiddenList.includes(th.id)
          );
          if (visible) return { id: visible.id, unhide: null };
          // 全 hidden 的极端 case:挑第一个非 custom 内置,自动 unhide
          const anyBuiltin = themeListCache.find(th => th.id !== "custom" && th.id !== themeId);
          const id = anyBuiltin ? anyBuiltin.id : "carton";
          return { id, unhide: id };
        };
        if (isCustom) {
          await CCApi.theme.deleteCustom();
          if (curSelected === "custom") {
            const hidden = Array.isArray(curSettings.themeHiddenIds) ? curSettings.themeHiddenIds.slice() : [];
            const fb = pickFallback(hidden);
            const patch = { codexUiTheme: fb.id };
            // 极端 case 自动 unhide fallback 保证 selected 在 grid 可见
            if (fb.unhide) {
              patch.themeHiddenIds = hidden.filter(id => id !== fb.unhide);
            }
            await CCApi.saveSettings(patch);
            if (curEnabled) {
              try {
                await CCApi.theme.apply(fb.id);
              } catch (e) {
                console.error("[theme] post-delete apply failed:", e);
                showToast(`${t("theme.applyFailed") || "应用失败"}: ${e.message || e} — 请重启 Codex 看效果`);
              }
            }
          }
        } else {
          const hidden = Array.isArray(curSettings.themeHiddenIds) ? curSettings.themeHiddenIds.slice() : [];
          if (!hidden.includes(themeId)) hidden.push(themeId);
          const patch = { themeHiddenIds: hidden };
          if (curSelected === themeId) {
            const fb = pickFallback(hidden);
            patch.codexUiTheme = fb.id;
            if (fb.unhide) {
              patch.themeHiddenIds = hidden.filter(id => id !== fb.unhide);
            }
            if (curEnabled) {
              try {
                await CCApi.theme.apply(fb.id);
              } catch (e) {
                console.error("[theme] post-hide apply failed:", e);
                showToast(`${t("theme.applyFailed") || "应用失败"}: ${e.message || e} — 请重启 Codex 看效果`);
              }
            }
          }
          await CCApi.saveSettings(patch);
        }
        themeListCache = null;
        await renderTheme();
      } catch (err) {
        console.error("[theme] delete failed:", err);
        showToast(err.message || String(err));
      }
    };

    // 顶部"恢复隐藏" — 清空 themeHiddenIds + 重渲染
    $("#themeRestoreHidden")?.addEventListener("click", async () => {
      try {
        await CCApi.saveSettings({ themeHiddenIds: [] });
        themeListCache = null;
        await renderTheme();
      } catch (err) {
        showToast(err.message || String(err));
      }
    });

    // 卡片点击 — 全局 fn `window.__themePickHandler`,inline onclick 触发。
    // 避开任何 event delegation / closure / listener-loss bug,绝对 reliable。
    //
    // 热更新(#264):toggle 开 + 点卡片 → save settings → apply → 立即切换
    // 主题(IIFE 进来先 remove 旧 style + mascot 再 inject 新的,**不需要**
    // reload Codex page;reload 会扰乱当前对话 React state)。
    window.__themePickHandler = async (themeId) => {
      console.log("[theme] pick", themeId);
      selectedThemeId = themeId;
      const enabled = $("#codexUiThemeEnabled")?.checked;
      try {
        if (enabled) {
          await CCApi.saveSettings({ codexUiThemeEnabled: true, codexUiTheme: themeId });
          await CCApi.theme.apply(themeId);
          showToast(t("theme.appliedToast") || "主题已应用");
        } else {
          // toggle 关时,只持久化 user 的选择(不调 apply,toggle 开时再 apply)
          await CCApi.saveSettings({ codexUiTheme: themeId });
        }
      } catch (err) {
        console.error("[theme] pick failed:", err);
        showToast(err.message);
      }
      await renderTheme();
    };
  }

  async function renderCodexAssets() {
    const sidebar = $("#codexSidebar");
    const initialTab = currentCodexTab();
    codexShowTab(initialTab);
    await codexLoadTab(initialTab);

    // textarea dirty 标记: user 编辑后 status 重 load 不覆盖
    const ta = $("#codexBlockContent");
    if (ta && !ta.dataset.bound) {
      ta.dataset.bound = "1";
      ta.addEventListener("input", () => (ta.dataset.dirty = "1"));
    }

    // AGENTS.md 路径 picker:toggle button click + menu item click + outside click
    const pathToggle = $("#codexAgentsPathToggle");
    const pathMenu = $("#codexAgentsPathMenu");
    if (pathToggle && !pathToggle.dataset.bound) {
      pathToggle.dataset.bound = "1";
      pathToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        codexAgentsTogglePicker();
      });
    }
    if (pathMenu && !pathMenu.dataset.bound) {
      pathMenu.dataset.bound = "1";
      pathMenu.addEventListener("click", (e) => {
        const li = e.target.closest(".codex-path-picker-item");
        if (!li || li.getAttribute("aria-disabled") === "true") return;
        const hash = li.dataset.hash;
        if (hash) codexAgentsSelectHash(hash);
      });
    }
    if (!document.body.dataset.codexPathPickerOutsideBound) {
      document.body.dataset.codexPathPickerOutsideBound = "1";
      document.addEventListener("click", (e) => {
        const aPicker = $("#codexAgentsPathPicker");
        if (aPicker && !aPicker.contains(e.target)) codexAgentsClosePicker();
        const mPicker = $("#codexMemoriesPathPicker");
        if (mPicker && !mPicker.contains(e.target)) codexMemoriesClosePicker();
        const sPicker = $("#codexSkillsPathPicker");
        if (sPicker && !sPicker.contains(e.target)) codexSkillsClosePicker();
      });
    }

    // Memories picker
    const memToggle = $("#codexMemoriesPathToggle");
    const memMenu = $("#codexMemoriesPathMenu");
    if (memToggle && !memToggle.dataset.bound) {
      memToggle.dataset.bound = "1";
      memToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        codexMemoriesTogglePicker();
      });
    }
    if (memMenu && !memMenu.dataset.bound) {
      memMenu.dataset.bound = "1";
      memMenu.addEventListener("click", (e) => {
        const li = e.target.closest(".codex-path-picker-item");
        if (!li || li.getAttribute("aria-disabled") === "true") return;
        const hash = li.dataset.hash;
        if (hash) codexMemoriesSelectHash(hash);
      });
    }

    // Skills picker
    const skToggle = $("#codexSkillsPathToggle");
    const skMenu = $("#codexSkillsPathMenu");
    if (skToggle && !skToggle.dataset.bound) {
      skToggle.dataset.bound = "1";
      skToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        codexSkillsTogglePicker();
      });
    }
    if (skMenu && !skMenu.dataset.bound) {
      skMenu.dataset.bound = "1";
      skMenu.addEventListener("click", (e) => {
        const li = e.target.closest(".codex-path-picker-item");
        if (!li || li.getAttribute("aria-disabled") === "true") return;
        const hash = li.dataset.hash;
        if (hash) codexSkillsSelectHash(hash);
      });
    }

    // 添加 modal:Enter 确认,Esc 取消
    const addInput = $("#codexAddPathInput");
    if (addInput && !addInput.dataset.bound) {
      addInput.dataset.bound = "1";
      addInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          codexAgentsConfirmPathAdd();
        } else if (e.key === "Escape") {
          e.preventDefault();
          codexAgentsClosePathModal();
        }
      });
    }
    // modal backdrop 点击关闭
    const modalBackdrop = $("#codexAddPathModal");
    if (modalBackdrop && !modalBackdrop.dataset.bound) {
      modalBackdrop.dataset.bound = "1";
      modalBackdrop.addEventListener("click", (e) => {
        if (e.target === modalBackdrop) codexAgentsClosePathModal();
      });
    }

    // History modal:toggle picker + menu item click + backdrop close + Esc
    const histToggle = $("#codexHistoryToggle");
    const histMenu = $("#codexHistoryMenu");
    if (histToggle && !histToggle.dataset.bound) {
      histToggle.dataset.bound = "1";
      histToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        codexHistoryPickerToggle();
      });
    }
    if (histMenu && !histMenu.dataset.bound) {
      histMenu.dataset.bound = "1";
      histMenu.addEventListener("click", (e) => {
        const li = e.target.closest(".codex-path-picker-item");
        if (!li || li.getAttribute("aria-disabled") === "true") return;
        const idx = Number(li.dataset.historyIdx);
        if (Number.isFinite(idx)) codexHistorySelect(idx);
      });
    }
    const histModal = $("#codexHistoryModal");
    if (histModal && !histModal.dataset.bound) {
      histModal.dataset.bound = "1";
      histModal.addEventListener("click", (e) => {
        if (e.target === histModal) codexHistoryClose();
      });
    }
    if (!document.body.dataset.codexHistoryEscBound) {
      document.body.dataset.codexHistoryEscBound = "1";
      document.addEventListener("keydown", (e) => {
        if (e.key === "Escape") {
          const m = $("#codexHistoryModal");
          if (m && !m.hidden) codexHistoryClose();
        }
      });
    }

    // sidebar click → 切 tab + lazy load
    if (sidebar && !sidebar.dataset.bound) {
      sidebar.dataset.bound = "1";
      sidebar.addEventListener("click", async (evt) => {
        const btn = evt.target.closest(".codex-sidebar-item");
        if (!btn) return;
        const tab = btn.dataset.codexTab;
        if (!tab) return;
        if (ta) delete ta.dataset.dirty;
        codexShowTab(tab);
        await codexLoadTab(tab);
      });
    }

    // MCP sub-nav 切换
    const mcpSubnav = $("#codexMcpSubnav");
    if (mcpSubnav && !mcpSubnav.dataset.bound) {
      mcpSubnav.dataset.bound = "1";
      mcpSubnav.addEventListener("click", async (evt) => {
        const btn = evt.target.closest(".codex-mcp-subnav-item");
        if (!btn) return;
        const sub = btn.dataset.mcpSub;
        if (!sub) return;
        await codexMcpOpenSubpane(sub);
      });
    }

    // MCP servers list item click → 选 server
    const mcpServersList = $("#codexMcpServersList");
    if (mcpServersList && !mcpServersList.dataset.bound) {
      mcpServersList.dataset.bound = "1";
      mcpServersList.addEventListener("click", (evt) => {
        const li = evt.target.closest(".codex-mcp-list-item");
        if (!li) return;
        const name = li.dataset.server;
        if (!name) return;
        codexMcpCurrentServerName = name;
        codexMcpJsonEditMode = false;
        codexMcpJsonDraft = "";
        codexMcpPendingNewName = null;
        codexMcpRenderServersList();
        codexMcpRenderForm();
      });
    }

    // MCP marketplace search input
    const mcpSearch = $("#codexMcpMarketSearch");
    if (mcpSearch && !mcpSearch.dataset.bound) {
      mcpSearch.dataset.bound = "1";
      mcpSearch.addEventListener("input", () => {
        codexMcpMarketFilter = mcpSearch.value;
        codexMcpRenderMarketIndex();
      });
    }

    // MCP modal backdrop close
    const mcpAddSourceModal = $("#codexMcpAddSourceModal");
    if (mcpAddSourceModal && !mcpAddSourceModal.dataset.bound) {
      mcpAddSourceModal.dataset.bound = "1";
      mcpAddSourceModal.addEventListener("click", (e) => {
        if (e.target === mcpAddSourceModal) codexMcpSourceAddClose();
      });
    }
    const mcpDeeplinkModal = $("#codexMcpDeeplinkModal");
    if (mcpDeeplinkModal && !mcpDeeplinkModal.dataset.bound) {
      mcpDeeplinkModal.dataset.bound = "1";
      mcpDeeplinkModal.addEventListener("click", (e) => {
        if (e.target === mcpDeeplinkModal) codexMcpDeeplinkCancel();
      });
    }

    const mcpNewModal = $("#codexMcpNewServerModal");
    if (mcpNewModal && !mcpNewModal.dataset.bound) {
      mcpNewModal.dataset.bound = "1";
      mcpNewModal.addEventListener("click", (e) => {
        if (e.target === mcpNewModal) codexMcpServerNewCancel();
      });
      // Enter 直接 confirm
      $("#codexMcpNewServerNameInput")?.addEventListener("keydown", (e) => {
        if (e.key === "Enter") codexMcpServerNewConfirm();
        if (e.key === "Escape") codexMcpServerNewCancel();
      });
    }
  }

  async function fillPreset(presetId) {
    if (!presetCache.length) presetCache = await CCApi.getPresets();
    const preset = presetCache.find((item) => item.id === presetId);
    if (!preset) return;
    editingProviderId = null;
    applyPresetToForm(preset);
  }

  // ── 用户反馈 modal ───────────────────────────────────────────────
  let feedbackAttachments = [];  // [{name, size, file}]
  let feedbackBsModal = null;

  function openFeedbackModal() {
    const el = $("#feedbackModal");
    if (!el) return;
    // 重置表单
    $("#feedbackTitle").value = "";
    $("#feedbackContactEmail").value = "";
    $("#feedbackBody").value = "";
    $("#feedbackIncludeDiagnostics").checked = true;
    feedbackAttachments = [];
    renderFeedbackAttachments();
    if (!feedbackBsModal) feedbackBsModal = new bootstrap.Modal(el);
    feedbackBsModal.show();
  }

  function renderFeedbackAttachments() {
    const list = $("#feedbackAttachmentList");
    if (!list) return;
    list.innerHTML = feedbackAttachments
      .map((a, i) => `<li class="feedback-attachment-item"><span>${escapeHtml(a.name)}</span><small>${formatBytes(a.size)}</small><button type="button" class="btn-link" data-idx="${i}">×</button></li>`)
      .join("");
    list.querySelectorAll("button[data-idx]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const idx = Number(btn.dataset.idx);
        feedbackAttachments.splice(idx, 1);
        renderFeedbackAttachments();
      });
    });
  }

  function addFeedbackFiles(files) {
    if (!files || !files.length) return;
    const max = 5 * 1024 * 1024;
    for (const f of files) {
      if (f.size > max) {
        showToast(tFmt("feedback.tooLargeFile", { name: f.name }));
        continue;
      }
      feedbackAttachments.push({ name: f.name, size: f.size, file: f });
    }
    renderFeedbackAttachments();
  }

  function formatBytes(n) {
    if (n < 1024) return `${n}B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
    return `${(n / 1024 / 1024).toFixed(2)}MB`;
  }

  async function submitFeedback() {
    const titleEl = $("#feedbackTitle");
    const contactEmailEl = $("#feedbackContactEmail");
    const bodyEl = $("#feedbackBody");
    const submitBtn = $("#feedbackSubmitBtn");
    if (!bodyEl) return;

    const title = (titleEl?.value || "").trim();
    const contactEmail = (contactEmailEl?.value || "").trim();
    const body = bodyEl.value.trim();
    if (!body) {
      showToast(t("feedback.bodyRequired"));
      bodyEl.focus();
      return;
    }

    submitBtn.disabled = true;
    const originalText = submitBtn.textContent;
    submitBtn.textContent = t("feedback.submitting");

    try {
      // 把附件转成 base64 嵌进 JSON,避开 pywebview WebKit 对 FormData 的 bug
      const attachments = [];
      for (const a of feedbackAttachments) {
        try {
          const b64 = await fileToBase64(a.file);
          const isImg = /^image\//.test(a.file.type || "");
          const safeName = String(a.name || `attachment-${Date.now()}.bin`)
            .replace(/[\x00-\x1f\\/]/g, "_")
            .slice(0, 200);
          attachments.push({
            kind: isImg ? "screenshot" : "log",
            name: safeName,
            content_type: a.file.type || "application/octet-stream",
            content_b64: b64,
          });
        } catch (innerErr) {
          console.warn("[feedback] skipped attachment:", innerErr, a);
        }
      }

      const payload = {
        title,
        contact_email: contactEmail,
        body,
        include_diagnostics: $("#feedbackIncludeDiagnostics").checked,
        attachments,
      };

      const result = await CCApi.submitFeedback(payload);
      if (feedbackBsModal) feedbackBsModal.hide();
      showToast(tFmt("feedback.successToast", { id: result.id || "" }));
    } catch (err) {
      console.error("[feedback] submit failed:", err);
      let msg = err && err.message ? err.message : String(err);
      if (msg.includes("did not match the expected pattern")) {
        msg = "请求体构造异常,请重试或去掉附件";
      }
      showToast(tFmt("feedback.failToast", { message: msg }));
    } finally {
      submitBtn.disabled = false;
      submitBtn.textContent = originalText;
    }
  }

  function fileToBase64(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const r = String(reader.result || "");
        const i = r.indexOf(",");
        resolve(i >= 0 ? r.slice(i + 1) : r);
      };
      reader.onerror = () => reject(reader.error || new Error("FileReader failed"));
      reader.readAsDataURL(file);
    });
  }

  function bindFeedbackEvents() {
    const dropzone = $("#feedbackDropzone");
    const fileInput = $("#feedbackFiles");
    if (dropzone && fileInput) {
      dropzone.addEventListener("click", (e) => {
        // 不要在点击删除按钮 / 列表项时触发
        if (e.target.closest(".feedback-attachment-item")) return;
        fileInput.click();
      });
      fileInput.addEventListener("change", () => {
        addFeedbackFiles(Array.from(fileInput.files));
        fileInput.value = "";
      });
      dropzone.addEventListener("dragover", (e) => {
        e.preventDefault();
        dropzone.classList.add("dragover");
      });
      dropzone.addEventListener("dragleave", () => dropzone.classList.remove("dragover"));
      dropzone.addEventListener("drop", (e) => {
        e.preventDefault();
        dropzone.classList.remove("dragover");
        addFeedbackFiles(Array.from(e.dataTransfer.files));
      });
    }
    document.addEventListener("paste", (e) => {
      // 粘贴截图(只有 modal 打开时响应)
      const modalEl = $("#feedbackModal");
      if (!modalEl?.classList.contains("show")) return;
      const items = e.clipboardData?.items || [];
      for (const it of items) {
        if (it.kind === "file" && /^image\//.test(it.type)) {
          const f = it.getAsFile();
          if (f) addFeedbackFiles([new File([f], f.name || `pasted-${Date.now()}.png`, { type: f.type })]);
        }
      }
    });
    const submitBtn = $("#feedbackSubmitBtn");
    if (submitBtn) submitBtn.addEventListener("click", submitFeedback);
  }

  function bindEvents() {
    window.addEventListener("hashchange", () => renderRoute(routeFromHash()));
    window.addEventListener("cc:i18n", () => renderRoute(routeFromHash()));
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
      if (currentTheme === "dark") applyTheme("dark");
    });

    document.addEventListener("click", async (event) => {
      if (!event.target.closest(".mapping-slot-menu-wrap")) {
        closeProviderSlotMenu();
      }
      if (!event.target.closest(".baseurl-input-wrap")) {
        closeBaseUrlMenu();
      }
      if (!event.target.closest(".provider-model-input-wrap")) {
        closeProviderModelMenu();
      }
      const langButton = event.target.closest("[data-lang]");
      if (langButton) {
        const lang = langButton.dataset.lang;
        CCI18n.apply(lang);
        // 落盘后端 settings,重启时 getSettings 能读回,避免回退默认语言 (MOC-70)
        await CCApi.saveSettings({ language: lang });
      }
      const addLink = event.target.closest("a[href='#providers/add']");
      if (addLink) {
        editingProviderId = null;
        selectedPreset = null;
        updatePresetSelection();
      }
      const themeButton = event.target.closest("[data-theme-action]");
      if (themeButton) {
        const nextTheme = applyTheme(themeButton.dataset.themeAction);
        await CCApi.saveSettings({ theme: nextTheme });
      }
      const presetButton = event.target.closest("[data-preset]");
      if (presetButton && presetButton.closest("#presetList")) {
        event.preventDefault();
        await fillPreset(presetButton.dataset.preset);
        return;
      }
      const presetModelOption = event.target.closest("[data-preset-model-option]");
      if (presetModelOption) {
        applyPresetModelOption(presetModelOption.dataset.presetModelOption, presetModelOption.checked);
        return;
      }
      await handleAction(event.target);
    });

    document.addEventListener("change", (event) => {
      const mappingInput = event.target.closest("[data-provider-model-input]");
      if (mappingInput) {
        updateProviderModelInput(mappingInput.dataset.providerModelInput, mappingInput.value);
        renderPresetOptions(selectedPreset, collectProviderMappings());
      }
      if (event.target.id === "providerBaseUrl") {
        renderBaseUrlOptions();
      }
      if (event.target.id === "providerApiFormatSelect") {
        updateApiFormatSelectDetail(event.target.value);
        formApiFormatValue = event.target.value;
        // R1 PR-7:切换 apiFormat 时同步 OAuth / grok_web row 显隐
        setOauthRowState(event.target.value);
        setGrokWebRowState(event.target.value);
      }
    });

    // P2.2 OAuth login/logout buttons —— delegate via closest() 防 future 嵌套
    // <i> icon 时 event.target 是 <i> 而 .id 为空导致 dead button (silent-failure L1 修)
    document.addEventListener("click", (event) => {
      if (event.target?.closest?.("#oauthLoginBtn")) {
        handleOauthLogin();
      } else if (event.target?.closest?.("#oauthLogoutBtn")) {
        handleOauthLogout();
      }
    });

    document.addEventListener("input", (event) => {
      if (event.target.id === "providerBaseUrl") {
        renderBaseUrlOptions();
      }
      const customLabelInput = event.target.closest("[data-custom-model-label]");
      if (customLabelInput) {
        const customKey = customLabelInput.dataset.customModelLabel;
        providerFormCustomLabels[customKey] = customLabelInput.value;
      }
      const mappingInput = event.target.closest("[data-provider-model-input]");
      if (!mappingInput) return;
      updateProviderModelInput(mappingInput.dataset.providerModelInput, mappingInput.value);
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        closeBaseUrlMenu();
        closeProviderSlotMenu();
        closeProviderModelMenu();
      }
    });

    $("#providerForm").addEventListener("submit", async (event) => {
      event.preventDefault();
      try {
        const wasEditing = !!editingProviderId;
        await saveProviderFromForm();
        if (editingProviderId) {
          showToast(wasEditing ? t("toast.providerUpdated") : t("toast.providerSaved"));
        } else {
          showToast(t("toast.providerSaved"));
        }
        editingProviderId = null;
        selectedPreset = null;
        window.location.hash = "providers";
      } catch (error) {
        console.error(error);
        showToast(error.message || t("toast.requestFailed"));
      }
    });

    $("#modelProvider")?.addEventListener("change", renderMappingCards);
    $("#settingsProxyPort").addEventListener("change", saveSettingsFromForm);
    $("#settingsAdminPort").addEventListener("change", saveSettingsFromForm);
    $("#settingsUpdateUrl").addEventListener("change", saveSettingsFromForm);
    $("#autoApplyOnStart")?.addEventListener("change", saveSettingsFromForm);
   $("#autoUnlockCodexPlugins")?.addEventListener("change", saveSettingsFromForm);
    $("#autoWakeCodexPet")?.addEventListener("change", saveSettingsFromForm);
    $("#mcpCredentialsPortableStore")?.addEventListener("change", saveSettingsFromForm);

   // Plugin Unlock 按钮事件
    $("[data-action=plugin-unlock-start]")?.addEventListener("click", async () => {
      try {
        await CCApi.pluginUnlock.start();
        showToast(t("pluginUnlock.started") || "解锁服务已启动");
        setTimeout(refreshPluginUnlockStatus, 1000);
      } catch (e) { showToast(e.message); }
    });
    $("[data-action=plugin-unlock-stop]")?.addEventListener("click", async () => {
      try {
        await CCApi.pluginUnlock.stop();
        showToast(t("pluginUnlock.stopped") || "解锁服务已停止");
        setTimeout(refreshPluginUnlockStatus, 500);
      } catch (e) { showToast(e.message); }
    });
    $("[data-action=plugin-unlock-reinject]")?.addEventListener("click", async () => {
      try {
        await CCApi.pluginUnlock.reinject();
        showToast(t("pluginUnlock.reinjecting") || "正在重新注入...");
        setTimeout(refreshPluginUnlockStatus, 1500);
      } catch (e) { showToast(e.message); }
    });
    $("#exposeAllProviderModels").addEventListener("change", saveSettingsFromForm);
    $("#showGrayProviders")?.addEventListener("change", async () => {
      // MOC-91:更新展示过滤缓存 + 持久化。设置页当前不展示 preset,无需即时重渲染;
      // 下次进「添加 provider」/ dashboard 时 visiblePresets() 即按新值过滤。
      showGrayPresets = $("#showGrayProviders")?.checked === true;
      await saveSettingsFromForm();
    });
    $("#restoreCodexOnExit")?.addEventListener("change", saveSettingsFromForm);
    $("#codexNetworkAccess")?.addEventListener("change", saveSettingsFromForm);
    $("#codexStatusSectionDefaultVisible")?.addEventListener("change", saveSettingsFromForm);
    $("#configImportFile")?.addEventListener("change", (event) => {
      importConfigFile(event.target.files?.[0]);
    });
    $("#restartReminderLater")?.addEventListener("click", dismissRestartReminderLater);
    $("#restartReminderNow")?.addEventListener("click", () => restartCodexAppNow());
    $("#autoUnlockRestartCodex")?.addEventListener("click", () =>
      restartCodexAppNow({
        buttonId: "autoUnlockRestartCodex",
        fallbackLabelKey: "settings.autoUnlockRestartCodex",
        hideModal: false,
      })
    );

    $("#confirmDelete").addEventListener("click", async () => {
      if (!pendingDeleteId) return;
      try {
        await CCApi.deleteProvider(pendingDeleteId);
        pendingDeleteId = null;
        deleteModal.hide();
        if (routeFromHash() === "dashboard") {
          await renderDashboard();
        } else {
          await renderProviders();
        }
        showToast(t("toast.providerDeleted"));
      } catch (error) {
        console.error(error);
        showToast(error.message || t("toast.requestFailed"));
      }
    });
  }

  document.addEventListener("DOMContentLoaded", async () => {
    // **M3 i18n apply 时序修**:第一时间用 localStorage cache + navigator.language
    // 同步 apply 一次,消除 DOMContentLoaded → getSettings (async) 之间空窗内
    // EN 用户看 zh 默认占位 / zh 用户看 EN 的乱码体验。Settings 拿到后再 apply
    // 第二次(idempotent,只在 language 不同时真改 DOM)
    CCI18n.apply(CCI18n.cachedLanguage());

    deleteModal = new bootstrap.Modal($("#deleteModal"));
    restartReminderModal = new bootstrap.Modal($("#restartReminderModal"), {
      backdrop: "static",
      keyboard: false,
    });
    toast = new bootstrap.Toast($("#appToast"), { delay: 2200 });
    bindEvents();
    bindFeedbackEvents();
    bindThemeEvents();

    // **race fix** (devin #269 review thread):Tauri 后端 startup hook 会
    // emit `residual-scan-report`(残留扫描;#MOC-54 起改为等 auto_apply 落盘后
    // 才发,时机不固定、最长 ~30s)跟 `codex-deeplink`,如果监听 listener 注册
    // 晚于 `await CCApi.getSettings()` / `await renderRoute()` 等异步链落地时点,
    // event 可能已 fire 完毕 → 启动 toast 静默丢失。把 listener 注册提到
    // bindEvents() 紧后(showToast / tFmt / codexMcpHandleDeeplink 等依赖此时
    // 都已初始化),先于所有 await,确保不漏 event(时机越不固定越要早注册)。
    try {
      const tauriEvent = window.__TAURI__?.event;
      if (tauriEvent && typeof tauriEvent.listen === "function") {
        await tauriEvent.listen("codex-deeplink", (e) => {
          const url = typeof e.payload === "string" ? e.payload : "";
          if (url) codexMcpHandleDeeplink(url);
        });
        // #268 启动自检 emit `residual-scan-report` 含污染文件清单 → 弹 toast
        // 提示用户去设置页查看。干净时 backend 不 emit,这里也不会触发。
        await tauriEvent.listen("residual-scan-report", (e) => {
          const report = e?.payload || {};
          const count = (report.polluted || []).length;
          if (count > 0) {
            showToast(tFmt("settings.residualScanStartupToast", { count }));
          }
        });
      }
    } catch (err) {
      console.error("event listen:", err);
    }

    // MOC-62:load 时轮询"是否有可恢复的 MCP 凭据备份"(整文件丢失 + 镜像有备份)→ 弹确认。
    // 用轮询而非一次性 startup event —— 后者可能在此 listener 注册前就 emit 而丢失
    // (chatgpt-codex-connector P2)。fire-and-forget,不阻塞 init。
    mcpCredentialsCheckRestoreOnLoad();

    // **#249 fix**:getSettings 失败时用默认值,确保 renderRoute 始终执行。
    // 之前无 try-catch,config.json 锁/损坏/权限问题 → getSettings 500 →
    // 整条初始化链断裂 → 白屏。
    let settings;
    try {
      settings = await CCApi.getSettings();
    } catch (err) {
      console.error("[init] getSettings failed, using defaults:", err);
      settings = {};
    }
    // MOC-91:在首屏 renderRoute 之前就同步灰色 preset 可见性,确保用户直接进
    // 「添加 provider」时 visiblePresets() 已用上正确的设置值(默认隐藏)。
    showGrayPresets = settings.showGrayProviders === true;
    const finalLang = settings.language || "zh";
    if (finalLang !== CCI18n.language) {
      // backend settings 跟 cache/navigator 不一致才再 apply,正常路径无 op
      CCI18n.apply(finalLang);
    }
    applyTheme(settings.theme || "default");
    if (!window.location.hash) window.location.hash = "dashboard";
    // **#249 fix**:renderRoute 外层兜底 try-catch,防初始路由渲染异常逃逸 → 白屏。
    // (codex-deeplink / residual-scan-report listener 已在上方 race-fix 块提前注册,
    // 此处不再重复注册,避免 codex-deeplink 监听器重复绑定。)
    try {
      await renderRoute(routeFromHash());
    } catch (err) {
      console.error("[init] renderRoute failed:", err);
    }
  });
})();
