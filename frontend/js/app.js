(function () {
  const routes = ["dashboard", "providers/add", "providers", "desktop", "proxy", "settings", "guide"];
  const restartReminderStorageKey = "cas.restartReminder.dismissed";
  const providerFormModelSlots = [
    { key: "default", label: "Default", icon: "bi-circle-fill", iconClass: "default", source: "未配置映射时默认使用这一项", required: true },
    { key: "gpt_5_5", label: "gpt-5.5", icon: "bi-circle", iconClass: "default", source: "gpt-5.5" },
    { key: "gpt_5_4", label: "gpt-5.4", icon: "bi-circle", iconClass: "default", source: "gpt-5.4" },
    { key: "gpt_5_4_mini", label: "gpt-5.4-mini", icon: "bi-circle", iconClass: "default", source: "gpt-5.4-mini" },
    { key: "gpt_5_3_codex", label: "gpt-5.3-codex", icon: "bi-circle", iconClass: "default", source: "gpt-5.3-codex" },
    { key: "gpt_5_2", label: "gpt-5.2", icon: "bi-circle", iconClass: "default", source: "gpt-5.2" },
  ];
  const availableThemes = ["default", "green", "orange", "gray", "dark", "white"];
  const providerAuthSchemes = ["bearer", "x-api-key", "none"];
  const providerFormDefaultRows = ["default", "gpt_5_5", "gpt_5_4", "gpt_5_4_mini", "gpt_5_3_codex", "gpt_5_2"];
  let pendingDeleteId = null;
  let selectedPreset = null;
  let presetCache = [];
  let formApiFormat = "Responses";
  let formModelCapabilities = {};
  let formRequestOptions = {};
  let providerFormMappings = {};
  let providerFormRows = [...providerFormDefaultRows];
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

  function restartReminderDismissed() {
    try {
      return localStorage.getItem(restartReminderStorageKey) === "1";
    } catch (error) {
      return false;
    }
  }

  function showRestartReminder() {
    if (restartReminderDismissed()) return;
    const checkbox = $("#restartReminderDontShow");
    if (checkbox) checkbox.checked = false;
    restartReminderModal?.show();
  }

  function dismissRestartReminder() {
    const checkbox = $("#restartReminderDontShow");
    if (checkbox?.checked) {
      try {
        localStorage.setItem(restartReminderStorageKey, "1");
      } catch (error) {
        console.warn(error);
      }
    }
    restartReminderModal?.hide();
  }

  function t(key) {
    return CCI18n.t(key);
  }

  function formatI18n(key, values = {}) {
    return t(key).replace(/\{(\w+)\}/g, (_, name) => (
      Object.prototype.hasOwnProperty.call(values, name) ? values[name] : `{${name}}`
    ));
  }

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
    const presetName = normalizePresetKey(preset.name);
    const presetUrl = normalizePresetKey(preset.baseUrl);
    return providers.some((provider) => (
      normalizePresetKey(provider.name) === presetName
      || normalizePresetKey(provider.baseUrl) === presetUrl
    ));
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

  function setFormApiFormat(format) {
    formApiFormat = ["OpenAI", "openai", "openai_chat"].includes(format) ? "OpenAI" : "Responses";
    const activeFormat = formApiFormat === "OpenAI" ? "openai_chat" : "responses";
    $all("[data-api-format]").forEach((button) => {
      const active = button.dataset.apiFormat === activeFormat;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", active ? "true" : "false");
    });
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

  function normalizeMappings(mappings = {}) {
    const normalized = emptyMappings();
    if (!mappings || typeof mappings !== "object") return normalized;
    normalized.default = String(mappings.default || "").trim();
    normalized.gpt_5_5 = String(mappings.gpt_5_5 || "").trim();
    normalized.gpt_5_4 = String(mappings.gpt_5_4 || "").trim();
    normalized.gpt_5_4_mini = String(mappings.gpt_5_4_mini || "").trim();
    normalized.gpt_5_3_codex = String(mappings.gpt_5_3_codex || "").trim();
    normalized.gpt_5_2 = String(mappings.gpt_5_2 || "").trim();
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
    return rows;
  }

  function slotByKey(key) {
    return providerFormModelSlots.find((slot) => slot.key === key) || providerFormModelSlots[0];
  }

  function slotOptionsForRow(currentKey) {
    const used = new Set(providerFormRows.filter((key) => key !== currentKey));
    return providerFormModelSlots.filter((slot) => !used.has(slot.key));
  }

  function providerModelOptionsMarkup(currentValue = "") {
    return providerAvailableModels.map((modelId) => (`
      <button
        class="mapping-slot-option ${modelId === currentValue ? "selected" : ""}"
        type="button"
        role="option"
        data-action="select-provider-model-option"
        data-model-value="${escapeHtml(modelId)}"
        aria-selected="${modelId === currentValue ? "true" : "false"}"
      >
        <span>${escapeHtml(modelId)}</span>
        ${modelId === currentValue ? '<i class="bi bi-check2"></i>' : ""}
      </button>
    `)).join("");
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

  function formMappingMarkup() {
    return providerFormRows.map((rowKey, index) => {
      const slot = slotByKey(rowKey);
      const isRequired = rowKey === "default";
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
              <input
                class="form-control provider-model-input"
                id="providerMappingValue-${index}"
                data-provider-model-input="${escapeHtml(rowKey)}"
                value="${escapeHtml(providerFormMappings[rowKey] || "")}"
                placeholder="${escapeHtml(t("providersAdd.providerModelPlaceholder"))}"
                ${isRequired ? "required" : ""}
              >
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
    const canAddMoreRows = providerFormModelSlots.some((slot) => !providerFormRows.includes(slot.key));
    stack.innerHTML = `
      <div class="provider-mapping-card">
        <div class="provider-mapping-list">
          ${formMappingMarkup()}
        </div>
        <div class="provider-mapping-footer">
          <button class="btn btn-outline-primary btn-sm" type="button" data-action="add-provider-model-row" ${canAddMoreRows ? "" : "disabled"}>
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
    if (!remaining) return;
    providerFormRows = [...providerFormRows, remaining];
    openProviderSlotMenuIndex = null;
    openProviderModelMenuKey = null;
    renderProviderMappings();
  }

  function removeProviderMappingRow(index) {
    const key = providerFormRows[index];
    if (!key || key === "default") return;
    providerFormRows = providerFormRows.filter((_, rowIndex) => rowIndex !== index);
    providerFormMappings[key] = "";
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
    if (!options.length) {
      container.hidden = true;
      container.innerHTML = "";
      return;
    }
    const currentMappings = normalizeMappings(mappings || collectProviderMappings());
    container.hidden = false;
    container.innerHTML = options.map(([id, option]) => `
      <label class="preset-option-item">
        <input class="form-check-input" type="checkbox" data-preset-model-option="${escapeHtml(id)}" ${optionEnabled(option, currentMappings) ? "checked" : ""}>
        <span>
          <strong>${escapeHtml(option.label || id)}</strong>
          <small>${escapeHtml(option.description || "")}</small>
        </span>
      </label>
    `).join("");
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
    return normalizeMappings(providerFormMappings);
  }

  function providerPayloadFromForm(includeModels = true) {
    const apiKey = $("#providerApiKey").value.trim();
    const mappings = includeModels ? collectProviderMappings() : null;
    const payload = {
      name: $("#providerName").value.trim(),
      baseUrl: $("#providerBaseUrl").value.trim(),
      authScheme: $("#providerAuth").value,
      apiFormat: formApiFormat,
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
    return payload;
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
    const providerHref = escapeHtml(safeHttpUrl(provider.baseUrl));
    const mappingText = escapeHtml(mapping || provider.apiFormat);
    return `
      <article class="provider-switch-card ${provider.default ? "active" : ""}" draggable="true" data-provider-id="${providerId}">
        <span class="drag-handle"><i class="bi bi-grip-vertical"></i></span>
        <span class="provider-logo">${iconMarkup(provider)}</span>
        <span class="provider-main">
          <strong>${providerName}</strong>
          <a class="truncate" href="${providerHref}" target="_blank" rel="noreferrer">${providerUrl}</a>
        </span>
        <span class="provider-meta truncate">${mappingText}</span>
        <span class="provider-actions">
          <button class="btn btn-primary compact-enable" type="button" data-action="set-default" data-id="${providerId}" ${provider.default ? "disabled" : ""}>
            <i class="bi bi-play-fill"></i><span>${provider.default ? t("status.default") : t("providers.enable")}</span>
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
        <span class="provider-meta">${escapeHtml(preset.apiFormat)}</span>
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
    const providerList = providers.length
      ? `<div class="provider-configured-list" data-provider-list>${providers.map(providerCardMarkup).join("")}</div>`
      : "";
    if (!providers.length) {
      const presets = await CCApi.getPresets();
      target.innerHTML = `<div class="provider-preset-grid">${presets.map((preset) => providerPresetCardMarkup(preset)).join("")}</div>`;
      return;
    }
    if (options.includePresets) {
      const presets = await CCApi.getPresets();
      target.innerHTML = `${providerList}${dashboardPresetSectionMarkup(providers, presets)}`;
    } else {
      target.innerHTML = providerList;
    }
    enableProviderReorder($("[data-provider-list]", target));
  }

  async function renderDashboard() {
    const status = await CCApi.getStatus();
    const activities = await CCApi.getActivities();
    const health = status.desktopHealth || {};
    const desktopReady = status.desktopConfigured && !health.needsApply;
    await renderProviderCards("#dashboardProviderCards", { includePresets: true });
    const desktopIcon = $("#dashboardDesktopIcon");
    desktopIcon.classList.toggle("muted", !desktopReady);
    desktopIcon.innerHTML = `<i class="bi ${desktopReady ? "bi-check-lg" : "bi-exclamation-lg"}"></i>`;
    const desktopStatus = $("#dashboardDesktopStatus");
    desktopStatus.classList.toggle("muted-text", !desktopReady);
    desktopStatus.textContent = health.needsApply
      ? t("status.needsApply")
      : status.desktopConfigured ? t("status.configured") : t("status.notConfigured");
    renderDesktopHealthWarning("#dashboardDesktopWarning", health);
    $("#dashboardProxyStatus").textContent = status.proxyRunning ? `${t("status.running")} :${status.proxyPort}` : t("status.stopped");
    $("#dashboardProviderName").textContent = status.activeProvider.name;
    $("#activityList").innerHTML = activities.map((item) => (
      `<div class="activity-row"><time>${escapeHtml(item.time)}</time><span>${escapeHtml(item.text)}</span></div>`
    )).join("");
    await refreshUpdateBadge();
  }

  async function renderPresets() {
    presetCache = await CCApi.getPresets();
    $("#presetList").innerHTML = presetCache.map((preset) => {
      const active = selectedPreset?.id === preset.id;
      return `
      <button class="preset-item ${active ? "active" : ""}" type="button" data-preset="${escapeHtml(preset.id)}" aria-pressed="${active ? "true" : "false"}">
        <span class="preset-logo">${iconMarkup(preset)}</span>
        <span><strong>${escapeHtml(preset.name)}</strong><span>${escapeHtml(preset.baseUrl)}</span></span>
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

  function isKimiProviderId(id) {
    const value = String(id || "").toLowerCase();
    return value === "kimi" || value === "kimi-code" || value.startsWith("kimi-");
  }

  function setUnverifiedBanner(show) {
    const banner = $("#providerUnverifiedBanner");
    if (banner) banner.hidden = !show;
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
    $("#providerBaseUrl").value = "";
    $("#providerBaseUrl").disabled = false;
    const trigger = $("#providerBaseUrlTrigger");
    if (trigger) trigger.hidden = true;
    renderBaseUrlOptions(null);
    setApiKeyInputState(false);
    $("#providerAuth").value = "bearer";
    setFormApiFormat("responses");
    setProviderMappings(emptyMappings());
    setUnverifiedBanner(false);
  }

  function applyPresetToForm(preset, notify = true) {
    $("#providerName").value = preset.name;
    $("#providerBaseUrl").value = preset.baseUrl;
    $("#providerBaseUrl").disabled = false;
    const trigger = $("#providerBaseUrlTrigger");
    if (trigger) trigger.hidden = true;
    baseUrlMenuOpen = false;
    renderBaseUrlOptions(preset);
    setAuthSchemeValue(preset.authScheme);
    setApiKeyInputState(false);
    selectedPreset = preset;
    setFormApiFormat(["openai", "openai_chat", "OpenAI"].includes(preset.apiFormat) ? "openai_chat" : "responses");
    formModelCapabilities = normalizeCapabilities(preset.modelCapabilities || {});
    formRequestOptions = normalizeRequestOptions(preset.requestOptions || {});
    providerAvailableModels = [];
    setProviderMappings(preset.models || emptyMappings());
    renderPresetOptions(preset, preset.models || emptyMappings());
    updatePresetSelection();
    setUnverifiedBanner(!isKimiProviderId(preset.id));
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
    $("#providerBaseUrl").value = provider.baseUrl;
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
    // 优先用匹配预设的 apiFormat,saved provider 的 apiFormat 可能因升级残留旧值
    // (例如 v1.0.0 时 Kimi 默认 "responses",v1.0.1 起改成 "openai_chat")。后端
    // _sync_apiformat_from_builtin 也会做一次根治,这里是 UI 侧的二重保险。
    const effectiveApiFormat = (matchedPreset && matchedPreset.apiFormat) || provider.apiFormat;
    setFormApiFormat(["openai", "openai_chat"].includes(effectiveApiFormat) ? "openai_chat" : "responses");
    providerAvailableModels = [];
    setProviderMappings(provider.mappings || emptyMappings());
    renderPresetOptions(selectedPreset, provider.mappings || emptyMappings());
    updatePresetSelection();
    setUnverifiedBanner(!isKimiProviderId(matchedPreset?.id || provider.id));
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
      logEl.innerHTML = logs.map((line) => `
        <div class="log-line"><span>${escapeHtml(line.at)}</span><span class="log-level ${escapeHtml(line.level)}">${escapeHtml(line.level.toUpperCase())}</span><span>${escapeHtml(line.message)}</span></div>
      `).join("");
      if ($("#autoScroll")?.checked) logEl.scrollTop = logEl.scrollHeight;
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
    await refreshProxyLog();
    startProxyLogAutoRefresh();
  }

  async function renderSettings() {
    const settings = await CCApi.getSettings();
    applyTheme(settings.theme || "default");
    $("#settingsProxyPort").value = settings.proxyPort;
    $("#settingsAdminPort").value = settings.adminPort;
    $("#autoApplyOnStart").checked = settings.autoApplyOnStart !== false;
    $("#exposeAllProviderModels").checked = !!settings.exposeAllProviderModels;
    $("#restoreCodexOnExit").checked = settings.restoreCodexOnExit !== false;
    $("#settingsUpdateUrl").value = settings.updateUrl || "";
    renderModelMenuModeState(settings);
    await refreshBackupList();
    await refreshCodexSnapshotStatus();
  }

  async function refreshCodexSnapshotStatus() {
    const target = $("#codexSnapshotStatus");
    if (!target) return;
    try {
      const status = await CCApi.getDesktopSnapshotStatus();
      if (status && status.hasSnapshot) {
        target.textContent = formatI18n("settings.codexSnapshotStatusActive", {
          time: status.snapshotAt || "",
        });
      } else {
        target.textContent = t("settings.codexSnapshotStatusEmpty");
      }
    } catch (error) {
      target.textContent = t("settings.codexSnapshotStatusEmpty");
    }
  }

  async function renderRoute(route) {
    $all(".page").forEach((page) => page.classList.toggle("active", page.dataset.page === route));
    $all(".route-tab").forEach((tab) => {
      const key = route.startsWith("providers") ? "providers" : route;
      tab.classList.toggle("active", tab.dataset.nav === key);
    });
    if (route !== "proxy") stopProxyLogAutoRefresh();
    if (route === "dashboard") await renderDashboard();
    if (route === "providers/add") await renderProviderForm();
    if (route === "providers") await renderProviders();
    if (route === "desktop") await renderDesktop();
    if (route === "proxy") await renderProxy();
    if (route === "settings") await renderSettings();
  }

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
      exposeAllProviderModels: $("#exposeAllProviderModels")?.checked || false,
      restoreCodexOnExit: $("#restoreCodexOnExit")?.checked !== false,
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
    const form = $("#providerForm");
    if (form && !form.reportValidity()) return;
    if (!window.confirm(t("confirm.providerApplyDesktop"))) return;

    actionEl.disabled = true;
    try {
      const provider = await saveProviderFromForm();
      await CCApi.activateProvider(provider.id);
      const desktopResult = await CCApi.configureDesktop();
      if (desktopResult.requiresProxy) {
        await CCApi.startProxy();
      }
      editingProviderId = null;
      selectedPreset = null;
      window.location.hash = "dashboard";
      showToast(t("toast.providerAppliedDesktop"));
      showRestartReminder();
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
        if (desktopSync.attempted && desktopSync.success) {
          showToast(t("toast.defaultUpdatedDesktop"));
          showRestartReminder();
        } else if (desktopSync.attempted && desktopSync.success === false) {
          showToast(t("toast.defaultUpdatedDesktopFailed"));
        } else {
          showToast(t("toast.defaultUpdated"));
          showRestartReminder();
        }
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

      if (action === "test-provider") {
        const resultEl = $(`[data-speed-for="${actionEl.dataset.id}"]`);
        actionEl.disabled = true;
        if (resultEl) {
          resultEl.textContent = t("providers.testing");
          resultEl.classList.remove("bad");
        }
        try {
          const result = await CCApi.testProvider(actionEl.dataset.id);
          if (resultEl) {
            resultEl.textContent = result.message || `${result.latencyMs} ms`;
            resultEl.classList.toggle("bad", result.ok === false);
          }
          showToast(result.message || t("providers.testDone"));
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
          const result = await CCApi.testProviderPayload(payload);
          resultEl.textContent = result.message || `${result.latencyMs} ms`;
          resultEl.classList.toggle("bad", result.ok === false);
          showToast(result.message || t("providers.testDone"));
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
          setProviderMappings(result.suggested || emptyMappings(), { availableModels: providerAvailableModels });
          if (resultEl) resultEl.textContent = t("models.fetchSuccess");
          showToast(t("toast.modelsAutofilled"));
        } catch (error) {
          providerAvailableModels = [];
          renderProviderMappings();
          if (resultEl) resultEl.textContent = t("models.fetchFailedManual");
          showToast(error.message || t("toast.requestFailed"));
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
        if (!window.confirm(t("confirm.desktopClear"))) return;
        const result = await CCApi.clearDesktop();
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
    } catch (error) {
      console.error(error);
      showToast(error.message || t("toast.requestFailed"));
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
        showToast(formatI18n("feedback.tooLargeFile", { name: f.name }));
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
    const bodyEl = $("#feedbackBody");
    const submitBtn = $("#feedbackSubmitBtn");
    if (!bodyEl) return;

    const title = (titleEl?.value || "").trim();
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
        body,
        include_diagnostics: $("#feedbackIncludeDiagnostics").checked,
        attachments,
      };

      const result = await CCApi.submitFeedback(payload);
      if (feedbackBsModal) feedbackBsModal.hide();
      showToast(formatI18n("feedback.successToast", { id: result.id || "" }));
    } catch (err) {
      console.error("[feedback] submit failed:", err);
      let msg = err && err.message ? err.message : String(err);
      if (msg.includes("did not match the expected pattern")) {
        msg = "请求体构造异常,请重试或去掉附件";
      }
      showToast(formatI18n("feedback.failToast", { message: msg }));
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
      if (langButton) CCI18n.apply(langButton.dataset.lang);
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
      const formatButton = event.target.closest("[data-api-format]");
      if (formatButton) {
        event.preventDefault();
        setFormApiFormat(formatButton.dataset.apiFormat);
        showToast(formatButton.dataset.apiFormat === "openai_chat" ? t("toast.openaiFormatExperimental") : t("toast.responsesFormatSelected"));
        return;
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
    });

    document.addEventListener("input", (event) => {
      if (event.target.id === "providerBaseUrl") {
        renderBaseUrlOptions();
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
    $("#exposeAllProviderModels").addEventListener("change", saveSettingsFromForm);
    $("#restoreCodexOnExit")?.addEventListener("change", saveSettingsFromForm);
    $("#configImportFile")?.addEventListener("change", (event) => {
      importConfigFile(event.target.files?.[0]);
    });
    $("#restartReminderAck")?.addEventListener("click", dismissRestartReminder);

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
    deleteModal = new bootstrap.Modal($("#deleteModal"));
    restartReminderModal = new bootstrap.Modal($("#restartReminderModal"), {
      backdrop: "static",
      keyboard: false,
    });
    toast = new bootstrap.Toast($("#appToast"), { delay: 2200 });
    bindEvents();
    bindFeedbackEvents();
    const settings = await CCApi.getSettings();
    CCI18n.apply(settings.language || "zh");
    applyTheme(settings.theme || "default");
    if (!window.location.hash) window.location.hash = "dashboard";
    await renderRoute(routeFromHash());
  });
})();
