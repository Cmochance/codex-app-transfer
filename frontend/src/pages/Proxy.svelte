<script lang="ts">
  import { onMount, onDestroy, afterUpdate } from 'svelte';
  import { CCApi } from '../lib/api';
  import type { ProxyLog, ProxyStatus } from '../lib/api';
  import { appStatus, proxyStatus, proxyLogs, refreshLogs, refreshAll } from '../lib/store';
  import { t } from '../lib/i18n';

  let port = 18080;
  let autoScroll = true;
  let pollInterval: any = null;
  let logTerminal: HTMLDivElement;

  $: {
    if ($appStatus) {
      port = $appStatus.proxyPort;
    }
  }

  async function handleToggleProxy() {
    try {
      if ($appStatus.proxyRunning) {
        await CCApi.stopProxy();
        showToast($t('toast.proxyStopped'));
      } else {
        await CCApi.startProxy(port);
        showToast($t('toast.proxyStarted'));
      }
      await refreshAll();
      await fetchLogsAndStatus();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function fetchLogsAndStatus() {
    try {
      await refreshLogs();
      const state = await CCApi.getProxyStatus();
      proxyStatus.set(state);
    } catch (_) {}
  }

  async function handleClearLogs() {
    try {
      await CCApi.clearLogs();
      proxyLogs.set([]);
      showToast($t('toast.logsCleared'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleOpenLogDir() {
    try {
      await CCApi.openLogDir();
      showToast($t('toast.logDirOpened'));
    } catch (err: any) {
      showToast(err.message || $t('toast.logDirOpenFailed'));
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

  // Auto scroll terminal log to bottom on updates
  afterUpdate(() => {
    if (autoScroll && logTerminal) {
      logTerminal.scrollTop = logTerminal.scrollHeight;
    }
  });

  onMount(() => {
    fetchLogsAndStatus();

    // Poll logs and status every 2 seconds
    pollInterval = setInterval(() => {
      fetchLogsAndStatus();
    }, 2000);
  });

  onDestroy(() => {
    if (pollInterval) clearInterval(pollInterval);
  });

  // Level color class
  function getLogLevelClass(level: string) {
    const l = level.toLowerCase();
    if (l === 'error' || l === 'err') return 'log-error';
    if (l === 'warn' || l === 'warning') return 'log-warn';
    if (l === 'info') return 'log-info';
    return 'log-debug';
  }
</script>

{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="proxy-page">
  <div class="page-title">
    <h1>{$t('proxy.title')}</h1>
    <p class="subtitle-text">{$t('proxy.subtitle')}</p>
  </div>

  <!-- Proxy Control Card -->
  <article class="mac-card control-card">
    <div class="status-indicator-box">
      <span class="pulse-dot" class:active={$appStatus.proxyRunning}></span>
      <div class="meta">
        <strong>{$appStatus.proxyRunning ? $t('status.running') : $t('status.stopped')}</strong>
        <span class="sub">{$t('proxy.localhost')}</span>
      </div>
    </div>

    <!-- Port Input -->
    <div class="port-input-group">
      <label for="proxyPort">{$t('settings.proxyPort')}</label>
      <input 
        class="mac-input port-input" 
        id="proxyPort" 
        type="number" 
        bind:value={port} 
        disabled={$appStatus.proxyRunning} 
      />
    </div>

    <!-- Toggle button -->
    <button 
      class="mac-btn toggle-btn" 
      class:primary={!$appStatus.proxyRunning}
      on:click={handleToggleProxy}
    >
      <i class="bi {$appStatus.proxyRunning ? 'bi-stop-fill' : 'bi-play-fill'}"></i>
      <span>{$appStatus.proxyRunning ? $t('proxy.stop') : $t('proxy.start')}</span>
    </button>
  </article>

  <!-- Stats Grid -->
  <div class="stats-grid">
    <div class="mac-card stat-card">
      <span class="label">{$t('proxy.stats.total')}</span>
      <span class="value">{$proxyStatus.stats.total}</span>
    </div>
    <div class="mac-card stat-card">
      <span class="label text-success">{$t('proxy.stats.success')}</span>
      <span class="value text-success">{$proxyStatus.stats.success}</span>
    </div>
    <div class="mac-card stat-card">
      <span class="label text-danger">{$t('proxy.stats.failed')}</span>
      <span class="value text-danger">{$proxyStatus.stats.failed}</span>
    </div>
    <div class="mac-card stat-card">
      <span class="label">{$t('proxy.stats.today')}</span>
      <span class="value">{$proxyStatus.stats.today}</span>
    </div>
  </div>

  <!-- Terminal log panel -->
  <article class="terminal-panel mac-card">
    <div class="terminal-header">
      <div class="terminal-actions">
        <button class="mac-btn terminal-btn" on:click={handleOpenLogDir}>
          <i class="bi bi-folder2-open"></i>
          <span>{$t('proxy.viewLog')}</span>
        </button>
        <button class="mac-btn terminal-btn" on:click={handleClearLogs}>
          <i class="bi bi-trash"></i>
          <span>{$t('proxy.clearLog')}</span>
        </button>
      </div>

      <label class="mac-switch auto-scroll-switch">
        <input type="checkbox" bind:checked={autoScroll} />
        <span class="mac-switch-slider"></span>
        <span class="switch-label">{$t('proxy.autoScroll')}</span>
      </label>
    </div>

    <!-- Terminal logs list -->
    <div class="terminal-logs" bind:this={logTerminal}>
      {#each $proxyLogs as log}
        <div class="log-line">
          <span class="log-time">{log.at}</span>
          <span class="log-level {getLogLevelClass(log.level)}">[{log.level}]</span>
          <span class="log-message">{log.message}</span>
        </div>
      {:else}
        <div class="logs-empty">
          <i class="bi bi-terminal"></i>
          <p>No active logs in proxy forwarding service.</p>
        </div>
      {/each}
    </div>
  </article>
</div>

<style>
  .proxy-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .control-card {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 24px;
  }

  .status-indicator-box {
    display: flex;
    align-items: center;
    gap: 16px;
  }

  .pulse-dot {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background-color: var(--mac-text-secondary);
    position: relative;
  }

  .pulse-dot.active {
    background-color: var(--mac-success);
    box-shadow: 0 0 8px var(--mac-success);
  }

  .pulse-dot.active::after {
    content: '';
    position: absolute;
    width: 100%;
    height: 100%;
    border-radius: 50%;
    background-color: var(--mac-success);
    animation: ripple 1.6s infinite ease-out;
  }

  @keyframes ripple {
    to {
      transform: scale(2.5);
      opacity: 0;
    }
  }

  .meta {
    display: flex;
    flex-direction: column;
  }

  .meta strong {
    font-size: 14px;
    font-weight: 600;
  }

  .meta .sub {
    font-size: 11px;
    color: var(--mac-text-secondary);
  }

  .port-input-group {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .port-input-group label {
    font-size: 12px;
    font-weight: 600;
    color: var(--mac-text-primary);
  }

  .port-input {
    width: 80px;
    padding: 6px;
    text-align: center;
  }

  .toggle-btn {
    height: 36px;
    padding: 0 16px;
    font-size: 13px;
  }

  /* Stats grid */
  .stats-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 16px;
  }

  .stat-card {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 12px;
    margin: 0;
  }

  .stat-card .label {
    font-size: 11px;
    font-weight: 600;
    color: var(--mac-text-secondary);
    text-transform: uppercase;
    margin-bottom: 4px;
  }

  .stat-card .value {
    font-size: 18px;
    font-weight: 700;
  }

  .text-success { color: var(--mac-success) !important; }
  .text-danger { color: var(--mac-danger) !important; }

  /* Terminal Log panel */
  .terminal-panel {
    display: flex;
    flex-direction: column;
    padding: 0;
    overflow: hidden;
    height: 400px;
    border-radius: 8px;
    background-color: #1e1e24; /* Dark shell color always */
    border: 1px solid rgba(255, 255, 255, 0.08);
  }
  
  .terminal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 16px;
    background-color: rgba(255, 255, 255, 0.04);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .terminal-actions {
    display: flex;
    gap: 8px;
  }

  .terminal-btn {
    background-color: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.08);
    color: #f5f5f7;
    font-size: 11px;
    padding: 4px 8px;
  }

  .terminal-btn:hover {
    background-color: rgba(255, 255, 255, 0.12);
  }

  .auto-scroll-switch {
    display: flex;
    align-items: center;
  }
  
  .auto-scroll-switch .switch-label {
    font-size: 11px;
    font-weight: 600;
    color: #f5f5f7;
  }

  .terminal-logs {
    flex: 1;
    overflow-y: auto;
    padding: 12px;
    font-family: var(--font-mono);
    font-size: 11px;
    line-height: 1.5;
    display: flex;
    flex-direction: column;
    gap: 4px;
    color: #dfdfe5;
  }

  .log-line {
    display: flex;
    word-break: break-all;
  }

  .log-time {
    color: #7d7d8e;
    margin-right: 8px;
    white-space: nowrap;
  }

  .log-level {
    margin-right: 8px;
    font-weight: 600;
    white-space: nowrap;
  }

  .log-info { color: #58a6ff; }
  .log-warn { color: #d29922; }
  .log-error { color: #f85149; }
  .log-debug { color: #8b949e; }

  .log-message {
    flex: 1;
  }

  .logs-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: #7d7d8e;
    gap: 8px;
  }
  
  .logs-empty i {
    font-size: 32px;
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
