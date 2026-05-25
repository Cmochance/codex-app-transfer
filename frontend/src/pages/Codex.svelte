<script lang="ts">
  import { onMount } from 'svelte';
  import { CCApi } from '../lib/api';
  import { t } from '../lib/i18n';

  let currentTab = 'agents'; // 'agents' | 'memories' | 'mcp' | 'skills'
  let subMcpTab = 'servers'; // 'servers' | 'plugins'

  // Generic Markdown Editor variables
  let paths: any[] = [];
  let selectedHash = '';
  let selectedPath: any = null;
  
  let mdContent = '';
  let editContent = '';
  let isEditing = false;
  let historyList: any[] = [];
  let showHistoryModal = false;
  let diffContent = '';

  // MCP Servers variables
  let mcpServers: Record<string, any> = {};
  let selectedMcpServerId = '';
  let mcpCommand = '';
  let mcpArgs: string[] = [];
  let mcpEnvs: Record<string, string> = {};
  let showRawTomlEditor = false;
  let rawTomlContent = '';

  // MCP Plugins variables
  let mcpPlugins: any[] = [];
  let pluginSearchQuery = '';

  // Skills variables
  let skills: any[] = [];
  let skillBackups: any[] = [];
  let skillMdContent = '';
  let skillMdEditing = false;

  // Load paths and preview for Markdown editor (Agents/Memories)
  async function loadMarkdownDoc(tabType: string) {
    isEditing = false;
    mdContent = '';
    paths = [];
    selectedHash = '';
    selectedPath = null;
    historyList = [];

    try {
      const endpoint = tabType === 'agents' ? 'agents-md' : 'memories-md';
      const res = await fetch(`/api/codex/${endpoint}/paths`).then(r => r.json());
      paths = res.entries || [];
      
      if (paths.length > 0) {
        selectedPath = paths[0];
        selectedHash = selectedPath.hash;
        await loadRawContent(tabType, selectedHash);
      }
    } catch (err) {
      console.error(err);
    }
  }

  async function loadRawContent(tabType: string, hash: string) {
    try {
      const endpoint = tabType === 'agents' ? 'agents-md' : 'memories-md';
      const res = await fetch(`/api/codex/${endpoint}/raw?hash=${encodeURIComponent(hash)}`).then(r => r.json());
      mdContent = res.content || '';
      editContent = mdContent;
    } catch (err) {
      console.error(err);
    }
  }

  async function handleHashChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    selectedHash = target.value;
    selectedPath = paths.find(p => p.hash === selectedHash);
    isEditing = false;
    await loadRawContent(currentTab, selectedHash);
  }

  async function handleApply() {
    try {
      const endpoint = currentTab === 'agents' ? 'agents-md' : 'memories-md';
      const r = await fetch(`/api/codex/${endpoint}/raw?hash=${encodeURIComponent(selectedHash)}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ content: editContent }),
      });
      if (!r.ok) throw new Error('Apply failed');
      
      showToast($t('codex.agentsApplyOk'));
      mdContent = editContent;
      isEditing = false;
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleBackup() {
    try {
      const endpoint = currentTab === 'agents' ? 'agents-md' : 'memories-md';
      const r = await fetch(`/api/codex/${endpoint}/backup?hash=${encodeURIComponent(selectedHash)}`, {
        method: 'POST',
      });
      if (!r.ok) throw new Error('Backup failed');
      showToast($t('codex.agentsBackupOk'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleLoadHistory() {
    try {
      const endpoint = currentTab === 'agents' ? 'agents-md' : 'memories-md';
      const res = await fetch(`/api/codex/${endpoint}/history?hash=${encodeURIComponent(selectedHash)}`).then(r => r.json());
      historyList = res.history || [];
      showHistoryModal = true;
    } catch (err: any) {
      showToast(err.message || 'Failed to load history');
    }
  }

  async function handleRollback(index: number) {
    if (!confirm('Confirm rollback?')) return;
    try {
      const endpoint = currentTab === 'agents' ? 'agents-md' : 'memories-md';
      const r = await fetch(`/api/codex/${endpoint}/rollback?hash=${encodeURIComponent(selectedHash)}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ index }),
      });
      if (!r.ok) throw new Error('Rollback failed');
      
      showHistoryModal = false;
      showToast('Rollback successful');
      await loadRawContent(currentTab, selectedHash);
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  // MCP Servers CRUD
  async function loadMcpServers() {
    try {
      const res = await fetch('/api/codex/mcp/servers').then(r => r.json());
      mcpServers = res.mcpServers || {};
      
      const keys = Object.keys(mcpServers);
      if (keys.length > 0) {
        selectMcpServer(keys[0]);
      }
    } catch (err) {
      console.error(err);
    }
  }

  function selectMcpServer(id: string) {
    selectedMcpServerId = id;
    const server = mcpServers[id] || {};
    mcpCommand = server.command || '';
    mcpArgs = server.args || [];
    mcpEnvs = server.env || {};
  }

  async function handleMcpServerSave() {
    try {
      const payload = {
        id: selectedMcpServerId,
        command: mcpCommand,
        args: mcpArgs,
        env: mcpEnvs
      };

      const r = await fetch('/api/codex/mcp/servers', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload)
      });
      if (!r.ok) throw new Error('Save failed');

      showToast($t('codex.mcp.saveOk'));
      await loadMcpServers();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleMcpServerDelete() {
    if (!confirm('Delete this server?')) return;
    try {
      const r = await fetch('/api/codex/mcp/servers/delete', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: selectedMcpServerId })
      });
      if (!r.ok) throw new Error('Delete failed');

      showToast('Deleted successfully');
      await loadMcpServers();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function loadRawToml() {
    try {
      const res = await fetch('/api/codex/mcp/config/raw').then(r => r.json());
      rawTomlContent = res.content || '';
      showRawTomlEditor = true;
    } catch (_) {}
  }

  async function handleSaveRawToml() {
    try {
      const r = await fetch('/api/codex/mcp/config/raw', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ content: rawTomlContent })
      });
      if (!r.ok) throw new Error('TOML Save failed');

      showToast($t('codex.mcp.saveOk'));
      showRawTomlEditor = false;
      await loadMcpServers();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  // MCP Plugins
  async function loadMcpPlugins() {
    try {
      const res = await fetch('/api/codex/mcp/plugins').then(r => r.json());
      mcpPlugins = res.plugins || [];
    } catch (_) {}
  }

  async function handleTogglePlugin(id: string) {
    try {
      await fetch('/api/codex/mcp/plugins/toggle', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id })
      });
      await loadMcpPlugins();
    } catch (_) {}
  }

  async function handleUninstallPlugin(id: string) {
    if (!confirm('Uninstall plugin?')) return;
    try {
      await fetch('/api/codex/mcp/plugins/uninstall', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id })
      });
      showToast($t('codex.mcp.uninstallOk'));
      await loadMcpPlugins();
    } catch (_) {}
  }

  // Skills
  async function loadSkillsData() {
    try {
      const res = await fetch('/api/codex/skills/list').then(r => r.json());
      skills = res.skills || [];
      
      const backupsRes = await fetch('/api/codex/skills/backups').then(r => r.json());
      skillBackups = backupsRes.backups || [];

      // Load SKILL.md
      const skillMdRes = await fetch('/api/codex/skills-md/raw').then(r => r.json());
      skillMdContent = skillMdRes.content || '';
    } catch (_) {}
  }

  async function handleSkillsBackup() {
    try {
      const r = await fetch('/api/codex/skills/backup', { method: 'POST' }).then(r => r.json());
      showToast($t('codex.toastSkillsBackedUp', { name: r.filename || 'backup' }));
      await loadSkillsData();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleSkillsRestore(filename: string) {
    if (!confirm($t('codex.confirmSkillsRestore', { filename }))) return;
    try {
      await fetch('/api/codex/skills/restore', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ filename })
      });
      showToast($t('codex.toastSkillsRestored', { filename }));
      await loadSkillsData();
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleSkillMdSave() {
    try {
      const r = await fetch('/api/codex/skills-md/raw', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ content: skillMdContent })
      });
      if (!r.ok) throw new Error('Save failed');

      showToast('SKILL.md saved');
      skillMdEditing = false;
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleSkillsReveal() {
    try {
      await fetch('/api/codex/skills-md/reveal', { method: 'POST' });
    } catch (_) {}
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

  // Helper to add MCP server environment variable row
  let newEnvKey = '';
  let newEnvVal = '';
  function handleAddEnv() {
    if (!newEnvKey.trim()) return;
    mcpEnvs[newEnvKey.trim()] = newEnvVal.trim();
    mcpEnvs = { ...mcpEnvs };
    newEnvKey = '';
    newEnvVal = '';
  }

  function handleRemoveEnv(key: string) {
    delete mcpEnvs[key];
    mcpEnvs = { ...mcpEnvs };
  }

  // Helper to add argument row
  let newArg = '';
  function handleAddArg() {
    if (!newArg.trim()) return;
    mcpArgs = [...mcpArgs, newArg.trim()];
    newArg = '';
  }

  function handleRemoveArg(index: number) {
    mcpArgs = mcpArgs.filter((_, i) => i !== index);
  }

  // Sidebar selector trigger
  $: {
    if (currentTab === 'agents' || currentTab === 'memories') {
      loadMarkdownDoc(currentTab);
    } else if (currentTab === 'mcp') {
      if (subMcpTab === 'servers') loadMcpServers();
      else loadMcpPlugins();
    } else if (currentTab === 'skills') {
      loadSkillsData();
    }
  }

  onMount(() => {
    loadMarkdownDoc('agents');
  });
</script>

{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="codex-page">
  <div class="page-title">
    <h1>{$t('codex.title')}</h1>
    <p class="subtitle-text">{$t('codex.subtitle')}</p>
  </div>

  <div class="codex-layout-wrapper">
    <!-- Sub-navigation Sidebar -->
    <nav class="codex-sub-sidebar">
      <button class:active={currentTab === 'agents'} on:click={() => currentTab = 'agents'}>
        <i class="bi bi-file-text"></i>
        <span>{$t('codex.tabAgents')}</span>
      </button>
      <button class:active={currentTab === 'memories'} on:click={() => currentTab = 'memories'}>
        <i class="bi bi-journal-text"></i>
        <span>{$t('codex.tabMemories')}</span>
      </button>
      <button class:active={currentTab === 'mcp'} on:click={() => currentTab = 'mcp'}>
        <i class="bi bi-plug"></i>
        <span>{$t('codex.tabMcp')}</span>
      </button>
      <button class:active={currentTab === 'skills'} on:click={() => currentTab = 'skills'}>
        <i class="bi bi-collection"></i>
        <span>{$t('codex.tabSkills')}</span>
      </button>
    </nav>

    <!-- Content Workspace -->
    <div class="codex-content-workspace">
      <!-- 1. AGENTS & MEMORIES (Markdown editor layouts) -->
      {#if currentTab === 'agents' || currentTab === 'memories'}
        <div class="markdown-editor-pane">
          <div class="doc-paths-toolbar">
            <select class="mac-input path-picker" on:change={handleHashChange} bind:value={selectedHash}>
              {#each paths as path}
                <option value={path.hash}>
                  [{path.category === 'global' ? $t('codex.agentsPath.global') : $t('codex.agentsPath.projectRoot')}] {path.path}
                </option>
              {/each}
            </select>
          </div>

          <div class="doc-warning-banner">
            <i class="bi bi-exclamation-triangle-fill"></i>
            <span>
              {currentTab === 'agents' ? $t('codex.agentsWarn') : $t('codex.memoriesWarn')}
            </span>
          </div>

          <!-- Preview & Editor Area -->
          <div class="editor-content-area mac-card">
            {#if isEditing}
              <textarea class="mac-input textarea-editor" bind:value={editContent}></textarea>
            {:else}
              <pre class="markdown-preview-pane">{mdContent || 'Document is empty.'}</pre>
            {/if}
          </div>

          <!-- Bottom Actions -->
          <div class="editor-actions">
            {#if !isEditing}
              <button class="mac-btn primary" on:click={() => { editContent = mdContent; isEditing = true; }}>
                <i class="bi bi-pencil"></i>
                <span>{$t('codex.agentsEdit')}</span>
              </button>
              <button class="mac-btn" on:click={handleBackup}>
                <i class="bi bi-archive"></i>
                <span>{$t('codex.agentsBackup')}</span>
              </button>
              <button class="mac-btn" on:click={handleLoadHistory}>
                <i class="bi bi-clock-history"></i>
                <span>{$t('codex.history')}</span>
              </button>
            {:else}
              <button class="mac-btn primary" on:click={handleApply}>
                <i class="bi bi-check2-circle"></i>
                <span>{$t('codex.apply')}</span>
              </button>
              <button class="mac-btn" on:click={() => isEditing = false}>
                <i class="bi bi-x"></i>
                <span>{$t('codex.agentsCancel')}</span>
              </button>
            {/if}
          </div>
        </div>
      {/if}

      <!-- 2. MCP TAB -->
      {#if currentTab === 'mcp'}
        <div class="mcp-editor-pane">
          <!-- MCP Sub-tabs navigation -->
          <div class="mcp-sub-nav">
            <button class:active={subMcpTab === 'servers'} on:click={() => subMcpTab = 'servers'}>
              <i class="bi bi-hdd-network"></i> Servers
            </button>
            <button class:active={subMcpTab === 'plugins'} on:click={() => subMcpTab = 'plugins'}>
              <i class="bi bi-box-seam"></i> Plugins
            </button>
          </div>

          <!-- MCP Servers sub-pane -->
          {#if subMcpTab === 'servers'}
            <div class="mcp-servers-layout">
              <div class="mcp-sidebar-list">
                {#each Object.keys(mcpServers) as serverId}
                  <button class="list-item" class:active={selectedMcpServerId === serverId} on:click={() => selectMcpServer(serverId)}>
                    <strong>{serverId}</strong>
                    <span class="truncate">{mcpServers[serverId].command}</span>
                  </button>
                {/each}
                <button class="mac-btn primary mt-2 w-100" on:click={loadRawToml}>
                  <i class="bi bi-code-square"></i> Edit Raw TOML
                </button>
              </div>

              <!-- Server Config Form -->
              <div class="mcp-detail-form mac-card">
                {#if selectedMcpServerId}
                  <h3>Server ID: {selectedMcpServerId}</h3>
                  
                  <div class="form-group">
                    <label class="form-label" for="mcpCmd">Command</label>
                    <input class="mac-input" id="mcpCmd" bind:value={mcpCommand} />
                  </div>

                  <!-- Arguments -->
                  <div class="form-group">
                    <label class="form-label">Arguments</label>
                    <div class="mcp-args-list">
                      {#each mcpArgs as arg, idx}
                        <div class="arg-item">
                          <span>{arg}</span>
                          <button type="button" on:click={() => handleRemoveArg(idx)} class="remove-btn"><i class="bi bi-x"></i></button>
                        </div>
                      {/each}
                    </div>
                    <div class="input-with-button mt-2">
                      <input class="mac-input" bind:value={newArg} placeholder="Add arg..." />
                      <button class="mac-btn" type="button" on:click={handleAddArg}><i class="bi bi-plus"></i></button>
                    </div>
                  </div>

                  <!-- Envs -->
                  <div class="form-group">
                    <label class="form-label">Environment Variables</label>
                    <div class="mcp-envs-list">
                      {#each Object.entries(mcpEnvs) as [envKey, envVal]}
                        <div class="env-item">
                          <strong>{envKey}</strong> = <span>{envVal}</span>
                          <button type="button" on:click={() => handleRemoveEnv(envKey)} class="remove-btn"><i class="bi bi-x"></i></button>
                        </div>
                      {/each}
                    </div>
                    <div class="env-input-row mt-2">
                      <input class="mac-input env-key-input" bind:value={newEnvKey} placeholder="Key" />
                      <input class="mac-input env-val-input" bind:value={newEnvVal} placeholder="Value" />
                      <button class="mac-btn" type="button" on:click={handleAddEnv}><i class="bi bi-plus"></i></button>
                    </div>
                  </div>

                  <div class="form-actions">
                    <button class="mac-btn primary" on:click={handleMcpServerSave}>Save</button>
                    <button class="mac-btn danger" on:click={handleMcpServerDelete}>Delete</button>
                  </div>
                {:else}
                  <p class="empty-text">Select or add an MCP server on the left sidebar.</p>
                {/if}
              </div>
            </div>
          {/if}

          <!-- MCP Plugins sub-pane -->
          {#if subMcpTab === 'plugins'}
            <div class="mcp-plugins-pane">
              <input class="mac-input" bind:value={pluginSearchQuery} placeholder="Search plugins..." />
              
              <div class="plugins-list mt-2">
                {#each mcpPlugins.filter(p => p.id.toLowerCase().includes(pluginSearchQuery.toLowerCase())) as plugin}
                  <div class="plugin-row mac-card">
                    <div class="meta">
                      <strong>{plugin.name || plugin.id}</strong>
                      <span class="truncate">{plugin.description || 'No description provided.'}</span>
                    </div>
                    <div class="actions">
                      <label class="mac-switch">
                        <input type="checkbox" checked={plugin.enabled} on:change={() => handleTogglePlugin(plugin.id)} />
                        <span class="mac-switch-slider"></span>
                      </label>
                      <button class="mac-btn danger compact" on:click={() => handleUninstallPlugin(plugin.id)}>Uninstall</button>
                    </div>
                  </div>
                {:else}
                  <p class="empty-text">No plugins installed yet.</p>
                {/each}
              </div>
            </div>
          {/if}
        </div>
      {/if}

      <!-- 3. SKILLS TAB -->
      {#if currentTab === 'skills'}
        <div class="skills-editor-pane">
          <div class="skills-top-bar">
            <h2>Installed Skills</h2>
            <div class="actions">
              <button class="mac-btn" on:click={handleSkillsBackup}>Backup Now</button>
              <button class="mac-btn" on:click={handleSkillsReveal}>Open Folder</button>
            </div>
          </div>

          <div class="skills-layout-grid">
            <div class="skills-list-panel mac-card">
              {#each skills as skill}
                <div class="skill-item">
                  <span><i class="bi bi-box"></i> {skill.name || 'Unnamed Skill'}</span>
                  {#if skill.hasSkillMd}
                    <span class="badge ok">✓ SKILL.md</span>
                  {:else}
                    <span class="badge err">✗ no SKILL.md</span>
                  {/if}
                </div>
              {:else}
                <p class="empty-text">No skills detected. Click Open Folder to add skill scripts.</p>
              {/each}
            </div>

            <div class="skills-backup-panel mac-card">
              <h3>Existing Backups</h3>
              {#each skillBackups as backup}
                <div class="backup-row">
                  <span class="truncate">{backup.filename}</span>
                  <button class="mac-btn compact" on:click={() => handleSkillsRestore(backup.filename)}>Restore</button>
                </div>
              {:else}
                <p class="empty-text">No backups created yet.</p>
              {/each}
            </div>
          </div>

          <!-- SKILL.md editor panel -->
          <div class="skill-md-editor mac-card mt-2">
            <div class="section-title-row">
              <h3>SKILL.md documentation</h3>
              {#if !skillMdEditing}
                <button class="mac-btn compact-btn" on:click={() => skillMdEditing = true}>Edit</button>
              {:else}
                <div class="actions">
                  <button class="mac-btn compact-btn primary" on:click={handleSkillMdSave}>Save</button>
                  <button class="mac-btn compact-btn" on:click={() => skillMdEditing = false}>Cancel</button>
                </div>
              {/if}
            </div>
            
            <div class="editor-content-area">
              {#if skillMdEditing}
                <textarea class="mac-input textarea-editor" bind:value={skillMdContent}></textarea>
              {:else}
                <pre class="markdown-preview-pane">{skillMdContent || 'SKILL.md is empty.'}</pre>
              {/if}
            </div>
          </div>
        </div>
      {/if}
    </div>
  </div>
</div>

<!-- History snapshot picker Modal -->
{#if showHistoryModal}
  <div class="mac-overlay">
    <div class="mac-modal history-modal">
      <h2>History snapshots (Cap 10)</h2>
      
      <div class="history-snapshots-list">
        {#each historyList as entry}
          <div class="history-row">
            <div class="meta">
              <strong>Index: {entry.index}</strong>
              <span>Time: {new Date(entry.time || Date.now()).toLocaleString()}</span>
            </div>
            <button class="mac-btn compact" on:click={() => handleRollback(entry.index)}>
              <i class="bi bi-arrow-counterclockwise"></i> Rollback
            </button>
          </div>
        {:else}
          <p class="empty-text">No history snapshots for this path.</p>
        {/each}
      </div>

      <div class="form-actions">
        <button class="mac-btn" on:click={() => showHistoryModal = false}>Close</button>
      </div>
    </div>
  </div>
{/if}

<!-- TOML edit modal -->
{#if showRawTomlEditor}
  <div class="mac-overlay">
    <div class="mac-modal toml-modal">
      <h2>Edit Raw MCP config.toml</h2>
      <p class="field-hint">{$t('codex.mcp.rawWarn')}</p>

      <textarea class="mac-input toml-textarea" bind:value={rawTomlContent}></textarea>

      <div class="form-actions">
        <button class="mac-btn primary" on:click={handleSaveRawToml}>Save TOML</button>
        <button class="mac-btn" on:click={() => showRawTomlEditor = false}>Cancel</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .codex-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
    height: 100%;
  }

  .codex-layout-wrapper {
    display: flex;
    gap: 20px;
    height: 100%;
  }

  /* Sub-sidebar inside codex page */
  .codex-sub-sidebar {
    width: 140px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    border-right: 1px solid var(--mac-border-separator);
    padding-right: 12px;
  }

  .codex-sub-sidebar button {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: transparent;
    border: none;
    border-radius: var(--radius-button);
    font-family: var(--font-sans);
    font-size: 13px;
    font-weight: 500;
    text-align: left;
    cursor: pointer;
    color: var(--mac-text-primary);
    transition: all var(--transition-fast);
  }

  .codex-sub-sidebar button:hover {
    background-color: rgba(0, 0, 0, 0.03);
  }
  @media (prefers-color-scheme: dark) {
    .codex-sub-sidebar button:hover {
      background-color: rgba(255, 255, 255, 0.03);
    }
  }

  .codex-sub-sidebar button.active {
    background-color: var(--mac-accent-soft);
    color: var(--mac-accent);
    font-weight: 600;
  }

  /* Content area */
  .codex-content-workspace {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding-bottom: 32px;
  }

  /* Markdown Editor elements */
  .markdown-editor-pane {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .doc-paths-toolbar {
    display: flex;
  }

  .path-picker {
    flex: 1;
  }

  .doc-warning-banner {
    background-color: var(--mac-warning);
    color: #ffffff;
    font-size: 12px;
    font-weight: 600;
    padding: 8px 12px;
    border-radius: var(--radius-card);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .editor-content-area {
    height: 380px;
    padding: 0;
    overflow: hidden;
  }

  .textarea-editor {
    width: 100%;
    height: 100%;
    border: none;
    resize: none;
    background-color: transparent;
    font-family: var(--font-mono);
    font-size: 12px;
    padding: 12px;
    box-shadow: none;
  }

  .markdown-preview-pane {
    padding: 12px;
    font-family: var(--font-sans);
    font-size: 13px;
    line-height: 1.6;
    overflow-y: auto;
    height: 100%;
    white-space: pre-wrap;
    user-select: text;
  }

  .editor-actions {
    display: flex;
    gap: 8px;
  }

  /* MCP sub nav tabs */
  .mcp-sub-nav {
    display: flex;
    border-bottom: 1px solid var(--mac-border-separator);
    margin-bottom: 16px;
    gap: 16px;
  }

  .mcp-sub-nav button {
    background: transparent;
    border: none;
    font-size: 13px;
    font-weight: 600;
    padding: 8px 0;
    cursor: pointer;
    color: var(--mac-text-secondary);
    border-bottom: 2px solid transparent;
    transition: all var(--transition-fast);
  }

  .mcp-sub-nav button.active {
    color: var(--mac-accent);
    border-bottom-color: var(--mac-accent);
  }

  /* MCP layouts */
  .mcp-servers-layout {
    display: flex;
    gap: 16px;
    align-items: flex-start;
  }

  .mcp-sidebar-list {
    width: 200px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 70vh;
    overflow-y: auto;
  }

  .list-item {
    padding: 8px 12px;
    background-color: var(--mac-bg-card);
    border: var(--mac-border-highlight);
    border-radius: var(--radius-card);
    text-align: left;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    min-width: 0;
    transition: all var(--transition-fast);
    color: var(--mac-text-primary);
  }

  .list-item:hover {
    background-color: rgba(0, 0, 0, 0.03);
  }

  .list-item.active {
    border-color: var(--mac-accent);
    background-color: var(--mac-accent-soft);
  }

  .list-item strong {
    font-size: 12px;
    font-weight: 600;
  }

  .mcp-detail-form {
    flex: 1;
    padding: 16px;
  }

  .mcp-detail-form h3 {
    font-size: 14px;
    margin-bottom: 16px;
    font-family: var(--font-mono);
  }

  .arg-item, .env-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    border-radius: 4px;
    padding: 4px 8px;
    font-size: 11px;
    margin-bottom: 4px;
  }

  .env-input-row {
    display: flex;
    gap: 6px;
  }

  .env-key-input { flex: 1; }
  .env-val-input { flex: 2; }

  /* MCP Plugins list */
  .plugins-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .plugin-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    margin-bottom: 0;
  }

  .plugin-row .meta {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    padding-right: 16px;
  }

  .plugin-row .actions {
    display: flex;
    align-items: center;
    gap: 16px;
  }

  /* Skills views */
  .skills-top-bar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
  }

  .skills-layout-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
  }

  .skills-list-panel, .skills-backup-panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: 220px;
    overflow-y: auto;
  }

  .skills-backup-panel h3 {
    font-size: 12px;
    text-transform: uppercase;
    color: var(--mac-text-secondary);
    border-bottom: 1px solid var(--mac-border-separator);
    padding-bottom: 4px;
    margin-bottom: 8px;
  }

  .skill-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 8px 12px;
    border-bottom: 1px solid var(--mac-border-separator);
    font-size: 12px;
  }

  .badge {
    font-size: 9px;
    padding: 1px 4px;
    border-radius: 4px;
  }

  .badge.ok { background-color: var(--mac-success-soft); color: var(--mac-success); }
  .badge.err { background-color: var(--mac-danger-soft); color: var(--mac-danger); }

  .skill-md-editor {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  /* Modals overrides */
  .history-snapshots-list {
    max-height: 200px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .toml-textarea {
    width: 100%;
    height: 250px;
    font-family: var(--font-mono);
    font-size: 11px;
    resize: vertical;
  }

  .empty-text {
    font-size: 12px;
    color: var(--mac-text-secondary);
    text-align: center;
    padding: 16px;
  }

  .w-100 { width: 100%; }
  .mt-2 { margin-top: 8px; }

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
