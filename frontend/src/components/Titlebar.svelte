<script lang="ts">
  import { activeTab, appStatus, refreshAll } from '../lib/store';
  import { t, locale, setLocale } from '../lib/i18n';
  import { CCApi } from '../lib/api';

  async function handleRefresh() {
    await refreshAll();
  }

  function toggleLanguage() {
    const nextLocale = $locale === 'zh' ? 'en' : 'zh';
    setLocale(nextLocale);
    // Also save settings to backend if needed, or keep local
    try {
      CCApi.saveSettings({ language: nextLocale });
    } catch (_) {}
  }

  async function toggleTheme() {
    // Read current theme settings
    try {
      const settings = await CCApi.getSettings();
      const currentTheme = settings.theme || 'default';
      
      // Toggle logic or trigger backend theme action
      // Here we just toggle preferences or let setting handle it
      // Let's call setting API to switch dark/light mode
    } catch (_) {}
  }
</script>

<div class="window-titlebar">
  <!-- Native macOS drag region -->
  <div class="drag-region" data-tauri-drag-region></div>

  <!-- Window title in the center -->
  <div class="titlebar-center">
    {$t('nav.' + $activeTab, { defaultValue: 'Codex App Transfer' })}
  </div>

  <!-- Actions on the right -->
  <div class="titlebar-right">
    <!-- Refresh Button -->
    <button class="mac-btn" on:click={handleRefresh} title={$t('codex.mcp.refresh')}>
      <i class="bi bi-arrow-clockwise"></i>
    </button>

    <!-- Language Switcher -->
    <button class="mac-btn" on:click={toggleLanguage}>
      {$locale === 'zh' ? 'EN' : '中'}
    </button>

    <!-- Settings shortcut -->
    <button 
      class="mac-btn" 
      class:primary={$activeTab === 'settings'}
      on:click={() => activeTab.set('settings')}
      title={$t('nav.settings')}
    >
      <i class="bi bi-gear"></i>
    </button>

    <!-- Add Provider shortcut -->
    <button 
      class="mac-btn primary" 
      on:click={() => activeTab.set('providers/add')}
      title={$t('providers.add')}
    >
      <i class="bi bi-plus-lg"></i>
    </button>
  </div>
</div>

<style>
  .window-titlebar {
    position: relative;
  }
  .mac-btn {
    height: 28px;
    width: 28px;
    padding: 0;
    font-size: 12px;
  }
</style>
