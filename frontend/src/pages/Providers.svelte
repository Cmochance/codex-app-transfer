<script lang="ts">
  import { onMount } from 'svelte';
  import { CCApi } from '../lib/api';
  import type { Provider } from '../lib/api';
  import { providers, appStatus, activeTab, refreshAll } from '../lib/store';
  import { t } from '../lib/i18n';

  let currentExposeAll = false;

  $: {
    currentExposeAll = $appStatus.exposeAllProviderModels;
  }

  async function handleToggleModelMenu() {
    try {
      const nextVal = !currentExposeAll;
      await CCApi.saveSettings({ exposeAllProviderModels: nextVal });
      
      // Update local state in appStatus store
      appStatus.update(s => ({ ...s, exposeAllProviderModels: nextVal }));
      
      showToast(nextVal ? $t('toast.allModelsEnabled') : $t('toast.singleModelEnabled'));
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

  async function handleDelete(id: string) {
    if (!confirm($t('providers.deleteMessage'))) return;
    try {
      await CCApi.deleteProvider(id);
      await refreshAll();
      showToast($t('toast.providerDeleted'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  function handleEdit(id: string) {
    activeTab.set('providers/edit');
    window.location.hash = `providers/edit/${id}`;
  }

  function handleAdd() {
    activeTab.set('providers/add');
    window.location.hash = 'providers/add';
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
</script>

{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="providers-page">
  <div class="page-title-row">
    <div>
      <h1>{$t('providers.title')}</h1>
      <p class="subtitle-text">{$t('providers.subtitle')}</p>
    </div>
    <button class="mac-btn primary btn-lg" on:click={handleAdd}>
      <i class="bi bi-plus-lg"></i>
      <span>{$t('providers.add')}</span>
    </button>
  </div>

  <!-- Claude Model Menu Configuration Card -->
  <article class="mac-card model-menu-panel">
    <div class="panel-main">
      <h2>{$t('providers.modelMenuTitle')}</h2>
      <p class="panel-hint">
        {currentExposeAll ? $t('providers.modelMenuAllHint') : $t('providers.modelMenuSingleHint')}
      </p>
    </div>
    <button 
      class="mac-btn" 
      class:primary={currentExposeAll}
      on:click={handleToggleModelMenu}
    >
      <i class="bi bi-list-stars"></i>
      <span>{currentExposeAll ? $t('providers.showSingleModel') : $t('providers.showAllModels')}</span>
    </button>
  </article>

  <!-- Providers Table Panel -->
  <article class="mac-card table-panel">
    <div class="table-header">
      <span class="col-icon"></span>
      <span class="col-name">{$t('providers.name')}</span>
      <span class="col-url">{$t('providers.baseUrl')}</span>
      <span class="col-mapping">{$t('providers.mapping')}</span>
      <span class="col-status">{$t('providers.status')}</span>
      <span class="col-actions">{$t('providers.actions')}</span>
    </div>

    <div class="table-rows">
      {#each $providers as provider}
        <div class="table-row" class:default-provider={provider.default}>
          <!-- Logo Icon -->
          <span class="col-icon">
            {#if provider.logo}
              <img src={provider.logo} alt={provider.name} class="logo-image" />
            {:else}
              <i class="bi {provider.icon || 'bi-plug-fill'} logo-icon"></i>
            {/if}
          </span>

          <!-- Name -->
          <span class="col-name font-semibold">
            {provider.name}
            {#if provider.isBuiltin}
              <span class="badge builtin">Built-in</span>
            {/if}
          </span>

          <!-- URL -->
          <span class="col-url truncate" title={provider.baseUrl}>{provider.baseUrl}</span>

          <!-- Mapping -->
          <span class="col-mapping truncate" title={provider.apiFormat}>
            {Object.values(provider.mappings || {}).filter(Boolean).slice(0, 2).join(' / ') || provider.apiFormat}
          </span>

          <!-- Status -->
          <span class="col-status">
            {#if provider.default}
              <span class="status-indicator active">
                <span class="dot green"></span>
                {$t('status.active')}
              </span>
            {:else}
              <span class="status-indicator standby">
                <span class="dot gray"></span>
                {$t('status.standby')}
              </span>
            {/if}
          </span>

          <!-- Actions -->
          <span class="col-actions">
            {#if !provider.default}
              <button class="mac-btn compact-btn" on:click={() => handleSetDefault(provider.id || '')}>
                {$t('providers.setDefault')}
              </button>
            {/if}
            <button class="mac-btn compact-btn" on:click={() => handleEdit(provider.id || '')}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="mac-btn compact-btn danger" on:click={() => handleDelete(provider.id || '')}>
              <i class="bi bi-trash"></i>
            </button>
          </span>
        </div>
      {:else}
        <div class="empty-rows">
          <p>{$t('providers.empty')}</p>
        </div>
      {/each}
    </div>
  </article>
</div>

<style>
  .providers-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .page-title-row {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
  }

  /* Model menu settings card */
  .model-menu-panel {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .panel-main {
    flex: 1;
    padding-right: 24px;
  }

  .panel-main h2 {
    font-size: 13px;
    font-weight: 600;
    margin-bottom: 4px;
  }

  .panel-hint {
    font-size: 11px;
    color: var(--mac-text-secondary);
    line-height: 1.4;
  }

  /* Table Style Layout */
  .table-panel {
    padding: 0;
    overflow: hidden;
  }

  .table-header {
    display: flex;
    padding: 10px 16px;
    background-color: rgba(0, 0, 0, 0.02);
    border-bottom: 1px solid var(--mac-border-separator);
    font-size: 11px;
    font-weight: 700;
    color: var(--mac-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  @media (prefers-color-scheme: dark) {
    .table-header {
      background-color: rgba(255, 255, 255, 0.02);
    }
  }

  .table-rows {
    display: flex;
    flex-direction: column;
  }

  .table-row {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--mac-border-separator);
    transition: background-color var(--transition-fast);
  }

  .table-row:last-child {
    border-bottom: none;
  }

  .table-row:hover {
    background-color: rgba(0, 0, 0, 0.015);
  }
  @media (prefers-color-scheme: dark) {
    .table-row:hover {
      background-color: rgba(255, 255, 255, 0.015);
    }
  }

  .table-row.default-provider {
    background-color: rgba(0, 122, 255, 0.02);
  }

  /* Columns sizes */
  .col-icon {
    width: 32px;
    display: flex;
    align-items: center;
  }
  
  .col-name {
    width: 160px;
    font-size: 13px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .col-url {
    flex: 2;
    min-width: 0;
    font-size: 12px;
    color: var(--mac-text-secondary);
    padding-right: 12px;
  }

  .col-mapping {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    color: var(--mac-text-secondary);
    padding-right: 12px;
  }

  .col-status {
    width: 100px;
  }

  .col-actions {
    width: 180px;
    display: flex;
    justify-content: flex-end;
    gap: 6px;
  }

  /* Mini indicators */
  .logo-image {
    width: 20px;
    height: 20px;
    object-fit: contain;
    border-radius: 4px;
  }

  .logo-icon {
    font-size: 16px;
    color: var(--mac-text-secondary);
  }

  .badge {
    font-size: 9px;
    font-weight: 700;
    padding: 1px 4px;
    border-radius: 4px;
    text-transform: uppercase;
  }
  
  .badge.builtin {
    background-color: rgba(120, 120, 128, 0.1);
    color: var(--mac-text-secondary);
  }

  .status-indicator {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    font-weight: 500;
  }

  .status-indicator.active {
    color: var(--mac-success);
  }

  .status-indicator.standby {
    color: var(--mac-text-secondary);
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
  }

  .dot.green {
    background-color: var(--mac-success);
  }

  .dot.gray {
    background-color: var(--mac-text-secondary);
  }

  .compact-btn {
    font-size: 11px;
    padding: 4px 8px;
    height: 24px;
  }

  .empty-rows {
    padding: 32px;
    text-align: center;
    color: var(--mac-text-secondary);
    font-size: 12px;
  }

  .btn-lg {
    font-size: 13px;
    padding: 8px 16px;
    border-radius: var(--radius-button);
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
