<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { t } from '@/i18n'
import {
  getConnectors,
  addConnectorSource,
  removeConnectorSource,
  toggleConnectorSource,
  iconSrc,
  type Connector,
  type ConnectorRegistry,
  type ConnectorSourceMeta,
} from '@/api/marketplace'
import { useToast } from '@/composables/useToast'
import AppButton from '@/components/ui/AppButton.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppModal from '@/components/ui/AppModal.vue'
import IconPlus from '~icons/lucide/plus'
import IconRefresh from '~icons/lucide/refresh-cw'
import IconSearch from '~icons/lucide/search'
import IconExternalLink from '~icons/lucide/external-link'

// 连接器市场(多源)— 官方源 + 用户自加源,聚合展示。嵌进 McpPanel 的 Marketplace 子区。
const { show: toast } = useToast()
const loading = ref(true)
const error = ref('')
const registry = ref<ConnectorRegistry | null>(null)
const search = ref('')
const failedIcons = ref<Set<string>>(new Set())
const addModal = ref(false)
const newName = ref('')
const newUrl = ref('')

async function load(force = false) {
  loading.value = true
  error.value = ''
  if (force) registry.value = null
  try {
    registry.value = await getConnectors(force)
  } catch (e) {
    error.value = (e as Error).message || String(e)
  } finally {
    loading.value = false
  }
}
onMounted(() => load())

function srcLabel(s: ConnectorSourceMeta): string {
  return s.official ? t('codex.mcp.officialSource') : s.name
}
async function toggle(s: ConnectorSourceMeta) {
  try {
    await toggleConnectorSource(s.id, !s.enabled)
    await load(true)
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function remove(s: ConnectorSourceMeta) {
  if (!window.confirm(t('codex.mcp.deleteSource') + '?')) return
  try {
    await removeConnectorSource(s.id)
    await load(true)
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}
async function confirmAdd() {
  const name = newName.value.trim()
  const url = newUrl.value.trim()
  if (!name || !url) return
  try {
    await addConnectorSource(name, url)
    addModal.value = false
    newName.value = ''
    newUrl.value = ''
    await load(true)
  } catch (e) {
    toast((e as Error).message || t('toast.requestFailed'), 'error')
  }
}

const sourceErrors = computed(() => Object.entries(registry.value?.errors || {}))

function displayName(c: Connector): string {
  // 强制返回 string —— 自加源可能把 display_name 设成非 string(数字等),否则 initial() 的
  // displayName(c).charAt(0) 会崩。name 后端已校验为 string。
  return typeof c.display_name === 'string' && c.display_name ? c.display_name : c.name
}

const filtered = computed<Connector[]>(() => {
  const all = registry.value?.connectors ?? []
  const q = search.value.trim().toLowerCase()
  if (!q) return all
  return all.filter((c) =>
    [c.display_name, c.name, c.short_description, c.developer_name, c.category]
      .filter(Boolean)
      .some((s) => String(s).toLowerCase().includes(q)),
  )
})

const grouped = computed(() => {
  const order = registry.value?.categories ?? []
  const byCat = new Map<string, Connector[]>()
  for (const c of filtered.value) {
    const cat = c.category || 'Other'
    if (!byCat.has(cat)) byCat.set(cat, [])
    byCat.get(cat)!.push(c)
  }
  return [...byCat.keys()]
    .sort((a, b) => {
      const ia = order.indexOf(a)
      const ib = order.indexOf(b)
      return (ia === -1 ? 999 : ia) - (ib === -1 ? 999 : ib)
    })
    .map((cat) => ({ cat, items: byCat.get(cat)! }))
})

function onIconError(id: string) {
  failedIcons.value = new Set(failedIcons.value).add(id)
}
function initial(c: Connector): string {
  return displayName(c).charAt(0).toUpperCase()
}
function safeWebsite(c: Connector): string | null {
  const u = c.website_url
  return u && /^https?:\/\//i.test(u) ? u : null
}
</script>

<template>
  <div class="cmkt">
    <div class="cmkt__head">
      <button
        v-for="s in registry?.sources || []"
        :key="s.id"
        type="button"
        class="cmkt-src"
        :class="{ disabled: !s.enabled }"
        @click="toggle(s)"
      >
        {{ srcLabel(s) }}<span class="cmkt-src__count">{{ s.count }}</span>
        <span v-if="!s.official" class="cmkt-src__remove" @click.stop="remove(s)">×</span>
      </button>
      <AppButton size="sm" :icon="IconPlus" :label="t('codex.mcp.sourceAdd')" @click="addModal = true" />
      <AppButton size="sm" :icon="IconRefresh" :label="t('codex.mcp.refresh')" @click="load(true)" />
      <div class="cmkt__search">
        <IconSearch class="cmkt__search-icon" />
        <input v-model="search" type="text" :placeholder="t('market.search')" />
      </div>
    </div>

    <div v-for="[id, msg] in sourceErrors" :key="id" class="cmkt__src-error">
      <code>{{ id }}</code> · {{ msg }}
    </div>

    <div v-if="loading" class="cmkt__state">{{ t('market.loading') }}</div>

    <div v-else-if="error" class="cmkt__state cmkt__state--error">
      <p>{{ t('market.loadFailed') }}</p>
      <code>{{ error }}</code>
      <AppButton size="sm" :label="t('codex.mcp.refresh')" @click="load(true)" />
    </div>

    <div v-else-if="filtered.length === 0" class="cmkt__state">{{ t('market.empty') }}</div>

    <section v-for="group in grouped" v-else :key="group.cat" class="cmkt__group">
      <h3 class="cmkt__group-title">
        {{ group.cat }} <span class="cmkt__group-count">{{ group.items.length }}</span>
      </h3>
      <div class="cmkt__grid">
        <article v-for="c in group.items" :key="`${c.source}/${c.id}`" class="cmkt-card">
          <div
            v-if="failedIcons.has(c.id) || !c.logo_url"
            class="cmkt-card__logo cmkt-card__logo--fallback"
            :style="{ background: c.brand_color || 'var(--accent)' }"
          >
            {{ initial(c) }}
          </div>
          <img
            v-else
            class="cmkt-card__logo"
            :src="iconSrc(c.logo_url, c.source)"
            :alt="displayName(c)"
            loading="lazy"
            @error="onIconError(c.id)"
          />
          <div class="cmkt-card__body">
            <div class="cmkt-card__name">{{ displayName(c) }}</div>
            <div class="cmkt-card__desc">{{ c.short_description }}</div>
          </div>
          <a
            v-if="safeWebsite(c)"
            class="cmkt-card__link"
            :href="safeWebsite(c)!"
            target="_blank"
            rel="noopener noreferrer"
            :title="t('market.openWebsite')"
          >
            <IconExternalLink />
          </a>
        </article>
      </div>
    </section>

    <AppModal v-if="addModal" :title="t('codex.mcp.sourceAddTitle')" @close="addModal = false">
      <div class="cmkt__add-fields">
        <AppInput v-model="newName" placeholder="My Connectors" />
        <AppInput v-model="newUrl" placeholder="https://example.com/registry.json" />
      </div>
      <div class="cmkt__add-actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="addModal = false" />
        <AppButton variant="primary" :label="t('codex.mcp.sourceAddConfirm')" @click="confirmAdd" />
      </div>
    </AppModal>
  </div>
</template>

<style scoped>
.cmkt {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.cmkt__head {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.cmkt-src {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 6px 4px 12px;
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent);
  font-size: var(--fs-sm);
  font-weight: 600;
  cursor: pointer;
}
.cmkt-src.disabled {
  background: var(--surface-2);
  color: var(--text-muted);
  border-color: var(--border);
  opacity: 0.7;
}
.cmkt-src__count {
  font-size: var(--fs-xs);
  font-weight: 500;
  background: var(--surface);
  color: var(--text-secondary);
  padding: 0 7px;
  border-radius: var(--radius-full);
}
.cmkt-src__remove {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  border-radius: var(--radius-full);
  color: var(--text-muted);
  font-size: 14px;
}
.cmkt-src__remove:hover {
  color: var(--danger);
}
.cmkt__search {
  position: relative;
  display: flex;
  align-items: center;
  margin-left: auto;
}
.cmkt__search-icon {
  position: absolute;
  left: 10px;
  width: 15px;
  height: 15px;
  color: var(--text-muted);
  pointer-events: none;
}
.cmkt__search input {
  width: 200px;
  padding: 6px 12px 6px 30px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-sm);
}
.cmkt__search input:focus {
  outline: none;
  border-color: var(--accent);
}
.cmkt__src-error {
  padding: var(--space-2) var(--space-3);
  background: var(--danger-soft);
  border-radius: var(--radius-sm);
  color: var(--danger);
  font-size: var(--fs-xs);
}
.cmkt__state {
  padding: var(--space-5);
  text-align: center;
  color: var(--text-muted);
}
.cmkt__state--error code {
  display: block;
  margin: var(--space-2) auto;
  max-width: 600px;
  padding: var(--space-2);
  background: var(--surface-2);
  border-radius: var(--radius-sm);
  font-size: var(--fs-xs);
  color: var(--text-secondary);
  word-break: break-all;
}
.cmkt__group {
  margin-top: var(--space-3);
}
.cmkt__group-title {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--fs-md);
  font-weight: 600;
  margin: 0 0 var(--space-2);
}
.cmkt__group-count {
  font-size: var(--fs-xs);
  font-weight: 400;
  color: var(--text-muted);
}
.cmkt__grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
  gap: var(--space-2);
}
.cmkt-card {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  transition: border-color var(--transition), box-shadow var(--transition);
}
.cmkt-card:hover {
  border-color: var(--border-strong);
  box-shadow: var(--shadow-sm);
}
.cmkt-card__logo {
  flex-shrink: 0;
  width: 38px;
  height: 38px;
  border-radius: var(--radius);
  object-fit: cover;
  background: var(--surface-2);
}
.cmkt-card__logo--fallback {
  display: flex;
  align-items: center;
  justify-content: center;
  color: #fff;
  font-weight: 600;
  font-size: 15px;
}
.cmkt-card__body {
  flex: 1;
  min-width: 0;
}
.cmkt-card__name {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.cmkt-card__desc {
  margin-top: 2px;
  font-size: var(--fs-xs);
  color: var(--text-muted);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.cmkt-card__link {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border-radius: var(--radius-sm);
  color: var(--text-muted);
}
.cmkt-card__link:hover {
  background: var(--surface-2);
  color: var(--accent);
}
.cmkt-card__link svg {
  width: 15px;
  height: 15px;
}
.cmkt__add-fields {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.cmkt__add-fields :deep(.app-input) {
  width: 100%;
}
.cmkt__add-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
</style>
