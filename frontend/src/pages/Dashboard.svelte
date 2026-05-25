<script lang="ts">
  import { onMount } from 'svelte';
  import { CCApi, PluginUnlockApi } from '../lib/api';
  import type { Provider, Preset, Activity, ProxyStatus, AppStatus } from '../lib/api';
  import { appStatus, providers, activeTab, refreshAll, proxyStatus } from '../lib/store';
  import { t } from '../lib/i18n';

  let presets: Preset[] = [];
  let activities: Activity[] = [];
  let pluginUnlockState = { status: 'disconnected', message: '' };

  let speedResults: Record<string, string> = {};
  let usageResults: Record<string, string> = {};

  let speedTesting: Record<string, boolean> = {};
  let usageQuerying: Record<string, boolean> = {};

  // Drag and drop variables
  let draggedIdx: number | null = null;

  async function loadInitialData() {
    try {
      presets = await CCApi.getPresets();
      activities = await CCApi.getActivities();
      await updatePluginUnlockStatus();
    } catch (err) {
      console.error(err);
    }
  }

  async function updatePluginUnlockStatus() {
    try {
      const res = await PluginUnlockApi.status();
      pluginUnlockState = res;
    } catch (_) {}
  }

  // Get status color mappings
  function getPluginUnlockClass(status: string) {
    if (status === 'injected') return 'success';
    if (status === 'connecting' || status === 'connected') return 'warning';
    if (status === 'failed') return 'danger';
    return 'muted';
  }

  function getPluginUnlockIcon(status: string) {
    if (status === 'injected') return 'bi-unlock';
    if (status === 'connecting') return 'bi-arrow-repeat';
    if (status === 'connected') return 'bi-plug';
    if (status === 'failed') return 'bi-exclamation-triangle';
    return 'bi-lock';
  }

  function getPluginUnlockText(status: string) {
    if (status === 'disconnected') return $t('status.notRunning') || '未运行';
    if (status === 'connecting') return '连接中...';
    if (status === 'connected') return '已连接';
    if (status === 'injected') return '已解锁';
    return pluginUnlockState.message || '失败';
  }

  // Actions
  async function handlePluginUnlock(action: 'start' | 'stop' | 'reinject') {
    try {
      if (action === 'start') await PluginUnlockApi.start();
      else if (action === 'stop') await PluginUnlockApi.stop();
      else await PluginUnlockApi.reinject();
      
      await updatePluginUnlockStatus();
      showToast($t('toast.codexAppRestartRequested'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleSetDefault(id: string) {
    try {
      await CCApi.setDefaultProvider(id);
      await refreshAll();
      showToast($t('toast.defaultUpdatedDesktop'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleTestSpeed(id: string) {
    speedTesting[id] = true;
    speedResults[id] = '';
    try {
      const res = await CCApi.testProvider(id);
      speedResults[id] = res.duration_ms ? `${res.duration_ms}ms` : 'Success';
    } catch (err: any) {
      speedResults[id] = 'Failed';
    } finally {
      speedTesting[id] = false;
    }
  }

  async function handleQueryUsage(id: string) {
    usageQuerying[id] = true;
    usageResults[id] = '';
    try {
      const res = await CCApi.queryProviderUsage(id);
      usageResults[id] = res.quota_text || res.message || $t('providers.usageUnavailable');
    } catch (err: any) {
      usageResults[id] = 'Error';
    } finally {
      usageQuerying[id] = false;
    }
  }

  async function handleDeleteProvider(id: string) {
    if (!confirm($t('providers.deleteMessage'))) return;
    try {
      await CCApi.deleteProvider(id);
      await refreshAll();
      showToast($t('toast.providerDeleted'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleToggleProxy() {
    try {
      if ($appStatus.proxyRunning) {
        await CCApi.stopProxy();
        showToast($t('toast.proxyStopped'));
      } else {
        await CCApi.startProxy();
        showToast($t('toast.proxyStarted'));
      }
      await refreshAll();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleClearDesktop() {
    if (!confirm($t('confirm.desktopClear'))) return;
    try {
      await CCApi.clearDesktop();
      await refreshAll();
      showToast($t('toast.desktopCleared'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleApplyDesktop() {
    try {
      await CCApi.configureDesktop();
      showToast($t('toast.desktopApplied'));
      await refreshAll();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
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

  // Drag and drop functions for Svelte reordering
  function dragStart(e: DragEvent, index: number) {
    draggedIdx = index;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = 'move';
    }
  }

  async function drop(e: DragEvent, index: number) {
    if (draggedIdx === null || draggedIdx === index) return;
    const items = [...$providers];
    const dragged = items[draggedIdx];
    items.splice(draggedIdx, 1);
    items.splice(index, 0, dragged);
    
    providers.set(items);
    draggedIdx = null;

    try {
      const ids = items.map(p => p.id).filter((id): id is string => !!id);
      await CCApi.reorderProviders(ids);
      showToast($t('toast.providersReordered'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
      await refreshAll();
    }
  }

  function handleAddFromPreset(presetId: string) {
    activeTab.set('providers/add');
    window.location.hash = `providers/add?preset=${presetId}`;
  }

  function handleEditProvider(id: string) {
    activeTab.set('providers/edit');
    window.location.hash = `providers/edit/${id}`;
  }

  onMount(() => {
    loadInitialData();
  });
</script>

<!-- Toast Message Container -->
{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="dashboard-container">
  <!-- Top switchboard for provider cards -->
  <section class="switch-board">
    <h2>{$t('dashboard.title')}</h2>
    <p class="subtitle-text">{$t('dashboard.subtitle')}</p>

    <!-- Configured Provider Cards -->
    <div class="provider-configured-list">
      {#each $providers as provider, idx (provider.id)}
        <!-- svelte-ignore a11y_no_noninteractive_element_to_interactive_role -->
        <article 
          class="provider-switch-card" 
          class:active={provider.default}
          draggable="true" 
          on:dragstart={(e) => dragStart(e, idx)}
          on:dragover|preventDefault
          on:drop|preventDefault={(e) => drop(e, idx)}
        >
          <span class="drag-handle"><i class="bi bi-grip-vertical"></i></span>
          <span class="provider-logo">
            {#if provider.logo}
              <img src={provider.logo} alt={provider.name} class="logo-image" />
            {:else}
              <i class="bi {provider.icon || 'bi-plug-fill'} logo-icon"></i>
            {/if}
          </span>
          <span class="provider-main">
            <strong>{provider.name}</strong>
            <span class="truncate">{provider.baseUrl}</span>
          </span>
          <span class="provider-meta truncate">
            {Object.values(provider.mappings || {}).filter(Boolean).slice(0, 2).join(' / ') || provider.apiFormat}
          </span>
          
          <span class="provider-actions">
            {#if provider.default}
              <span class="active-indicator" role="status">
                <i class="bi bi-broadcast"></i>
                <span>{$t('status.active')}</span>
              </span>
            {:else}
              <button class="mac-btn primary compact-enable" on:click={() => handleSetDefault(provider.id || '')}>
                <i class="bi bi-play-fill"></i>
                <span>{$t('providers.enable')}</span>
              </button>
            {/if}
            
            <button class="icon-action" on:click={() => handleTestSpeed(provider.id || '')} disabled={speedTesting[provider.id || '']} title={$t('providers.testSpeed')}>
              <i class="bi bi-lightning-charge"></i>
            </button>
            <button class="icon-action" on:click={() => handleQueryUsage(provider.id || '')} disabled={usageQuerying[provider.id || '']} title={$t('providers.usage')}>
              <i class="bi bi-wallet2"></i>
            </button>
            <button class="icon-action" on:click={() => handleEditProvider(provider.id || '')} title={$t('common.edit')}>
              <i class="bi bi-pencil-square"></i>
            </button>
            <button class="icon-action danger" on:click={() => handleDeleteProvider(provider.id || '')} title={$t('common.delete')}>
              <i class="bi bi-trash"></i>
            </button>
          </span>

          <span class="provider-feedback">
            {#if speedTesting[provider.id || '']}
              <span class="speed-result inline loading">Testing...</span>
            {:else if speedResults[provider.id || '']}
              <span class="speed-result inline">{speedResults[provider.id || '']}</span>
            {/if}

            {#if usageQuerying[provider.id || '']}
              <span class="usage-result inline loading">Querying...</span>
            {:else if usageResults[provider.id || '']}
              <span class="usage-result inline">{usageResults[provider.id || '']}</span>
            {/if}
          </span>
        </article>
      {:else}
        <!-- Fallback to presets if no providers exist -->
        <div class="empty-state">
          <p>{$t('providers.empty')}</p>
        </div>
      {/each}
    </div>

    <!-- Available presets not yet added -->
    {#if presets.filter(p => !$providers.some(configured => configured.id === p.id || configured.name === p.name)).length > 0}
      <section class="dashboard-preset-section">
        <div class="section-title-row">
          <h2>{$t('dashboard.availablePresets')}</h2>
          <p>{$t('dashboard.availablePresetsHint')}</p>
        </div>
        <div class="provider-preset-grid">
          {#each presets.filter(p => !$providers.some(configured => configured.id === p.id || configured.name === p.name)) as preset}
            <button class="provider-switch-card preset-card" on:click={() => handleAddFromPreset(preset.id)}>
              <span class="drag-handle preset-plus"><i class="bi bi-plus-lg"></i></span>
              <span class="provider-logo">
                {#if preset.logo}
                  <img src={preset.logo} alt={preset.name} class="logo-image" />
                {:else}
                  <i class="bi {preset.icon || 'bi-plug-fill'} logo-icon"></i>
                {/if}
              </span>
              <span class="provider-main">
                <strong>{preset.name}</strong>
                <span class="truncate">{preset.baseUrl}</span>
              </span>
              <span class="provider-meta">{preset.apiFormat}</span>
            </button>
          {/each}
        </div>
      </section>
    {/if}
  </section>

  <!-- Status indicator panels grid -->
  <div class="status-grid">
    <!-- CLI status card -->
    <article class="status-card mac-card">
      <h2>{$t('dashboard.desktopStatus')}</h2>
      <div class="hero-status" class:active={$appStatus.desktopConfigured && !$appStatus.desktopHealth.needsApply}>
        <i class="bi {$appStatus.desktopConfigured && !$appStatus.desktopHealth.needsApply ? 'bi-check-lg' : 'bi-exclamation-lg'}"></i>
      </div>
      <strong class="status-label">
        {#if $appStatus.desktopHealth.needsApply}
          {$t('status.needsApply')}
        {:else if $appStatus.desktopConfigured}
          {$t('status.configured')}
        {:else}
          {$t('status.notConfigured')}
        {/if}
      </strong>
    </article>

    <!-- Proxy Forwarding status card -->
    <article class="status-card mac-card">
      <h2>{$t('dashboard.proxyStatus')}</h2>
      <div class="hero-status" class:active={$appStatus.proxyRunning}>
        <i class="bi bi-hdd-network"></i>
        {#if $appStatus.proxyRunning}
          <i class="bi bi-activity badge-icon"></i>
        {/if}
      </div>
      <strong class="status-label">
        {#if $appStatus.proxyRunning}
          {$t('status.running')}: {$appStatus.proxyPort}
        {:else}
          {$t('status.stopped')}
        {/if}
      </strong>
    </article>

    <!-- Active Provider card -->
    <article class="status-card mac-card">
      <h2>{$t('dashboard.activeProvider')}</h2>
      <span class="large-dot" class:active={$appStatus.activeProvider.id !== null}></span>
      <strong class="status-label">{$appStatus.activeProvider.name}</strong>
    </article>

    <!-- Plugin Unlock status card -->
    <article class="status-card mac-card">
      <h2>{$t('dashboard.pluginUnlockStatus')}</h2>
      <div class="hero-status {getPluginUnlockClass(pluginUnlockState.status)}">
        <i class="bi {getPluginUnlockIcon(pluginUnlockState.status)}"></i>
      </div>
      <strong class="status-label">{getPluginUnlockText(pluginUnlockState.status)}</strong>
      
      {#if pluginUnlockState.status === 'injected' || pluginUnlockState.status === 'connected'}
        <div class="plugin-actions">
          <button class="mac-btn" on:click={() => handlePluginUnlock('start')}>
            <i class="bi bi-play"></i><span>{$t('common.start') || '启动'}</span>
          </button>
          <button class="mac-btn" on:click={() => handlePluginUnlock('reinject')}>
            <i class="bi bi-arrow-repeat"></i><span>重新注入</span>
          </button>
          <button class="mac-btn danger" on:click={() => handlePluginUnlock('stop')}>
            <i class="bi bi-stop"></i><span>停止</span>
          </button>
        </div>
      {/if}
    </article>
  </div>

  <!-- Warning banner for CLI config if any issues exist -->
  {#if $appStatus.desktopHealth.needsApply && $appStatus.desktopHealth.issues.length > 0}
    <div class="desktop-warning">
      <i class="bi bi-exclamation-triangle"></i>
      <span>{$appStatus.desktopHealth.issues.join('; ')}</span>
    </div>
  {/if}

  <!-- Quick Actions Row -->
  <div class="quick-actions">
    <button class="mac-btn primary action-button" on:click={handleApplyDesktop}>
      <i class="bi bi-magic"></i>
      <span>{$t('dashboard.configureDesktop')}</span>
    </button>
    <button 
      class="mac-btn action-button" 
      class:primary={!$appStatus.proxyRunning}
      on:click={handleToggleProxy}
    >
      <i class="bi {$appStatus.proxyRunning ? 'bi-stop-circle' : 'bi-play-circle'}"></i>
      <span>{$appStatus.proxyRunning ? $t('proxy.stop') : $t('proxy.start')}</span>
    </button>
    <button class="mac-btn danger action-button" on:click={handleClearDesktop}>
      <i class="bi bi-arrow-counterclockwise"></i>
      <span>{$t('dashboard.clearDesktopConfig')}</span>
    </button>
  </div>

  <!-- Recent Activity log view -->
  <article class="mac-card panel">
    <div class="panel-header">
      <div class="title-with-icon">
        <span class="soft-icon"><i class="bi bi-file-earmark-text"></i></span>
        <h2>{$t('dashboard.recentActivity')}</h2>
      </div>
      <button class="mac-btn" on:click={() => activeTab.set('proxy')}>
        <span>{$t('common.viewAll')}</span>
        <i class="bi bi-chevron-right"></i>
      </button>
    </div>
    
    <div class="activity-list">
      {#each activities as item}
        <div class="activity-row">
          <time>{item.time}</time>
          <span>{item.text}</span>
        </div>
      {:else}
        <div class="activity-empty">
          <p>{$t('codex.historyEmpty') || '暂无最近操作'}</p>
        </div>
      {/each}
    </div>
  </article>
</div>

<style>
  .dashboard-container {
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .switch-board {
    display: flex;
    flex-direction: column;
  }

  .provider-configured-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }

  .provider-preset-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 12px;
    margin-top: 8px;
  }

  /* macOS Style Cards */
  .provider-switch-card {
    display: flex;
    align-items: center;
    padding: 10px 16px;
    background-color: var(--mac-bg-card);
    border: var(--mac-border-highlight);
    border-radius: var(--radius-card);
    box-shadow: var(--mac-shadow-card);
    transition: all var(--transition-fast);
    position: relative;
    cursor: default;
  }

  .provider-switch-card:hover {
    background-color: rgba(255, 255, 255, 0.75);
  }
  @media (prefers-color-scheme: dark) {
    .provider-switch-card:hover {
      background-color: rgba(45, 45, 45, 0.75);
    }
  }

  .provider-switch-card.active {
    border-color: var(--mac-accent);
    box-shadow: 0 0 0 1px var(--mac-accent);
  }

  .drag-handle {
    color: var(--mac-text-secondary);
    cursor: grab;
    margin-right: 12px;
    display: flex;
    align-items: center;
  }

  .provider-logo {
    width: 32px;
    height: 32px;
    border-radius: 6px;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    margin-right: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
  }

  .logo-image {
    width: 20px;
    height: 20px;
    object-fit: contain;
  }

  .logo-icon {
    font-size: 18px;
    color: var(--mac-accent);
  }

  .provider-main {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }

  .provider-main strong {
    font-size: 13px;
    font-weight: 600;
  }

  .truncate {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-size: 11px;
    color: var(--mac-text-secondary);
  }

  .provider-meta {
    width: 150px;
    font-size: 12px;
    color: var(--mac-text-secondary);
    padding: 0 12px;
  }

  .provider-actions {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .active-indicator {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: 11px;
    font-weight: 600;
    color: var(--mac-accent);
    padding: 4px 8px;
    background-color: var(--mac-accent-soft);
    border-radius: 4px;
  }

  .icon-action {
    background: transparent;
    border: none;
    cursor: pointer;
    font-size: 14px;
    color: var(--mac-text-secondary);
    padding: 4px;
    border-radius: 4px;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    transition: all var(--transition-fast);
  }

  .icon-action:hover {
    background-color: rgba(0, 0, 0, 0.05);
    color: var(--mac-text-primary);
  }
  @media (prefers-color-scheme: dark) {
    .icon-action:hover {
      background-color: rgba(255, 255, 255, 0.05);
    }
  }

  .icon-action.danger:hover {
    color: var(--mac-danger);
    background-color: var(--mac-danger-soft);
  }

  .provider-feedback {
    display: flex;
    gap: 8px;
    margin-left: 12px;
  }

  .speed-result, .usage-result {
    font-size: 10px;
    font-weight: 600;
    padding: 2px 6px;
    border-radius: 4px;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
  }
  
  .speed-result.loading, .usage-result.loading {
    color: var(--mac-text-secondary);
    animation: pulse 1.5s infinite;
  }

  /* Preset specific */
  .preset-card {
    text-align: left;
    width: 100%;
    cursor: pointer;
  }
  
  .preset-plus {
    cursor: default;
    color: var(--mac-accent);
  }

  /* Status grid */
  .status-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 16px;
  }

  .status-card {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 20px;
    text-align: center;
  }

  .status-card h2 {
    font-size: 12px;
    font-weight: 700;
    color: var(--mac-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 12px;
  }

  .hero-status {
    width: 48px;
    height: 48px;
    border-radius: 50%;
    background-color: rgba(120, 120, 128, 0.08);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 24px;
    color: var(--mac-text-secondary);
    margin-bottom: 12px;
    position: relative;
  }

  .hero-status.active {
    background-color: var(--mac-accent-soft);
    color: var(--mac-accent);
  }

  .hero-status.success {
    background-color: var(--mac-success-soft);
    color: var(--mac-success);
  }

  .hero-status.warning {
    background-color: var(--mac-accent-soft);
    color: var(--mac-warning);
  }

  .hero-status.danger {
    background-color: var(--mac-danger-soft);
    color: var(--mac-danger);
  }

  .badge-icon {
    position: absolute;
    bottom: -2px;
    right: -2px;
    font-size: 14px;
    color: var(--mac-success);
  }

  .status-label {
    font-size: 13px;
    font-weight: 600;
  }

  .large-dot {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background-color: rgba(120, 120, 128, 0.25);
    margin: 17px 0 17px;
  }

  .large-dot.active {
    background-color: var(--mac-accent);
    box-shadow: 0 0 8px var(--mac-accent);
  }

  .plugin-actions {
    display: flex;
    gap: 4px;
    margin-top: 8px;
  }

  .plugin-actions .mac-btn {
    font-size: 10px;
    padding: 4px 8px;
  }

  /* Warning banner */
  .desktop-warning {
    background-color: var(--mac-danger-soft);
    border: 1px solid rgba(255, 59, 48, 0.2);
    color: var(--mac-text-danger);
    font-size: 12px;
    padding: 10px 16px;
    border-radius: var(--radius-card);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  /* Actions Row */
  .quick-actions {
    display: flex;
    gap: 12px;
  }

  .action-button {
    flex: 1;
    padding: 12px;
    font-size: 14px;
  }

  /* Activities panel */
  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-bottom: 1px solid var(--mac-border-separator);
    padding-bottom: 12px;
    margin-bottom: 12px;
  }

  .title-with-icon {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .title-with-icon h2 {
    font-size: 14px;
    font-weight: 600;
  }

  .soft-icon {
    color: var(--mac-accent);
    font-size: 16px;
  }

  .activity-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 180px;
    overflow-y: auto;
  }

  .activity-row {
    display: flex;
    gap: 16px;
    font-size: 12px;
    padding: 4px 0;
    border-bottom: 1px solid rgba(0, 0, 0, 0.03);
  }
  @media (prefers-color-scheme: dark) {
    .activity-row {
      border-bottom: 1px solid rgba(255, 255, 255, 0.03);
    }
  }

  .activity-row time {
    color: var(--mac-text-secondary);
    min-width: 130px;
    font-family: var(--font-mono);
  }

  .activity-row span {
    flex: 1;
    color: var(--mac-text-primary);
  }

  .activity-empty {
    padding: 24px;
    text-align: center;
    color: var(--mac-text-secondary);
    font-size: 12px;
  }

  .empty-state {
    padding: 40px;
    text-align: center;
    color: var(--mac-text-secondary);
    background-color: rgba(120, 120, 128, 0.03);
    border: 1px dashed var(--mac-border-separator);
    border-radius: var(--radius-card);
    font-size: 13px;
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

  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.5; }
  }
</style>
