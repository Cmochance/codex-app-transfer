<script lang="ts">
  import { onMount } from 'svelte';
  import { CCApi } from '../lib/api';
  import { appStatus, settings, refreshAll } from '../lib/store';
  import { t, locale, setLocale } from '../lib/i18n';

  let themeVal = 'default';
  let langVal = 'zh';
  let proxyPort = 18080;
  let adminPort = 18081;
  let autoApplyOnStart = true;
  let autoUnlockCodexPlugins = true;
  let autoWakeCodexPet = false;
  let restoreCodexOnExit = true;
  let codexNetworkAccess = true;
  let updateUrl = '';

  let appVersion = '...';
  let updateStatusText = '';
  let updateAvailable = false;
  let installBtnVisible = false;

  let backups: any[] = [];
  let compatList: any[] = [];
  let compatChecking = false;

  // Feedback form fields
  let feedbackTitle = '';
  let feedbackEmail = '';
  let feedbackBody = '';
  let feedbackFiles: { name: string; base64: string; size: number }[] = [];
  let feedbackSubmitting = false;

  // Load initial settings
  async function loadSettings() {
    try {
      const data = await CCApi.getSettings();
      settings.set(data);
      themeVal = data.theme || 'default';
      langVal = data.language || 'zh';
      proxyPort = data.proxyPort || 18080;
      adminPort = data.adminPort || 18081;
      autoApplyOnStart = data.autoApplyOnStart !== false;
      autoUnlockCodexPlugins = data.autoUnlockCodexPlugins !== false;
      autoWakeCodexPet = !!data.autoWakeCodexPet;
      restoreCodexOnExit = data.restoreCodexOnExit !== false;
      codexNetworkAccess = data.codexNetworkAccess !== false;
      updateUrl = data.updateUrl || '';

      const ver = await CCApi.getVersion();
      appVersion = ver.version || '...';
      
      await loadBackupsList();
    } catch (err) {
      console.error(err);
    }
  }

  async function loadBackupsList() {
    try {
      backups = await CCApi.listBackups();
    } catch (_) {}
  }

  // Settings Save logic
  async function updateSettingField(key: string, val: any) {
    try {
      const payload: Record<string, any> = {};
      payload[key] = val;
      const updated = await CCApi.saveSettings(payload);
      settings.set(updated);
    } catch (err: any) {
      showToast(`Save failed: ${err.message}`);
    }
  }

  async function handleThemeChange(palette: string) {
    themeVal = palette;
    await updateSettingField('theme', palette);
    document.documentElement.setAttribute('data-theme-palette', palette);
  }

  async function handleLanguageChange(lang: string) {
    langVal = lang;
    setLocale(lang);
    await updateSettingField('language', lang);
  }

  // Backup & Import
  async function handleCreateBackup() {
    try {
      await CCApi.createBackup();
      await loadBackupsList();
      showToast($t('toast.configBackedUp'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  async function handleExportConfig() {
    try {
      const config = await CCApi.exportConfig();
      const blob = new Blob([JSON.stringify(config, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `codex-transfer-config-${Date.now()}.json`;
      a.click();
      showToast($t('toast.configExported'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    }
  }

  let fileInput: HTMLInputElement;
  function triggerImport() {
    fileInput.click();
  }

  async function handleImportConfig(e: Event) {
    const target = e.target as HTMLInputElement;
    const file = target.files?.[0];
    if (!file) return;

    if (!confirm($t('confirm.configImport'))) return;

    const reader = new FileReader();
    reader.onload = async (evt) => {
      try {
        const text = evt.target?.result as string;
        const parsed = JSON.parse(text);
        await CCApi.importConfig(parsed);
        showToast($t('toast.configImported'));
        await loadSettings();
        await refreshAll();
      } catch (err: any) {
        showToast($t('toast.configImportFailed') + `: ${err.message}`);
      }
    };
    reader.readAsText(file);
  }

  // Compatibility checking
  async function handleCheckCompat() {
    compatChecking = true;
    compatList = [];
    try {
      const res = await CCApi.getProviderCompatibility();
      compatList = Object.entries(res || {}).map(([name, status]: any) => ({
        name,
        ok: status.ok,
        message: status.message || (status.ok ? 'Available' : 'Error')
      }));
      showToast($t('toast.compatibilityChecked'));
    } catch (err: any) {
      showToast(err.message || $t('toast.requestFailed'));
    } finally {
      compatChecking = false;
    }
  }

  // Updates checking
  async function handleCheckUpdate() {
    updateStatusText = 'Checking...';
    try {
      const res = await CCApi.checkUpdate(updateUrl);
      if (res.available) {
        updateAvailable = true;
        updateStatusText = $t('toast.updateAvailable') + ` (v${res.version})`;
        installBtnVisible = true;
      } else {
        updateAvailable = false;
        updateStatusText = $t('toast.noUpdate');
        installBtnVisible = false;
      }
    } catch (err: any) {
      updateStatusText = `Check failed: ${err.message}`;
    }
  }

  async function handleInstallUpdate() {
    if (!confirm($t('confirm.installUpdate'))) return;
    try {
      updateStatusText = $t('toast.updateDownloading');
      await CCApi.installUpdate(updateUrl);
      showToast($t('toast.updateInstallerStarted'));
    } catch (err: any) {
      updateStatusText = `Download failed: ${err.message}`;
    }
  }

  // Feedback file upload helper
  function handleFileSelect(e: Event) {
    const target = e.target as HTMLInputElement;
    const files = target.files;
    if (!files) return;

    for (let i = 0; i < files.length; i++) {
      const file = files[i];
      if (file.size > 5 * 1024 * 1024) {
        showToast($t('feedback.tooLargeFile', { name: file.name }));
        continue;
      }

      const reader = new FileReader();
      reader.onload = (evt) => {
        const result = evt.target?.result as string;
        const base64 = result.split(',')[1];
        feedbackFiles = [...feedbackFiles, {
          name: file.name,
          base64,
          size: file.size
        }];
      };
      reader.readAsDataURL(file);
    }
  }

  async function handleFeedbackSubmit() {
    if (!feedbackBody.trim()) return alert($t('feedback.bodyRequired'));
    feedbackSubmitting = true;
    try {
      const payload = {
        title: feedbackTitle.trim() || undefined,
        contactEmail: feedbackEmail.trim() || undefined,
        body: feedbackBody.trim(),
        attachments: feedbackFiles.map(f => ({ name: f.name, base64: f.base64 })),
        includeDiagnostics: true
      };

      const res = await CCApi.submitFeedback(payload);
      showToast($t('feedback.successToast', { id: res.id || 'OK' }));
      
      // Clear fields
      feedbackTitle = '';
      feedbackEmail = '';
      feedbackBody = '';
      feedbackFiles = [];
    } catch (err: any) {
      showToast($t('feedback.failToast', { message: err.message || 'Error' }));
    } finally {
      feedbackSubmitting = false;
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

  async function handleRestartCodex() {
    try {
      await CCApi.restartCodexApp();
      showToast($t('toast.codexAppRestartRequested'));
    } catch (err: any) {
      showToast(err.message || $t('toast.codexAppRestartFailed'));
    }
  }

  onMount(() => {
    loadSettings();
  });
</script>

{#if showToastBanner}
  <div class="toast-banner">
    {toastMsg}
  </div>
{/if}

<div class="settings-page">
  <div class="page-title">
    <h1>{$t('settings.title')}</h1>
    <p class="subtitle-text">{$t('settings.subtitle')}</p>
  </div>

  <div class="settings-content-wrapper">
    <!-- Left Column: Settings Options -->
    <div class="settings-main-column">
      <!-- Section 1: Appearance & Locale -->
      <section class="mac-card settings-section">
        <h2>Appearance & Language</h2>
        
        <!-- Theme palette -->
        <div class="setting-row">
          <div class="label-box">
            <strong>{$t('settings.theme')}</strong>
          </div>
          <div class="theme-picker">
            {#each ['default', 'green', 'orange', 'gray', 'dark', 'white'] as color}
              <button 
                class="theme-dot {color}" 
                class:active={themeVal === color}
                on:click={() => handleThemeChange(color)}
                title={color}
              ></button>
            {/each}
          </div>
        </div>

        <!-- Language segmented button -->
        <div class="setting-row">
          <div class="label-box">
            <strong>{$t('settings.language')}</strong>
          </div>
          <div class="segmented-control">
            <button class:active={langVal === 'zh'} on:click={() => handleLanguageChange('zh')}>中文</button>
            <button class:active={langVal === 'en'} on:click={() => handleLanguageChange('en')}>English</button>
          </div>
        </div>
      </section>

      <!-- Section 2: Port Configurations -->
      <section class="mac-card settings-section">
        <h2>Network Configuration</h2>

        <div class="setting-row">
          <div class="label-box">
            <strong>{$t('settings.proxyPort')}</strong>
          </div>
          <input 
            class="mac-input settings-num-input" 
            type="number" 
            bind:value={proxyPort} 
            on:blur={() => updateSettingField('proxyPort', proxyPort)} 
          />
        </div>

        <div class="setting-row">
          <div class="label-box">
            <strong>{$t('settings.adminPort')}</strong>
          </div>
          <input 
            class="mac-input settings-num-input" 
            type="number" 
            bind:value={adminPort} 
            on:blur={() => updateSettingField('adminPort', adminPort)} 
          />
        </div>
      </section>

      <!-- Section 3: Automation options -->
      <section class="mac-card settings-section">
        <h2>Automation Settings</h2>

        <!-- Auto Apply -->
        <div class="setting-row-stack">
          <div class="row-header">
            <strong>{$t('settings.autoApplyOnStart')}</strong>
            <label class="mac-switch">
              <input 
                type="checkbox" 
                bind:checked={autoApplyOnStart} 
                on:change={() => updateSettingField('autoApplyOnStart', autoApplyOnStart)} 
              />
              <span class="mac-switch-slider"></span>
            </label>
          </div>
          <p class="field-hint">{$t('settings.autoApplyOnStartHint')}</p>
        </div>

        <!-- Plugins Auto Unlock -->
        <div class="setting-row-stack">
          <div class="row-header">
            <strong>{$t('settings.autoUnlockCodexPlugins')}</strong>
            <div class="row-actions">
              <button class="mac-btn compact" on:click={handleRestartCodex}>
                {$t('settings.autoUnlockRestartCodex')}
              </button>
              <label class="mac-switch">
                <input 
                  type="checkbox" 
                  bind:checked={autoUnlockCodexPlugins} 
                  on:change={() => updateSettingField('autoUnlockCodexPlugins', autoUnlockCodexPlugins)} 
                />
                <span class="mac-switch-slider"></span>
              </label>
            </div>
          </div>
          <p class="field-hint">{$t('settings.autoUnlockCodexPluginsHint')}</p>
        </div>

        <!-- Codex Pet Auto Wake -->
        <div class="setting-row-stack">
          <div class="row-header">
            <strong>{$t('settings.autoWakeCodexPet')}</strong>
            <label class="mac-switch">
              <input 
                type="checkbox" 
                bind:checked={autoWakeCodexPet} 
                on:change={() => updateSettingField('autoWakeCodexPet', autoWakeCodexPet)} 
              />
              <span class="mac-switch-slider"></span>
            </label>
          </div>
          <p class="field-hint">{$t('settings.autoWakeCodexPetHint')}</p>
        </div>

        <!-- Restore Config on Exit -->
        <div class="setting-row-stack">
          <div class="row-header">
            <strong>{$t('settings.restoreCodexOnExit')}</strong>
            <label class="mac-switch">
              <input 
                type="checkbox" 
                bind:checked={restoreCodexOnExit} 
                on:change={() => updateSettingField('restoreCodexOnExit', restoreCodexOnExit)} 
              />
              <span class="mac-switch-slider"></span>
            </label>
          </div>
          <p class="field-hint">{$t('settings.restoreCodexOnExitHint')}</p>
        </div>

        <!-- Full Network Permission Sandboxing -->
        <div class="setting-row-stack">
          <div class="row-header">
            <strong>{$t('settings.codexNetworkAccess')}</strong>
            <label class="mac-switch">
              <input 
                type="checkbox" 
                bind:checked={codexNetworkAccess} 
                on:change={() => updateSettingField('codexNetworkAccess', codexNetworkAccess)} 
              />
              <span class="mac-switch-slider"></span>
            </label>
          </div>
          <p class="field-hint">{$t('settings.codexNetworkAccessHint')}</p>
        </div>
      </section>

      <!-- Section 4: Configuration Backups -->
      <section class="mac-card settings-section">
        <div class="section-title-row">
          <h2>{$t('settings.configBackup')}</h2>
          <div class="actions">
            <button class="mac-btn primary" on:click={handleCreateBackup}>{$t('settings.backupNow')}</button>
            <button class="mac-btn" on:click={handleExportConfig}>{$t('settings.exportConfig')}</button>
            <button class="mac-btn" on:click={triggerImport}>{$t('settings.importConfig')}</button>
            
            <input 
              type="file" 
              bind:this={fileInput} 
              on:change={handleImportConfig} 
              accept=".json" 
              style="display:none;" 
            />
          </div>
        </div>
        <p class="field-hint">{$t('settings.configBackupHint')}</p>

        <!-- Backups list -->
        <div class="backups-list">
          {#each backups as backup}
            <div class="backup-row">
              <span class="backup-name"><i class="bi bi-shield-lock-fill"></i> {backup.filename || backup.time || 'Snapshot'}</span>
              <button class="mac-btn compact" on:click={() => {}}>Restore</button>
            </div>
          {:else}
            <p class="empty-text">{$t('settings.noBackups')}</p>
          {/each}
        </div>
      </section>

      <!-- Section 5: API Compatibility Testing -->
      <section class="mac-card settings-section">
        <div class="section-title-row">
          <h2>{$t('settings.thirdPartyCompat')}</h2>
          <button class="mac-btn" on:click={handleCheckCompat} disabled={compatChecking}>
            <i class="bi bi-patch-check"></i>
            <span>{compatChecking ? 'Checking...' : $t('settings.checkCompatibility')}</span>
          </button>
        </div>
        <p class="field-hint">{$t('settings.thirdPartyCompatHint')}</p>

        <!-- Compatibility results -->
        {#if compatList.length > 0}
          <div class="compat-table">
            {#each compatList as item}
              <div class="compat-row">
                <strong class="provider-name">{item.name}</strong>
                <span class="status-badge" class:ok={item.ok} class:err={!item.ok}>
                  {item.ok ? '✓ OK' : '✗ Failed'}
                </span>
                <span class="message truncate">{item.message}</span>
              </div>
            {/each}
          </div>
        {/if}
      </section>
    </div>

    <!-- Right Column: Feedback Form & About -->
    <div class="settings-sidebar-column">
      <!-- Section 6: About Codex App Transfer -->
      <section class="mac-card settings-section about-panel">
        <h2>{$t('settings.about')}</h2>
        
        <div class="about-field">
          <span>{$t('settings.version')}</span>
          <strong>{appVersion}</strong>
        </div>

        <div class="about-field">
          <span>{$t('settings.license')}</span>
          <strong>MIT License</strong>
        </div>

        <div class="about-field">
          <span>GitHub</span>
          <a href="https://github.com/Cmochance/codex-app-transfer" target="_blank" rel="noreferrer">
            <i class="bi bi-box-arrow-up-right"></i>
          </a>
        </div>

        <!-- Update checking -->
        <div class="update-section">
          <div class="row-header">
            <strong>Check for Updates</strong>
            <button class="mac-btn compact" on:click={handleCheckUpdate}>
              {$t('settings.checkUpdate')}
            </button>
          </div>
          <p class="field-hint">{updateStatusText}</p>
          {#if installBtnVisible}
            <button class="mac-btn primary w-100 mt-2" on:click={handleInstallUpdate}>
              {$t('settings.installUpdate')}
            </button>
          {/if}
        </div>
      </section>

      <!-- Section 7: Feedback Submission Form -->
      <section class="mac-card settings-section feedback-form">
        <h2>{$t('feedback.title')}</h2>
        <p class="field-hint">{$t('feedback.intro')}</p>

        <!-- Form fields -->
        <div class="form-group">
          <label class="form-label" for="feedTitle">{$t('feedback.titleLabel')}</label>
          <input class="mac-input" id="feedTitle" bind:value={feedbackTitle} placeholder={$t('feedback.titlePlaceholder')} />
        </div>

        <div class="form-group">
          <label class="form-label" for="feedEmail">{$t('feedback.contactEmailLabel')}</label>
          <input class="mac-input" id="feedEmail" type="email" bind:value={feedbackEmail} placeholder={$t('feedback.contactEmailPlaceholder')} />
          <p class="field-hint">{$t('feedback.contactEmailHint')}</p>
        </div>

        <div class="form-group">
          <label class="form-label required" for="feedBody">{$t('feedback.bodyLabel')}</label>
          <textarea class="mac-input text-area" id="feedBody" bind:value={feedbackBody} rows="4" placeholder={$t('feedback.bodyPlaceholder')}></textarea>
        </div>

        <!-- Attachment drop zone -->
        <div class="form-group attachment-dropzone">
          <label class="form-label" for="feedFiles">{$t('feedback.attachmentsLabel')}</label>
          <input type="file" id="feedFiles" multiple on:change={handleFileSelect} style="display:none;" />
          
          <button class="mac-btn w-100" type="button" on:click={() => document.getElementById('feedFiles')?.click()}>
            <i class="bi bi-cloud-upload"></i>
            <span>{$t('feedback.attachmentsHint')}</span>
          </button>

          {#if feedbackFiles.length > 0}
            <div class="files-preview">
              {#each feedbackFiles as file, index}
                <div class="file-item">
                  <span class="truncate">{file.name} ({(file.size / 1024).toFixed(1)} KB)</span>
                  <button type="button" class="remove-btn" on:click={() => feedbackFiles = feedbackFiles.filter((_, i) => i !== index)}>
                    <i class="bi bi-x"></i>
                  </button>
                </div>
              {/each}
            </div>
          {/if}
        </div>

        <p class="field-hint tos-warning">{$t('feedback.privacyWarning')}</p>

        <button 
          class="mac-btn primary w-100 mt-2" 
          on:click={handleFeedbackSubmit} 
          disabled={feedbackSubmitting}
        >
          <i class="bi bi-chat-left-dots"></i>
          <span>{feedbackSubmitting ? $t('feedback.submitting') : $t('feedback.submit')}</span>
        </button>
      </section>
    </div>
  </div>
</div>

<style>
  .settings-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .settings-content-wrapper {
    display: flex;
    gap: 20px;
    align-items: flex-start;
  }

  .settings-main-column {
    flex: 2;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .settings-sidebar-column {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-height: 85vh;
    overflow-y: auto;
  }

  .settings-section {
    padding: 16px 20px;
  }

  .settings-section h2 {
    font-size: 13px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--mac-text-secondary);
    margin-bottom: 12px;
    border-bottom: 1px solid var(--mac-border-separator);
    padding-bottom: 8px;
  }

  .setting-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 0;
    border-bottom: 1px solid var(--mac-border-separator);
  }

  .setting-row:last-of-type {
    border-bottom: none;
  }

  .label-box {
    display: flex;
    flex-direction: column;
  }

  .label-box strong {
    font-size: 13px;
    font-weight: 600;
  }

  /* Theme Pickers */
  .theme-picker {
    display: flex;
    gap: 6px;
  }

  .theme-dot {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    border: 1px solid var(--mac-border-separator);
    cursor: pointer;
    position: relative;
    outline: none;
  }

  .theme-dot.active::after {
    content: '';
    position: absolute;
    width: 6px;
    height: 6px;
    background-color: #ffffff;
    border-radius: 50%;
    top: 4px;
    left: 4px;
  }

  .theme-dot.default { background-color: #007aff; }
  .theme-dot.green { background-color: #34c759; }
  .theme-dot.orange { background-color: #ff9500; }
  .theme-dot.gray { background-color: #8e8e93; }
  .theme-dot.dark { background-color: #1c1c1e; }
  .theme-dot.white { background-color: #f5f5f7; }

  /* Segmented language button */
  .segmented-control {
    display: flex;
    background-color: rgba(120, 120, 128, 0.08);
    border-radius: 6px;
    padding: 2px;
  }

  .segmented-control button {
    background: transparent;
    border: none;
    font-size: 12px;
    font-weight: 500;
    padding: 4px 12px;
    border-radius: 4px;
    cursor: pointer;
    color: var(--mac-text-primary);
    transition: all var(--transition-fast);
  }

  .segmented-control button.active {
    background-color: #ffffff;
    box-shadow: 0 1px 3px rgba(0,0,0,0.12);
  }
  @media (prefers-color-scheme: dark) {
    .segmented-control button.active {
      background-color: rgba(255, 255, 255, 0.15);
    }
  }

  .settings-num-input {
    width: 90px;
    text-align: center;
    padding: 4px;
  }

  /* Automation toggles */
  .setting-row-stack {
    display: flex;
    flex-direction: column;
    padding: 12px 0;
    border-bottom: 1px solid var(--mac-border-separator);
  }

  .setting-row-stack:last-of-type {
    border-bottom: none;
  }

  .row-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 13px;
    font-weight: 600;
    margin-bottom: 4px;
  }

  .row-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .compact {
    font-size: 10px;
    padding: 3px 6px;
    height: auto;
  }

  /* Backups and Compatibility */
  .backups-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 10px;
  }

  .backup-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 8px 12px;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    border-radius: var(--radius-card);
  }

  .backup-name {
    font-size: 12px;
    color: var(--mac-text-primary);
  }

  .empty-text {
    font-size: 12px;
    color: var(--mac-text-secondary);
    text-align: center;
    padding: 12px;
  }

  .compat-table {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 12px;
  }

  .compat-row {
    display: flex;
    align-items: center;
    padding: 8px 12px;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    border-radius: var(--radius-card);
    font-size: 12px;
  }

  .compat-row .provider-name {
    width: 140px;
    font-weight: 600;
  }

  .status-badge {
    font-size: 10px;
    font-weight: 700;
    padding: 2px 6px;
    border-radius: 4px;
    margin-right: 12px;
  }

  .status-badge.ok { background-color: var(--mac-success-soft); color: var(--mac-success); }
  .status-badge.err { background-color: var(--mac-danger-soft); color: var(--mac-danger); }

  .compat-row .message {
    flex: 1;
    color: var(--mac-text-secondary);
  }

  /* About and feedback forms */
  .about-field {
    display: flex;
    justify-content: space-between;
    padding: 6px 0;
    font-size: 12px;
    border-bottom: 1px dashed var(--mac-border-separator);
  }

  .about-field:last-of-type {
    border-bottom: none;
  }

  .update-section {
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid var(--mac-border-separator);
  }

  .text-area {
    width: 100%;
    resize: vertical;
    min-height: 80px;
  }

  .files-preview {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 8px;
  }

  .file-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    background-color: var(--mac-bg-panel);
    border: 1px solid var(--mac-border-separator);
    border-radius: 4px;
    padding: 4px 8px;
    font-size: 11px;
  }

  .remove-btn {
    background: transparent;
    border: none;
    cursor: pointer;
    color: var(--mac-text-danger);
    font-size: 14px;
    display: flex;
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
