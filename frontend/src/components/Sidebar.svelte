<script lang="ts">
  import { activeTab } from '../lib/store';
  import { t } from '../lib/i18n';

  interface NavItem {
    id: string;
    icon: string;
    labelKey: string;
  }

  const navItems: NavItem[] = [
    { id: 'dashboard', icon: 'bi-speedometer2', labelKey: 'nav.dashboard' },
    { id: 'providers', icon: 'bi-plug', labelKey: 'nav.providers' },
    { id: 'proxy', icon: 'bi-broadcast-pin', labelKey: 'nav.proxy' },
    { id: 'codex', icon: 'bi-bookmark', labelKey: 'nav.codex' },
    { id: 'guide', icon: 'bi-book', labelKey: 'nav.guide' },
    { id: 'settings', icon: 'bi-gear', labelKey: 'nav.settings' }
  ];

  function selectTab(id: string) {
    activeTab.set(id);
    window.location.hash = id;
  }
</script>

<aside class="app-sidebar">
  <div class="sidebar-header-spacer"></div>
  <div class="sidebar-group">
    <div class="sidebar-group-title">{$t('codex.assetType', { defaultValue: '导航' })}</div>
    
    {#each navItems as item}
      <button 
        class="sidebar-item" 
        class:active={$activeTab === item.id || $activeTab.startsWith(item.id + '/')}
        on:click={() => selectTab(item.id)}
      >
        <span class="sidebar-icon"><i class="bi {item.icon}"></i></span>
        <span class="sidebar-label">{$t(item.labelKey)}</span>
      </button>
    {/each}
  </div>
</aside>

<style>
  .sidebar-header-spacer {
    height: 12px;
  }
  
  .sidebar-group {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
  }

  .sidebar-group-title {
    font-size: 10px;
    font-weight: 700;
    color: var(--mac-text-secondary);
    padding: 6px 12px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .sidebar-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border: none;
    background: transparent;
    border-radius: var(--radius-button);
    color: var(--mac-text-primary);
    font-family: var(--font-sans);
    font-size: 13px;
    font-weight: 500;
    text-align: left;
    cursor: pointer;
    transition: background-color var(--transition-fast), color var(--transition-fast);
    width: 100%;
    outline: none;
  }

  .sidebar-item:hover {
    background-color: rgba(0, 0, 0, 0.04);
  }
  @media (prefers-color-scheme: dark) {
    .sidebar-item:hover {
      background-color: rgba(255, 255, 255, 0.04);
    }
  }

  .sidebar-item.active {
    background-color: var(--mac-accent);
    color: #ffffff;
  }

  .sidebar-icon {
    font-size: 14px;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 18px;
    height: 18px;
  }

  .sidebar-label {
    flex: 1;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
