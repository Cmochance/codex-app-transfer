<script lang="ts">
  import { onMount } from 'svelte';
  import { activeTab, refreshAll, settings } from './lib/store';
  import Titlebar from './components/Titlebar.svelte';
  import Sidebar from './components/Sidebar.svelte';

  // Import pages
  import Dashboard from './pages/Dashboard.svelte';
  import Providers from './pages/Providers.svelte';
  import ProvidersAdd from './pages/ProvidersAdd.svelte';
  import Proxy from './pages/Proxy.svelte';
  import SettingsPage from './pages/Settings.svelte';
  import Codex from './pages/Codex.svelte';
  import Guide from './pages/Guide.svelte';

  // Helper to parse hash parameters (e.g. #providers/edit/123)
  let routeParams: Record<string, string> = {};

  function handleHashChange() {
    const hash = window.location.hash || '#dashboard';
    const cleanHash = hash.replace(/^#/, '');
    
    // Parse routes
    if (cleanHash.startsWith('providers/edit/')) {
      activeTab.set('providers/edit');
      routeParams = { id: cleanHash.substring('providers/edit/'.length) };
    } else {
      activeTab.set(cleanHash);
      routeParams = {};
    }
  }

  // Update theme classes on document based on settings store changes
  $: {
    if (typeof document !== 'undefined' && $settings) {
      const palette = $settings.theme || 'default';
      document.documentElement.setAttribute('data-theme-palette', palette);
    }
  }

  onMount(async () => {
    // Listen to hash router
    window.addEventListener('hashchange', handleHashChange);
    handleHashChange();

    // Initial state fetch
    await refreshAll();
  });
</script>

<div id="app-root">
  <!-- Top macOS Titlebar -->
  <Titlebar />

  <!-- Workspace area -->
  <div class="app-workspace">
    <!-- Left Navigation Sidebar -->
    <Sidebar />

    <!-- Main page viewport -->
    <main class="app-viewport">
      {#if $activeTab === 'dashboard'}
        <Dashboard />
      {:else if $activeTab === 'providers'}
        <Providers />
      {:else if $activeTab === 'providers/add' || $activeTab === 'providers/edit'}
        <ProvidersAdd id={routeParams.id} />
      {:else if $activeTab === 'proxy'}
        <Proxy />
      {:else if $activeTab === 'codex'}
        <Codex />
      {:else if $activeTab === 'guide'}
        <Guide />
      {:else if $activeTab === 'settings'}
        <SettingsPage />
      {:else}
        <div class="mac-card">
          <h1>404 Not Found</h1>
          <p class="subtitle-text">The requested view {$activeTab} could not be resolved.</p>
        </div>
      {/if}
    </main>
  </div>
</div>

<style>
  /* App Root fills the viewport completely */
  #app-root {
    display: flex;
    flex-direction: column;
    height: 100vh;
    width: 100vw;
  }
</style>
