(function () {
  'use strict';

  const BASE = '';

  async function api(method, path, body) {
    const opts = { method, headers: { 'X-CAS-Request': '1' } };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    const resp = await fetch(BASE + path, opts);
    const data = await resp.json();
    if (!resp.ok || data.success === false) {
      throw new Error(data.message || `Request failed: ${method} ${path}`);
    }
    return data;
  }

  // ── 工具 ──
  const ICON_MAP = {
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
  };

  function computeIcon(provider) {
    const id = `${provider.id || ''} ${provider.name || ''} ${provider.baseUrl || ''}`.toLowerCase();
    for (const [key, val] of Object.entries(ICON_MAP)) {
      if (id.includes(key)) return val;
    }
    return { icon: 'bi-plug-fill' };
  }

  function mapProvider(provider, activeId) {
    const models = provider.models || {};
    return {
      id: provider.id,
      name: provider.name,
      baseUrl: provider.baseUrl,
      apiFormat: ['openai', 'openai_chat'].includes(provider.apiFormat) ? 'openai_chat' : (provider.apiFormat || 'responses'),
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

  function providerBody(payload, includeModels = true) {
    const body = {
      name: payload.name,
      baseUrl: payload.baseUrl,
      authScheme: payload.authScheme || 'bearer',
      apiFormat: ['OpenAI', 'openai', 'openai_chat'].includes(payload.apiFormat) ? 'openai_chat' : 'responses',
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
    return body;
  }

  function mapLog(log) {
    return {
      at: log.time,
      level: log.level.toLowerCase(),
      message: log.message,
    };
  }

  // ── 公开 API ──
  window.CCApi = {
    async getStatus() {
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

    async getProviders() {
      const data = await api('GET', '/api/providers');
      return (data.providers || []).map(p => mapProvider(p, data.activeId));
    },

    async getProviderSecret(id) {
      return api('GET', `/api/providers/${encodeURIComponent(id)}/secret`);
    },

    async getPresets() {
      const data = await api('GET', '/api/presets');
      return (data.presets || []).map(p => ({
        id: p.id,
        name: p.name,
        baseUrl: p.baseUrl,
        apiFormat: ['openai', 'openai_chat'].includes(p.apiFormat) ? 'OpenAI' : 'Responses',
        authScheme: p.authScheme || 'bearer',
        models: p.models || {},
        modelOptions: p.modelOptions || {},
        baseUrlOptions: p.baseUrlOptions || [],
        baseUrlHint: p.baseUrlHint || '',
        requestOptionPresets: p.requestOptionPresets || {},
        extraHeaders: p.extraHeaders || {},
        modelCapabilities: p.modelCapabilities || {},
        requestOptions: p.requestOptions || {},
        ...computeIcon(p),
      }));
    },

    async addProvider(payload) {
      const data = await api('POST', '/api/providers', providerBody(payload));
      return data.provider || data;
    },

    async updateProvider(id, payload) {
      const data = await api('PUT', `/api/providers/${encodeURIComponent(id)}`, providerBody(payload));
      return data.provider || data;
    },

    async deleteProvider(id) {
      return api('DELETE', `/api/providers/${encodeURIComponent(id)}`);
    },

    async setDefaultProvider(id) {
      return api('PUT', `/api/providers/${encodeURIComponent(id)}/default`);
    },

    async saveDraft(id, payload) {
      return api('POST', `/api/providers/${encodeURIComponent(id)}/draft`, providerBody(payload, true));
    },

    async activateProvider(id) {
      return api('POST', `/api/providers/${encodeURIComponent(id)}/activate`);
    },

    async reorderProviders(providerIds) {
      return api('PUT', '/api/providers/reorder', { providerIds });
    },

    async testProvider(id) {
      return api('POST', `/api/providers/${encodeURIComponent(id)}/test`);
    },

    async queryProviderUsage(id) {
      return api('POST', `/api/providers/${encodeURIComponent(id)}/usage`);
    },

    async getProviderCompatibility() {
      return api('GET', '/api/providers/compatibility');
    },

    async testProviderPayload(payload) {
      return api('POST', '/api/providers/test', providerBody(payload, false));
    },

    async saveModelMappings(id, mappings) {
      return api('PUT', `/api/providers/${encodeURIComponent(id)}/models`, { models: mappings });
    },

    async fetchProviderModels(id) {
      return api('GET', `/api/providers/${encodeURIComponent(id)}/models/available`);
    },

    async fetchProviderModelsPayload(payload) {
      return api('POST', '/api/providers/models/available', providerBody(payload, false));
    },

    async autofillProviderModels(id) {
      return api('POST', `/api/providers/${encodeURIComponent(id)}/models/autofill`);
    },

    async getDesktopStatus() {
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
          inferenceModels: registryConfig.inferenceModels || '["sonnet","haiku","opus"]',
        },
      };
    },

    async configureDesktop() {
      const result = await api('POST', '/api/desktop/configure');
      return result;
    },

    async clearDesktop() {
      await api('POST', '/api/desktop/clear');
      return this.getDesktopStatus();
    },

    async startProxy(port) {
      if (port) {
        await this.saveSettings({ proxyPort: Number(port) });
      }
      await api('POST', '/api/proxy/start', port ? { port: Number(port) } : undefined);
      const status = await api('GET', '/api/status');
      return {
        running: !!status.proxyRunning,
        port: status.proxyPort || port || 18080,
      };
    },

    async stopProxy() {
      await api('POST', '/api/proxy/stop');
      return { running: false };
    },

    async getProxyLogs() {
      const data = await api('GET', '/api/proxy/logs');
      return (data.logs || []).map(mapLog);
    },

    async getProxyStatus() {
      const data = await api('GET', '/api/proxy/status');
      return {
        running: !!data.running,
        port: data.port || 18080,
        stats: data.stats || { total: 0, success: 0, failed: 0, today: 0 },
      };
    },

    async clearLogs() {
      return api('POST', '/api/proxy/logs/clear');
    },

    async openLogDir() {
      return api('POST', '/api/proxy/logs/open-dir');
    },

    async getSettings() {
      return api('GET', '/api/settings');
    },

    async saveSettings(settings) {
      const data = await api('PUT', '/api/settings', settings);
      return data.settings || data;
    },

    async checkUpdate(updateUrl) {
      const params = new URLSearchParams();
      if (updateUrl) params.set('url', updateUrl);
      return api('GET', `/api/update/check?${params.toString()}`);
    },

    async installUpdate(updateUrl) {
      return api('POST', '/api/update/install', updateUrl ? { url: updateUrl } : {});
    },

    async createBackup() {
      return api('POST', '/api/config/backup');
    },

    async listBackups() {
      const data = await api('GET', '/api/config/backups');
      return data.backups || [];
    },

    async exportConfig() {
      return api('GET', '/api/config/export');
    },

    async importConfig(configData) {
      return api('POST', '/api/config/import', configData);
    },

    async getDesktopSnapshotStatus() {
      return api('GET', '/api/desktop/snapshot-status');
    },

    async getActivities() {
      const data = await api('GET', '/api/proxy/logs');
      const logs = data.logs || [];
      return logs.slice(-5).reverse().map(log => ({
        time: log.time,
        text: log.message,
      }));
    },
  };
})();
