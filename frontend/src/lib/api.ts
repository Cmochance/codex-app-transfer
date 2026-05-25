const BASE = '';

async function api(method: string, path: string, body?: any): Promise<any> {
  const opts: RequestInit = { method, headers: { 'X-CAS-Request': '1' } };
  if (body !== undefined) {
    (opts.headers as Record<string, string>)['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const resp = await fetch(BASE + path, opts);
  const data = await resp.json();
  if (!resp.ok || data.success === false) {
    const baseMessage = data.message || `Request failed: ${method} ${path}`;
    const error = new Error(baseMessage) as any;
    error.errors = Array.isArray(data.errors) ? data.errors : [];
    error.responseData = data;
    throw error;
  }
  return data;
}

// ── Types ──
export interface Provider {
  id?: string;
  name: string;
  baseUrl: string;
  apiFormat: string;
  authScheme: string;
  hasApiKey?: boolean;
  apiKey?: string;
  extraHeaders?: Record<string, string>;
  modelCapabilities?: Record<string, any>;
  requestOptions?: Record<string, any>;
  default?: boolean;
  isBuiltin?: boolean;
  icon?: string;
  logo?: string;
  mappings?: Record<string, string>;
  models?: Record<string, string>;
  supportsWebSearch?: boolean;
  grokWeb?: {
    sso?: string;
    cookieString?: string;
    cfClearance?: string;
    ssoRw?: string;
    statsigId?: string;
    userAgent?: string;
  };
}

export interface AppStatus {
  desktopConfigured: boolean;
  proxyRunning: boolean;
  proxyPort: number;
  activeProvider: { name: string; id: string | null };
  activeProviderId: string | null;
  desktopHealth: { needsApply: boolean; issues: string[] };
  exposeAllProviderModels: boolean;
}

export interface Preset {
  id: string;
  name: string;
  baseUrl: string;
  apiFormat: string;
  authScheme: string;
  models: Record<string, string>;
  modelOptions: Record<string, any>;
  baseUrlOptions: string[];
  baseUrlHint: string;
  requestOptionPresets: Record<string, any>;
  extraHeaders: Record<string, string>;
  modelCapabilities: Record<string, any>;
  requestOptions: Record<string, any>;
  supportsWebSearch: boolean;
  icon?: string;
  logo?: string;
}

export interface DesktopStatus {
  configured: boolean;
  health: { needsApply: boolean; issues: string[] };
  config: {
    inferenceProvider: string;
    inferenceGatewayBaseUrl: string;
    inferenceGatewayApiKey: string;
    inferenceGatewayAuthScheme: string;
    inferenceModels: string;
  };
}

export interface ProxyStatus {
  running: boolean;
  port: number;
  stats: { total: number; success: number; failed: number; today: number };
}

export interface ProxyLog {
  at: string;
  level: string;
  message: string;
}

export interface Activity {
  time: string;
  text: string;
}

// ── Icon Mapping ──
const ICON_MAP: Record<string, { logo?: string; icon?: string }> = {
  deepseek: { logo: 'assets/providers/deepseek.ico' },
  kimi: { logo: 'assets/providers/kimi.ico' },
  moonshot: { logo: 'assets/providers/kimi.ico' },
  xiaomi: { logo: 'assets/providers/xiaomi-mimo.png' },
  mimo: { logo: 'assets/providers/xiaomi-mimo.png' },
  qiniu: { logo: 'assets/providers/qiniu.ico' },
  qnaigc: { logo: 'assets/providers/qiniu.ico' },
  zhipu: { logo: 'assets/providers/zhipu.png' },
  bigmodel: { logo: 'assets/providers/zhipu.png' },
  glm: { logo: 'assets/providers/zhipu.png' },
  siliconflow: { icon: 'bi-diagram-3-fill' },
  bailian: { logo: 'assets/providers/aliyun.ico' },
  dashscope: { logo: 'assets/providers/aliyun.ico' },
  aliyun: { logo: 'assets/providers/aliyun.ico' },
  minimax: { logo: 'assets/providers/minimax.ico' },
  minimaxi: { logo: 'assets/providers/minimax.ico' },
  'gemini-cli': { logo: 'assets/providers/gemini.svg' },
  'antigravity-oauth': { logo: 'assets/providers/antigravity.png' },
  google: { logo: 'assets/providers/google-ai-studio.png' },
  gemini: { logo: 'assets/providers/google-ai-studio.png' },
  aistudio: { logo: 'assets/providers/google-ai-studio.png' },
  generativelanguage: { logo: 'assets/providers/google-ai-studio.png' },
  'grok-web': { logo: 'assets/providers/grok.svg' },
  anyrouter: { logo: 'assets/providers/anyrouter.png' },
};

function buildCustomThirdPartyPreset(): Preset {
  return {
    id: 'custom-third-party',
    name: '自定义第三方',
    baseUrl: '',
    apiFormat: 'OpenAI',
    authScheme: 'bearer',
    models: {},
    modelOptions: {},
    baseUrlOptions: [],
    baseUrlHint: '',
    requestOptionPresets: {},
    extraHeaders: {},
    modelCapabilities: {},
    requestOptions: {},
    icon: 'bi-puzzle',
    supportsWebSearch: true,
  };
}

function computeIcon(provider: { id?: string; name: string; baseUrl: string; apiFormat: string }) {
  const raw = `${provider.id || ''} ${provider.name || ''} ${provider.baseUrl || ''} ${provider.apiFormat || ''}`.toLowerCase();
  const lookup = raw.replace(/[_\s]+/g, '-');
  for (const [key, val] of Object.entries(ICON_MAP)) {
    if (lookup.includes(key)) return val;
  }
  return { icon: 'bi-plug-fill' };
}

function mapProvider(provider: any, activeId: string | null): Provider {
  const models = provider.models || {};
  return {
    id: provider.id,
    name: provider.name,
    baseUrl: provider.baseUrl,
    apiFormat: ['openai', 'openai_chat'].includes(provider.apiFormat) ? 'openai_chat' : (provider.apiFormat || 'openai_chat'),
    authScheme: provider.authScheme || 'bearer',
    hasApiKey: !!provider.hasApiKey,
    extraHeaders: provider.extraHeaders || {},
    modelCapabilities: provider.modelCapabilities || {},
    requestOptions: provider.requestOptions || {},
    default: provider.id === activeId,
    isBuiltin: !!provider.isBuiltin,
    mappings: {
      default: models.default || '',
      gpt_5_5: models.gpt_5_5 || '',
      gpt_5_4: models.gpt_5_4 || '',
      gpt_5_4_mini: models.gpt_5_4_mini || '',
      gpt_5_3_codex: models.gpt_5_3_codex || '',
      gpt_5_2: models.gpt_5_2 || '',
    },
    ...computeIcon(provider),
  };
}

function providerBody(payload: Provider, includeModels = true) {
  const body: any = {
    name: payload.name,
    baseUrl: payload.baseUrl,
    authScheme: payload.authScheme || 'bearer',
    apiFormat: (() => {
      const v = (payload.apiFormat || '').toLowerCase().replace(/-/g, '_');
      if (['responses', 'openai_responses'].includes(v)) return 'responses';
      if (['anthropic_messages', 'anthropic', 'claude', 'messages', 'claude_messages'].includes(v)) return 'anthropic_messages';
      if (['gemini_native', 'google_ai_studio', 'gemini'].includes(v)) return 'gemini_native';
      if (['gemini_cli_oauth', 'gemini_oauth', 'google_oauth_cloud_code'].includes(v)) return 'gemini_cli_oauth';
      if (['antigravity_oauth', 'google_oauth_antigravity'].includes(v)) return 'antigravity_oauth';
      if (['grok_web', 'grok', 'grok_com'].includes(v)) return 'grok_web';
      return 'openai_chat';
    })(),
    extraHeaders: payload.extraHeaders || {},
    modelCapabilities: payload.modelCapabilities || {},
    requestOptions: payload.requestOptions || {},
  };
  if (payload.apiKey) {
    body.apiKey = payload.apiKey;
  }
  if (includeModels) {
    body.models = payload.models || {};
  }
  if (payload.grokWeb) {
    body.grokWeb = payload.grokWeb;
  }
  return body;
}

function mapLog(log: any): ProxyLog {
  return {
    at: log.time,
    level: log.level.toLowerCase(),
    message: log.message,
  };
}

// ── Public API Export ──
export const CCApi = {
  async getStatus(): Promise<AppStatus> {
    const data = await api('GET', '/api/status');
    const active = data.activeProvider;
    return {
      desktopConfigured: !!data.desktopConfigured,
      proxyRunning: !!data.proxyRunning,
      proxyPort: data.proxyPort || 18080,
      activeProvider: active ? { name: active.name, id: active.id } : { name: '-', id: null },
      activeProviderId: data.activeProviderId,
      desktopHealth: data.desktopHealth || { needsApply: false, issues: [] },
      exposeAllProviderModels: !!data.exposeAllProviderModels,
    };
  },

  async getProviders(): Promise<Provider[]> {
    const data = await api('GET', '/api/providers');
    return (data.providers || []).map((p: any) => mapProvider(p, data.activeId));
  },

  async getProviderSecret(id: string): Promise<any> {
    return api('GET', `/api/providers/${encodeURIComponent(id)}/secret`);
  },

  async getPresets(): Promise<Preset[]> {
    const data = await api('GET', '/api/presets');
    const builtin = (data.presets || []).map((p: any) => ({
      id: p.id,
      name: p.name,
      baseUrl: p.baseUrl,
      apiFormat: p.apiFormat || 'openai_chat',
      authScheme: p.authScheme || 'bearer',
      models: p.models || {},
      modelOptions: p.modelOptions || {},
      baseUrlOptions: p.baseUrlOptions || [],
      baseUrlHint: p.baseUrlHint || '',
      requestOptionPresets: p.requestOptionPresets || {},
      extraHeaders: p.extraHeaders || {},
      modelCapabilities: p.modelCapabilities || {},
      requestOptions: p.requestOptions || {},
      supportsWebSearch: !!p.supportsWebSearch,
      ...computeIcon(p),
    }));
    return [...builtin, buildCustomThirdPartyPreset()];
  },

  async addProvider(payload: Provider): Promise<Provider> {
    const data = await api('POST', '/api/providers', providerBody(payload));
    return data.provider || data;
  },

  async updateProvider(id: string, payload: Provider): Promise<Provider> {
    const data = await api('PUT', `/api/providers/${encodeURIComponent(id)}`, providerBody(payload));
    return data.provider || data;
  },

  async deleteProvider(id: string): Promise<any> {
    return api('DELETE', `/api/providers/${encodeURIComponent(id)}`);
  },

  async setDefaultProvider(id: string): Promise<any> {
    return api('PUT', `/api/providers/${encodeURIComponent(id)}/default`);
  },

  async saveDraft(id: string, payload: Provider): Promise<any> {
    return api('POST', `/api/providers/${encodeURIComponent(id)}/draft`, providerBody(payload, true));
  },

  async activateProvider(id: string): Promise<any> {
    return api('POST', `/api/providers/${encodeURIComponent(id)}/activate`);
  },

  async reorderProviders(providerIds: string[]): Promise<any> {
    return api('PUT', '/api/providers/reorder', { providerIds });
  },

  async testProvider(id: string): Promise<any> {
    return api('POST', `/api/providers/${encodeURIComponent(id)}/test`);
  },

  async queryProviderUsage(id: string): Promise<any> {
    return api('POST', `/api/providers/${encodeURIComponent(id)}/usage`);
  },

  async getProviderCompatibility(): Promise<any> {
    return api('GET', '/api/providers/compatibility');
  },

  async testProviderPayload(payload: Provider): Promise<any> {
    return api('POST', '/api/providers/test', providerBody(payload, true));
  },

  async saveModelMappings(id: string, mappings: Record<string, string>): Promise<any> {
    return api('PUT', `/api/providers/${encodeURIComponent(id)}/models`, { models: mappings });
  },

  async fetchProviderModels(id: string): Promise<any> {
    return api('GET', `/api/providers/${encodeURIComponent(id)}/models/available`);
  },

  async fetchProviderModelsPayload(payload: Provider): Promise<any> {
    return api('POST', '/api/providers/models/available', providerBody(payload, false));
  },

  async autofillProviderModels(id: string): Promise<any> {
    return api('POST', `/api/providers/${encodeURIComponent(id)}/models/autofill`);
  },

  async getDesktopStatus(): Promise<DesktopStatus> {
    const data = await api('GET', '/api/desktop/status');
    const status = await api('GET', '/api/status');
    const proxyPort = status.proxyPort || 18080;
    const registryConfig = data.keys || {};
    return {
      configured: !!data.configured,
      health: data.health || { needsApply: false, issues: [] },
      config: {
        inferenceProvider: registryConfig.inferenceProvider || 'gateway',
        inferenceGatewayBaseUrl: registryConfig.inferenceGatewayBaseUrl || `http://127.0.0.1:${proxyPort}`,
        inferenceGatewayApiKey: registryConfig.inferenceGatewayApiKey ? '******' : '',
        inferenceGatewayAuthScheme: registryConfig.inferenceGatewayAuthScheme || 'bearer',
        inferenceModels: registryConfig.inferenceModels || '[]',
      },
    };
  },

  async configureDesktop(): Promise<any> {
    return api('POST', '/api/desktop/configure');
  },

  async clearDesktop(): Promise<any> {
    return api('POST', '/api/desktop/clear');
  },

  async getDesktopSnapshots(): Promise<any[]> {
    const data = await api('GET', '/api/desktop/snapshots');
    return data.snapshots || [];
  },

  async restoreDesktopSnapshot(snapshotId: string): Promise<any> {
    return api('POST', '/api/desktop/restore', {
      snapshotId,
      cleanupAll: true,
    });
  },

  async startProxy(port?: number): Promise<ProxyStatus> {
    if (port) {
      await this.saveSettings({ proxyPort: Number(port) });
    }
    await api('POST', '/api/proxy/start', port ? { port: Number(port) } : undefined);
    const status = await api('GET', '/api/status');
    return {
      running: !!status.proxyRunning,
      port: status.proxyPort || port || 18080,
      stats: { total: 0, success: 0, failed: 0, today: 0 } // Default stats structure
    };
  },

  async stopProxy(): Promise<any> {
    await api('POST', '/api/proxy/stop');
    const status = await api('GET', '/api/status');
    return {
      running: !!status.proxyRunning,
      port: status.proxyPort || 18080,
    };
  },

  async getProxyLogs(): Promise<ProxyLog[]> {
    const data = await api('GET', '/api/proxy/logs');
    return (data.logs || []).map(mapLog);
  },

  async getProxyStatus(): Promise<ProxyStatus> {
    const data = await api('GET', '/api/proxy/status');
    return {
      running: !!data.running,
      port: data.port || 18080,
      stats: data.stats || { total: 0, success: 0, failed: 0, today: 0 },
    };
  },

  async clearLogs(): Promise<any> {
    return api('POST', '/api/proxy/logs/clear');
  },

  async openLogDir(): Promise<any> {
    return api('POST', '/api/proxy/logs/open-dir');
  },

  async getSettings(): Promise<any> {
    return api('GET', '/api/settings');
  },

  async getVersion(): Promise<any> {
    return api('GET', '/api/version');
  },

  async saveSettings(settings: any): Promise<any> {
    const data = await api('PUT', '/api/settings', settings);
    return data.settings || data;
  },

  async checkUpdate(updateUrl?: string): Promise<any> {
    const params = new URLSearchParams();
    if (updateUrl) params.set('url', updateUrl);
    return api('GET', `/api/update/check?${params.toString()}`);
  },

  async installUpdate(updateUrl?: string): Promise<any> {
    return api('POST', '/api/update/install', updateUrl ? { url: updateUrl } : {});
  },

  async createBackup(): Promise<any> {
    return api('POST', '/api/config/backup');
  },

  async listBackups(): Promise<any[]> {
    const data = await api('GET', '/api/config/backups');
    return data.backups || [];
  },

  async exportConfig(): Promise<any> {
    return api('GET', '/api/config/export');
  },

  async importConfig(configData: any): Promise<any> {
    return api('POST', '/api/config/import', configData);
  },

  async getDesktopSnapshotStatus(): Promise<any> {
    return api('GET', '/api/desktop/snapshot-status');
  },

  async restartCodexApp(): Promise<any> {
    return api('POST', '/api/desktop/restart-codex-app');
  },

  async submitFeedback(payload: any): Promise<any> {
    return api('POST', '/api/feedback', payload);
  },

  async getActivities(): Promise<Activity[]> {
    const data = await api('GET', '/api/proxy/logs');
    const logs = data.logs || [];
    return logs.slice(-5).reverse().map((log: any) => ({
      time: log.time,
      text: log.message,
    }));
  },

  // ── Gemini CLI OAuth ──
  async getGeminiOauthStatus(): Promise<any> {
    return api('GET', '/api/gemini-oauth/status');
  },

  async loginGeminiOauth(): Promise<any> {
    return api('POST', '/api/gemini-oauth/login', {});
  },

  async logoutGeminiOauth(): Promise<any> {
    return api('DELETE', '/api/gemini-oauth/logout');
  },

  // ── Antigravity OAuth ──
  async getAntigravityOauthStatus(): Promise<any> {
    return api('GET', '/api/antigravity-oauth/status');
  },

  async loginAntigravityOauth(): Promise<any> {
    return api('POST', '/api/antigravity-oauth/login', {});
  },

  async logoutAntigravityOauth(): Promise<any> {
    return api('DELETE', '/api/antigravity-oauth/logout');
  },

  async getAntigravityOauthModels(): Promise<any> {
    return api('GET', '/api/antigravity-oauth/models');
  },
};

// ── Plugin Unlock API ──
export const PluginUnlockApi = {
  async status(): Promise<any> {
    return api('GET', '/api/desktop/plugin-unlock/status');
  },
  async start(): Promise<any> {
    return api('POST', '/api/desktop/plugin-unlock/start');
  },
  async stop(): Promise<any> {
    return api('POST', '/api/desktop/plugin-unlock/stop');
  },
  async reinject(): Promise<any> {
    return api('POST', '/api/desktop/plugin-unlock/reinject');
  },
};
