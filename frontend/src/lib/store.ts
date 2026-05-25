import { writable } from 'svelte/store';
import type { AppStatus, Provider, ProxyStatus, ProxyLog } from './api';
import { CCApi } from './api';

// Active Tab
export const activeTab = writable<string>('dashboard');

// Global App Status
export const appStatus = writable<AppStatus>({
  desktopConfigured: false,
  proxyRunning: false,
  proxyPort: 18080,
  activeProvider: { name: '-', id: null },
  activeProviderId: null,
  desktopHealth: { needsApply: false, issues: [] },
  exposeAllProviderModels: false,
});

// Providers List
export const providers = writable<Provider[]>([]);

// Settings
export const settings = writable<any>({});

// Proxy Status & Logs
export const proxyStatus = writable<ProxyStatus>({
  running: false,
  port: 18080,
  stats: { total: 0, success: 0, failed: 0, today: 0 }
});
export const proxyLogs = writable<ProxyLog[]>([]);

// OAuth Statuses
export const geminiOauthStatus = writable<any>(null);
export const antigravityOauthStatus = writable<any>(null);

// Functions to refresh state
export async function refreshAll() {
  try {
    const [statusData, providersData, settingsData] = await Promise.all([
      CCApi.getStatus(),
      CCApi.getProviders(),
      CCApi.getSettings(),
    ]);

    appStatus.set(statusData);
    providers.set(providersData);
    settings.set(settingsData);
    
    // Also fetch proxy details if running
    if (statusData.proxyRunning) {
      const proxyState = await CCApi.getProxyStatus();
      proxyStatus.set(proxyState);
    }
  } catch (err) {
    console.error('Failed to refresh global state:', err);
  }
}

export async function refreshLogs() {
  try {
    const logs = await CCApi.getProxyLogs();
    proxyLogs.set(logs);
  } catch (err) {
    console.error('Failed to fetch proxy logs:', err);
  }
}
