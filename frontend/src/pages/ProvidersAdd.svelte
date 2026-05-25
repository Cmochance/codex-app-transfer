<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { CCApi } from '../lib/api';
  import type { Provider, Preset } from '../lib/api';
  import { activeTab, refreshAll } from '../lib/store';
  import { t } from '../lib/i18n';

  export let id: string | undefined = undefined;

  let isEditMode = false;
  $: isEditMode = !!id;

  let presets: Preset[] = [];
  let selectedPreset: Preset | null = null;

  // Form Fields
  let name = 'My DeepSeek';
  let baseUrl = 'https://api.deepseek.com/v1';
  let apiKey = '';
  let authScheme = 'bearer';
  let apiFormat = 'openai_chat';
  let webSearchEnabled = false;
  
  // Grok Web inputs
  let grokSso = '';
  let grokSsoRw = '';
  let grokCfClearance = '';
  let grokCookieString = '';
  let grokStatsigId = '';
  let grokUserAgent = '';

  // Model mappings
  let mappings: Record<string, string> = {
    default: '',
    gpt_5_5: '',
    gpt_5_4: '',
    gpt_5_4_mini: '',
    gpt_5_3_codex: '',
    gpt_5_2: '',
  };

  // UI state
  let showApiKey = false;
  let hasSavedKey = false;
  let speedResult = '';
  let speedTesting = false;
  let modelFetchResult = '';
  let modelFetching = false;
  let fetchedModels: string[] = [];
  let showModelDropdown: Record<string, boolean> = {};

  // OAuth states
  let oauthStatus: any = null;
  let oauthLoading = false;
  let oauthInterval: any = null;

  // Active OAuth provider details
  $: isOauth = ['gemini_cli_oauth', 'antigravity_oauth'].includes(apiFormat);
  $: oauthPrefix = apiFormat === 'gemini_cli_oauth' ? 'geminiOauth' : 'antigravityOauth';

  async function loadInitialData() {
    try {
      presets = await CCApi.getPresets();
      
      // If edit mode, load existing provider details
      if (isEditMode && id) {
        const providers = await CCApi.getProviders();
        const editing = providers.find(p => p.id === id);
        if (editing) {
          name = editing.name;
          baseUrl = editing.baseUrl;
          apiKey = ''; // Always clear API key input for editing
          hasSavedKey = !!editing.hasApiKey;
          authScheme = editing.authScheme;
          apiFormat = editing.apiFormat;
          mappings = {
            default: editing.mappings?.default || '',
            gpt_5_5: editing.mappings?.gpt_5_5 || '',
            gpt_5_4: editing.mappings?.gpt_5_4 || '',
            gpt_5_4_mini: editing.mappings?.gpt_5_4_mini || '',
            gpt_5_3_codex: editing.mappings?.gpt_5_3_codex || '',
            gpt_5_2: editing.mappings?.gpt_5_2 || '',
          };
          
          if (editing.requestOptions) {
            webSearchEnabled = !!editing.requestOptions.web_search_enabled;
          }

          // If custom preset, try to find preset by baseUrl match
          const presetMatch = presets.find(p => p.baseUrl === editing.baseUrl);
          if (presetMatch) {
            selectedPreset = presetMatch;
          }
        }
      } else {
        // Look for preset in query params
        const hash = window.location.hash;
        const presetParam = new URLSearchParams(hash.split('?')[1] || '').get('preset');
        if (presetParam) {
          const preset = presets.find(p => p.id === presetParam);
          if (preset) {
            applyPreset(preset);
          }
        } else {
          // Default to first preset (usually DeepSeek)
          if (presets.length > 0) {
            applyPreset(presets[0]);
          }
        }
      }
    } catch (err) {
      console.error(err);
    }
  }

  function applyPreset(preset: Preset) {
    selectedPreset = preset;
    name = preset.name === '自定义第三方' ? '自定义第三方' : preset.name;
    baseUrl = preset.baseUrl;
    authScheme = preset.authScheme;
    apiFormat = preset.apiFormat;
    hasSavedKey = false;
    apiKey = '';
    
    // Reset grok web details
    grokSso = '';
    grokSsoRw = '';
    grokCfClearance = '';
    grokCookieString = '';
    grokStatsigId = '';
    grokUserAgent = '';

    mappings = {
      default: preset.models?.default || '',
      gpt_5_5: preset.models?.gpt_5_5 || '',
      gpt_5_4: preset.models?.gpt_5_4 || '',
      gpt_5_4_mini: preset.models?.gpt_5_4_mini || '',
      gpt_5_3_codex: preset.models?.gpt_5_3_codex || '',
      gpt_5_2: preset.models?.gpt_5_2 || '',
    };
    webSearchEnabled = false;
    fetchedModels = [];
  }

  // Speed test for the current inputs
  async function handleTestSpeed() {
    speedTesting = true;
    speedResult = '';
    try {
      const payload = getPayload();
      const res = await CCApi.testProviderPayload(payload);
      speedResult = res.duration_ms ? `Ping: ${res.duration_ms}ms` : 'Ping Success';
    } catch (err: any) {
      speedResult = `Ping Failed: ${err.message || 'Error'}`;
    } finally {
      speedTesting = false;
    }
  }

  // Fetch available models from upstream
  async function handleFetchModels() {
    modelFetching = true;
    modelFetchResult = '';
    fetchedModels = [];
    try {
      const payload = getPayload();
      const res = isEditMode && id
        ? await CCApi.fetchProviderModels(id)
        : await CCApi.fetchProviderModelsPayload(payload);
      
      if (res && Array.isArray(res.models)) {
        fetchedModels = res.models;
        modelFetchResult = $t('models.fetched') + ` ${fetchedModels.length}`;
      } else if (res && Array.isArray(res.data)) {
        fetchedModels = res.data.map((m: any) => m.id);
        modelFetchResult = $t('models.fetched') + ` ${fetchedModels.length}`;
      } else {
        modelFetchResult = $t('models.fetchFailedManual');
      }
    } catch (err: any) {
      modelFetchResult = err.message || $t('models.fetchFailed');
    } finally {
      modelFetching = false;
    }
  }

  // Get full form payload
  function getPayload(): Provider {
    const payload: Provider = {
      name: name.trim(),
      baseUrl: baseUrl.trim(),
      authScheme,
      apiFormat,
      models: mappings,
      requestOptions: {
        web_search_enabled: webSearchEnabled,
      },
    };

    if (apiKey.trim()) {
      payload.apiKey = apiKey.trim();
    }

    if (apiFormat === 'grok_web') {
      const cookies: Record<string, string> = { sso: grokSso.trim() };
      if (grokSsoRw.trim()) cookies['sso-rw'] = grokSsoRw.trim();
      if (grokCfClearance.trim()) cookies.cf_clearance = grokCfClearance.trim();
      if (grokCookieString.trim()) cookies.cookieString = grokCookieString.trim();
      
      payload.grokWeb = {
        cookies,
        statsigId: grokStatsigId.trim(),
        userAgent: grokUserAgent.trim(),
      };
      payload.authScheme = 'grok_cookie';
    }

    return payload;
  }

  async function handleSave(enableImmediately = false) {
    if (!name.trim()) return alert('Name required');
    if (!isOauth && apiFormat !== 'grok_web' && !baseUrl.trim()) return alert('Base URL required');
    if (!isOauth && apiFormat !== 'grok_web' && !hasSavedKey && !apiKey.trim()) return alert('API Key required');

    try {
      const payload = getPayload();
      let savedProvider: Provider;
      
      if (isEditMode && id) {
        savedProvider = await CCApi.updateProvider(id, payload);
      } else {
        savedProvider = await CCApi.addProvider(payload);
      }

      if (enableImmediately && savedProvider.id) {
        await CCApi.setDefaultProvider(savedProvider.id);
        showToast($t('toast.providerAppliedDesktop'));
      } else {
        showToast($t('toast.providerSaved'));
      }

      await refreshAll();
      activeTab.set('providers');
      window.location.hash = 'providers';
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  // Google OAuth flow actions
  async function refreshOauthStatus() {
    if (!isOauth) return;
    try {
      const status = apiFormat === 'gemini_cli_oauth'
        ? await CCApi.getGeminiOauthStatus()
        : await CCApi.getAntigravityOauthStatus();
      oauthStatus = status;
    } catch (err) {
      console.error(err);
    }
  }

  async function handleOauthLogin() {
    oauthLoading = true;
    try {
      if (apiFormat === 'gemini_cli_oauth') {
        await CCApi.loginGeminiOauth();
      } else {
        await CCApi.loginAntigravityOauth();
      }
      await refreshOauthStatus();
      showToast($t(oauthPrefix + '.loginSuccess'));
    } catch (err: any) {
      showToast(err.message || $t(oauthPrefix + '.loginFailed'));
    } finally {
      oauthLoading = false;
    }
  }

  async function handleOauthLogout() {
    if (!confirm('Logout?')) return;
    try {
      if (apiFormat === 'gemini_cli_oauth') {
        await CCApi.logoutGeminiOauth();
      } else {
        await CCApi.logoutAntigravityOauth();
      }
      await refreshOauthStatus();
      showToast($t(oauthPrefix + '.logoutConfirmed'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  // Dropdown list select helper
  function selectModel(key: string, model: string) {
    mappings[key] = model;
    showModelDropdown[key] = false;
  }

  // Toast Helpers
  let toastMsg = '';
  let showToastBanner = false;
  function showToast(msg: string) {
    toastMsg = msg;
    showToastBanner = true;
    setTimeout(() => {
      showToastBanner = false;
    }, 3000);
  }

  onMount(() => {
    loadInitialData();

    // Start OAuth status poll if OAuth mode
    oauthInterval = setInterval(() => {
      if (isOauth) refreshOauthStatus();
    }, 5000);
  });

  onDestroy(() => {
    if (oauthInterval) clearInterval(oauthInterval);
  });

  // Watchers for OAuth status updates when switching formats
  $: {
    if (isOauth) {
      refreshOauthStatus();
    }
  }
</script>

{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="providers-add-page">
  <div class="page-title">
    <h1>{isEditMode ? $t('providersAdd.editTitle') : $t('providersAdd.title')}</h1>
    <p class="subtitle-text">{isEditMode ? $t('providersAdd.editTitle') : $t('providersAdd.subtitle')}</p>
  </div>

  <div class="form-preset-layout">
    <!-- Form Panel -->
    <article class="mac-card form-panel">
      <form on:submit|preventDefault={() => handleSave(false)}>
        <!-- Provider Name -->
        <div class="form-group">
          <label class="form-label required" for="provName">{$t('providers.name')}</label>
          <input class="mac-input" id="provName" bind:value={name} required />
        </div>

        <!-- API Base URL -->
        {#if !isOauth}
          <div class="form-group">
            <div class="label-line">
              <label class="form-label required" for="provUrl">API Base URL</label>
              <button class="link-btn" type="button" on:click={handleTestSpeed} disabled={speedTesting}>
                <i class="bi bi-lightning-charge"></i>
                <span>{$t('providers.manageAndTest')}</span>
              </button>
            </div>
            <input class="mac-input" id="provUrl" bind:value={baseUrl} required={apiFormat !== 'grok_web'} placeholder="https://api..." />
            {#if speedTesting}
              <p class="field-hint loading">Testing latency...</p>
            {:else if speedResult}
              <p class="field-hint" class:danger={speedResult.includes('Failed')}>{speedResult}</p>
            {/if}
          </div>
        {/if}

        <!-- API Key input (standard providers) -->
        {#if !isOauth && apiFormat !== 'grok_web'}
          <div class="form-group">
            <label class="form-label" class:required={!hasSavedKey} for="provKey">API Key</label>
            <div class="input-with-icon">
              <input 
                class="mac-input" 
                id="provKey" 
                type={showApiKey ? 'text' : 'password'} 
                bind:value={apiKey} 
                required={!hasSavedKey && !apiKey}
                placeholder={hasSavedKey ? $t('providers.keySavedPlaceholder') : $t('providers.keyPlaceholder')}
              />
              <button class="icon-toggle" type="button" on:click={() => showApiKey = !showApiKey}>
                <i class="bi {showApiKey ? 'bi-eye-slash' : 'bi-eye'}"></i>
              </button>
            </div>
          </div>
        {/if}

        <!-- Google OAuth Panel (impersonate flows) -->
        {#if isOauth}
          <div class="form-group oauth-box mac-card">
            <label class="form-label">{$t(oauthPrefix + '.title')}</label>
            {#if oauthLoading}
              <p class="field-hint loading">Waiting for Google browser authentication...</p>
            {:else if oauthStatus}
              {#if oauthStatus.loggedIn}
                <p class="oauth-status success">
                  <i class="bi bi-check-circle-fill"></i>
                  Logged in: {oauthStatus.email || '?'} (Project: {oauthStatus.projectId || '?'})
                </p>
                <div class="oauth-actions">
                  <button class="mac-btn" type="button" on:click={handleOauthLogin}>
                    {$t(oauthPrefix + '.switchAccountBtn')}
                  </button>
                  <button class="mac-btn danger" type="button" on:click={handleOauthLogout}>
                    {$t(oauthPrefix + '.logoutBtn')}
                  </button>
                </div>
              {:else}
                <p class="oauth-status warning">
                  <i class="bi bi-exclamation-triangle-fill"></i>
                  {$t(oauthPrefix + '.statusNotLoggedIn')}
                </p>
                <button class="mac-btn primary" type="button" on:click={handleOauthLogin}>
                  {$t(oauthPrefix + '.loginBtn')}
                </button>
              {/if}
            {:else}
              <p class="field-hint loading">Loading authentication status...</p>
            {/if}
            <p class="field-hint">{oauthStatus?.expiresAt ? `Token expires: ${new Date(oauthStatus.expiresAt).toLocaleString()}` : ''}</p>
            <p class="field-hint tos-warning">{$t(oauthPrefix + '.tosWarning')}</p>
          </div>
        {/if}

        <!-- Grok Web Cookies Panel -->
        {#if apiFormat === 'grok_web'}
          <div class="form-group grok-box mac-card">
            <label class="form-label">{$t('grokWeb.title')}</label>
            <p class="field-hint">{$t('grokWeb.hint')}</p>
            
            <div class="grok-field-row">
              <label class="form-label required" for="grokSso">sso (SSO token)</label>
              <input class="mac-input" id="grokSso" type="password" bind:value={grokSso} placeholder="sso token from grok.com cookie" />
            </div>

            <div class="grok-field-row">
              <label class="form-label" for="grokCookieString">Cookie String</label>
              <input class="mac-input" id="grokCookieString" type="password" bind:value={grokCookieString} placeholder="Full Cookie Headers value if Cloudflare challenged" />
            </div>

            <details class="grok-advanced">
              <summary>Advanced Custom Options</summary>
              <div class="grok-field-row">
                <label class="form-label" for="grokSsoRw">sso-rw</label>
                <input class="mac-input" id="grokSsoRw" type="password" bind:value={grokSsoRw} />
              </div>
              <div class="grok-field-row">
                <label class="form-label" for="grokCf">cf_clearance</label>
                <input class="mac-input" id="grokCf" type="password" bind:value={grokCfClearance} />
              </div>
              <div class="grok-field-row">
                <label class="form-label" for="grokStatsig">x-statsig-id</label>
                <input class="mac-input" id="grokStatsig" type="password" bind:value={grokStatsigId} />
              </div>
              <div class="grok-field-row">
                <label class="form-label" for="grokUA">User-Agent override</label>
                <input class="mac-input" id="grokUA" type="text" bind:value={grokUserAgent} />
              </div>
            </details>
            <p class="field-hint tos-warning">{$t('grokWeb.tosWarning')}</p>
          </div>
        {/if}

        <!-- Web Search Switch -->
        {#if selectedPreset?.supportsWebSearch}
          <div class="form-group web-search-toggle">
            <label class="mac-switch">
              <input type="checkbox" bind:checked={webSearchEnabled} />
              <span class="mac-switch-slider"></span>
              <span class="form-label inline">{$t('providersAdd.webSearchEnabled')}</span>
            </label>
            <p class="field-hint">
              {#if selectedPreset.id === 'kimi'}
                {$t('providersAdd.webSearchEnabledHint.kimi')}
              {:else if selectedPreset.id === 'mimo'}
                {$t('providersAdd.webSearchEnabledHint.xiaomi-mimo-payg')}
              {:else}
                {$t('providersAdd.webSearchEnabledHint.default')}
              {/if}
            </p>
          </div>
        {/if}

        <!-- Protocol selector for custom preset -->
        <div class="form-group">
          <label class="form-label" for="provFormat">{$t('providersAdd.apiFormatLabel')}</label>
          {#if selectedPreset?.id !== 'custom-third-party'}
            <input class="mac-input readonly" id="provFormat" value={apiFormat} readonly />
          {:else}
            <select class="mac-input" id="provFormat" bind:value={apiFormat}>
              <option value="openai_chat">OpenAI Chat</option>
              <option value="responses">Responses (Direct Passthrough)</option>
              <option value="anthropic_messages">Anthropic Messages</option>
              <option value="gemini_native">Gemini Native</option>
              <option value="gemini_cli_oauth">Gemini CLI (OAuth)</option>
              <option value="antigravity_oauth">Antigravity (OAuth)</option>
              <option value="grok_web">Grok Web</option>
            </select>
          {/if}
        </div>

        <!-- Model Mapping Grid -->
        <section class="mapping-section mac-card">
          <div class="section-title-row">
            <div>
              <h3>{$t('providersAdd.mappingTitle')}</h3>
              <p class="field-hint">{$t('providersAdd.mappingSubtitle')}</p>
            </div>
            <button class="mac-btn compact-btn" type="button" on:click={handleFetchModels} disabled={modelFetching}>
              <i class="bi bi-cloud-arrow-down"></i>
              <span>{$t('models.fetch')}</span>
            </button>
          </div>

          {#if modelFetching}
            <p class="field-hint loading">Fetching upstream models...</p>
          {:else if modelFetchResult}
            <p class="field-hint success">{modelFetchResult}</p>
          {/if}

          <div class="mapping-grid">
            {#each Object.keys(mappings) as mapKey}
              <div class="mapping-row">
                <label class="mapping-label" for="map-{mapKey}">{mapKey}</label>
                
                <div class="mapping-input-wrapper">
                  <input 
                    class="mac-input" 
                    id="map-{mapKey}" 
                    bind:value={mappings[mapKey]} 
                    placeholder={mapKey === 'default' ? 'Required default model name' : 'Fallback to default'}
                  />
                  
                  <!-- Dropdown selector from fetched models -->
                  {#if fetchedModels.length > 0}
                    <button class="dropdown-trigger" type="button" on:click={() => showModelDropdown[mapKey] = !showModelDropdown[mapKey]}>
                      <i class="bi bi-chevron-down"></i>
                    </button>
                    
                    {#if showModelDropdown[mapKey]}
                      <!-- svelte-ignore a11y_no_noninteractive_element_to_interactive_role -->
                      <ul class="dropdown-menu" role="listbox">
                        {#each fetchedModels as model}
                          <!-- svelte-ignore a11y_click_events_have_key_events -->
                          <li class="dropdown-item" role="option" aria-selected={mappings[mapKey] === model} on:click={() => selectModel(mapKey, model)}>
                            {model}
                          </li>
                        {/each}
                      </ul>
                    {/if}
                  {/if}
                </div>
              </div>
            {/each}
          </div>
        </section>

        <!-- Actions -->
        <div class="form-actions">
          <button class="mac-btn primary btn-lg" type="button" on:click={() => handleSave(true)}>
            <i class="bi bi-play-fill"></i>
            <span>{$t('providers.enable')}</span>
          </button>
          <button class="mac-btn btn-lg" type="submit">
            {$t('common.saveOnly')}
          </button>
          <button class="mac-btn btn-lg" type="button" on:click={() => { activeTab.set('providers'); window.location.hash = 'providers'; }}>
            {$t('common.cancel')}
          </button>
        </div>
      </form>
    </article>

    <!-- Presets Selector Panel -->
    <article class="mac-card presets-panel">
      <h2>{$t('providersAdd.presets')}</h2>
      <p class="panel-hint">{$t('providersAdd.presetsHint')}</p>
      
      <div class="preset-list">
        {#each presets as preset}
          <button 
            class="preset-item" 
            class:active={selectedPreset?.id === preset.id}
            type="button" 
            on:click={() => applyPreset(preset)}
          >
            <span class="preset-logo">
              {#if preset.logo}
                <img src={preset.logo} alt={preset.name} class="logo-image" />
              {:else}
                <i class="bi {preset.icon || 'bi-plug-fill'} logo-icon"></i>
              {/if}
            </span>
            <div class="preset-meta">
              <strong>{preset.name}</strong>
              <span class="truncate">{preset.baseUrl || 'Dynamic Input'}</span>
            </div>
            <i class="bi {selectedPreset?.id === preset.id ? 'bi-check2' : 'bi-chevron-right'}"></i>
          </button>
        {/each}
      </div>
    </article>
  </div>
</div>

<style>
  .providers-add-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .form-preset-layout {
    display: flex;
    gap: 20px;
    align-items: flex-start;
  }

  .form-panel {
    flex: 2;
    padding: 20px;
  }

  .presets-panel {
    flex: 1;
    padding: 16px;
    max-height: 80vh;
    overflow-y: auto;
  }

  .presets-panel h2 {
    font-size: 13px;
    font-weight: 600;
    margin-bottom: 4px;
  }

  .panel-hint {
    font-size: 11px;
    color: var(--mac-text-secondary);
    margin-bottom: 12px;
  }

  .preset-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .preset-item {
    display: flex;
    align-items: center;
    padding: 8px 12px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-button);
    cursor: pointer;
    text-align: left;
    width: 100%;
    transition: all var(--transition-fast);
    color: var(--mac-text-primary);
  }

  .preset-item:hover {
    background-color: rgba(0, 0, 0, 0.03);
  }
  @media (prefers-color-scheme: dark) {
    .preset-item:hover {
      background-color: rgba(255, 255, 255, 0.03);
    }
  }

  .preset-item.active {
    background-color: var(--mac-accent-soft);
    border-color: var(--mac-accent);
  }

  .preset-logo {
    width: 24px;
    height: 24px;
    border-radius: 4px;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    margin-right: 10px;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
  }

  .logo-image {
    width: 16px;
    height: 16px;
    object-fit: contain;
  }

  .logo-icon {
    font-size: 14px;
    color: var(--mac-accent);
  }

  .preset-meta {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .preset-meta strong {
    font-size: 12px;
    font-weight: 600;
  }

  /* Form layouts */
  .label-line {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .link-btn {
    background: transparent;
    border: none;
    cursor: pointer;
    font-size: 11px;
    color: var(--mac-accent);
    display: flex;
    align-items: center;
    gap: 4px;
    outline: none;
  }

  .link-btn:hover {
    text-decoration: underline;
  }

  .input-with-icon {
    position: relative;
    display: flex;
    align-items: center;
  }

  .icon-toggle {
    position: absolute;
    right: 8px;
    background: transparent;
    border: none;
    color: var(--mac-text-secondary);
    cursor: pointer;
    padding: 4px;
    font-size: 14px;
    display: flex;
    align-items: center;
  }

  .readonly {
    background-color: rgba(120, 120, 128, 0.05);
    cursor: not-allowed;
    color: var(--mac-text-secondary);
  }

  /* OAuth and Grok Boxes */
  .oauth-box, .grok-box {
    margin-top: 20px;
    padding: 14px;
    border: var(--mac-border-highlight);
    background-color: rgba(120, 120, 128, 0.03);
  }

  .oauth-status {
    font-size: 12px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 12px;
  }

  .oauth-status.success { color: var(--mac-success); }
  .oauth-status.warning { color: var(--mac-warning); }

  .oauth-actions {
    display: flex;
    gap: 8px;
  }

  .tos-warning {
    color: var(--mac-warning) !important;
    font-weight: 500;
    margin-top: 8px !important;
  }

  .grok-field-row {
    margin-bottom: 10px;
  }

  .grok-advanced {
    margin-top: 14px;
    border-top: 1px solid var(--mac-border-separator);
    padding-top: 8px;
  }

  .grok-advanced summary {
    font-size: 11px;
    font-weight: 600;
    color: var(--mac-text-secondary);
    cursor: pointer;
    outline: none;
    margin-bottom: 8px;
  }

  /* Switch */
  .web-search-toggle {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 16px;
  }
  
  .form-label.inline {
    display: inline-block;
    margin-bottom: 0;
    font-size: 13px;
  }

  /* Mappings Section */
  .mapping-section {
    margin-top: 20px;
    background-color: rgba(120, 120, 128, 0.02);
  }

  .section-title-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    border-bottom: 1px solid var(--mac-border-separator);
    padding-bottom: 10px;
    margin-bottom: 12px;
  }

  .mapping-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .mapping-row {
    display: flex;
    align-items: center;
  }

  .mapping-label {
    width: 120px;
    font-size: 12px;
    font-weight: 600;
    font-family: var(--font-mono);
  }

  .mapping-input-wrapper {
    flex: 1;
    position: relative;
    display: flex;
    align-items: center;
  }

  .dropdown-trigger {
    position: absolute;
    right: 8px;
    background: transparent;
    border: none;
    cursor: pointer;
    color: var(--mac-text-secondary);
    font-size: 12px;
  }

  .dropdown-menu {
    position: absolute;
    top: 100%;
    left: 0;
    width: 100%;
    max-height: 160px;
    overflow-y: auto;
    background-color: var(--mac-bg-window);
    backdrop-filter: blur(25px) saturate(180%);
    border: var(--mac-border-window);
    border-radius: var(--radius-input);
    box-shadow: var(--mac-shadow-popover);
    z-index: 500;
    list-style: none;
    margin-top: 4px;
    padding: 4px;
  }

  .dropdown-item {
    padding: 6px 12px;
    font-size: 12px;
    border-radius: 4px;
    cursor: pointer;
    transition: background-color var(--transition-fast);
  }

  .dropdown-item:hover {
    background-color: var(--mac-accent);
    color: #ffffff;
  }

  /* Form actions */
  .form-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
    margin-top: 24px;
    border-top: 1px solid var(--mac-border-separator);
    padding-top: 16px;
  }

  .btn-lg {
    font-size: 13px;
    padding: 8px 16px;
    border-radius: var(--radius-button);
  }

  .loading {
    color: var(--mac-text-secondary);
    animation: pulse 1.5s infinite;
  }

  .success {
    color: var(--mac-success);
  }

  .danger {
    color: var(--mac-text-danger);
  }

  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.5; }
  }

  /* Toast Notification */
  .toast-banner {
    position: fixed;
    bottom: 24px;
    right: 24px;
    background-color: var(--mac-bg-window);
    backdrop-filter: blur(30px) saturate(190%);
    border: var(--mac-border-window);
    border-radius: 8px;
    padding: 10px 18px;
    box-shadow: var(--mac-shadow-popover);
    z-index: 2000;
    font-size: 13px;
    font-weight: 500;
    animation: slideUp 0.2s ease-out;
  }

  @keyframes slideUp {
    from { transform: translateY(10px); opacity: 0; }
    to { transform: translateY(0); opacity: 1; }
  }
</style>
