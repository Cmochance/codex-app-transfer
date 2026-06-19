<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { t } from '@/i18n'
import { getConnectors, iconSrc, type Connector, type ConnectorRegistry } from '@/api/marketplace'
import AppButton from '@/components/ui/AppButton.vue'
import IconRefresh from '~icons/lucide/refresh-cw'
import IconSearch from '~icons/lucide/search'
import IconExternalLink from '~icons/lucide/external-link'

// 连接器市场(官方源)— 嵌进 McpPanel 的 Marketplace 子区。数据来自私有 storage 仓库的展示镜像。
const loading = ref(true)
const error = ref('')
const registry = ref<ConnectorRegistry | null>(null)
const search = ref('')
const failedIcons = ref<Set<string>>(new Set())

async function load(force = false) {
  loading.value = true
  error.value = ''
  if (force) registry.value = null
  try {
    registry.value = await getConnectors()
  } catch (e) {
    error.value = (e as Error).message || String(e)
  } finally {
    loading.value = false
  }
}
onMounted(() => load())

function displayName(c: Connector): string {
  return c.display_name || c.name
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

// 按 registry.categories 顺序分组(列表外的归末尾)。
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
// 只放行 http(s) 官网链接(防 registry 里万一有 javascript:/data: 之类 URL 进 href)。
function safeWebsite(c: Connector): string | null {
  const u = c.website_url
  return u && /^https?:\/\//i.test(u) ? u : null
}
</script>

<template>
  <div class="cmkt">
    <div class="cmkt__head">
      <span class="cmkt__source">{{ t('codex.mcp.officialSource') }}</span>
      <span v-if="registry" class="cmkt__count">{{ registry.connectors.length }}</span>
      <div class="cmkt__search">
        <IconSearch class="cmkt__search-icon" />
        <input v-model="search" type="text" :placeholder="t('market.search')" />
      </div>
      <AppButton size="sm" :icon="IconRefresh" :label="t('market.refresh')" @click="load(true)" />
    </div>

    <p class="cmkt__note">{{ t('market.note') }}</p>

    <div v-if="loading" class="cmkt__state">{{ t('market.loading') }}</div>

    <div v-else-if="error" class="cmkt__state cmkt__state--error">
      <p>{{ t('market.loadFailed') }}</p>
      <code>{{ error }}</code>
      <AppButton size="sm" :label="t('market.refresh')" @click="load(true)" />
    </div>

    <div v-else-if="filtered.length === 0" class="cmkt__state">{{ t('market.empty') }}</div>

    <section v-for="group in grouped" v-else :key="group.cat" class="cmkt__group">
      <h3 class="cmkt__group-title">
        {{ group.cat }} <span class="cmkt__group-count">{{ group.items.length }}</span>
      </h3>
      <div class="cmkt__grid">
        <article v-for="c in group.items" :key="c.id" class="cmkt-card">
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
            :src="iconSrc(c.logo_url)"
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
  gap: var(--space-2);
  flex-wrap: wrap;
}
.cmkt__source {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--accent);
  background: var(--accent-soft);
  padding: 2px 10px;
  border-radius: var(--radius-full);
}
.cmkt__count {
  font-size: var(--fs-sm);
  color: var(--text-muted);
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
  width: 220px;
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
.cmkt__note {
  margin: 0;
  font-size: var(--fs-xs);
  color: var(--text-muted);
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
</style>
