<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { t } from '@/i18n'
import { getConnectors, iconSrc, type Connector, type ConnectorRegistry } from '@/api/marketplace'
import AppButton from '@/components/ui/AppButton.vue'
import IconRefresh from '~icons/lucide/refresh-cw'
import IconSearch from '~icons/lucide/search'
import IconExternalLink from '~icons/lucide/external-link'
import IconStore from '~icons/lucide/store'

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
</script>

<template>
  <div class="market">
    <header class="market__head">
      <div class="market__title">
        <IconStore class="market__title-icon" />
        <h1>{{ t('market.title') }}</h1>
        <span v-if="registry" class="market__count">{{ registry.connectors.length }}</span>
      </div>
      <div class="market__actions">
        <div class="market__search">
          <IconSearch class="market__search-icon" />
          <input v-model="search" type="text" :placeholder="t('market.search')" />
        </div>
        <AppButton :icon="IconRefresh" :label="t('market.refresh')" @click="load(true)" />
      </div>
    </header>

    <p class="market__note">{{ t('market.note') }}</p>

    <div v-if="loading" class="market__state">{{ t('market.loading') }}</div>

    <div v-else-if="error" class="market__state market__state--error">
      <p>{{ t('market.loadFailed') }}</p>
      <code>{{ error }}</code>
      <AppButton :label="t('market.refresh')" @click="load(true)" />
    </div>

    <div v-else-if="filtered.length === 0" class="market__state">{{ t('market.empty') }}</div>

    <section v-for="group in grouped" v-else :key="group.cat" class="market__group">
      <h2 class="market__group-title">
        {{ group.cat }} <span class="market__group-count">{{ group.items.length }}</span>
      </h2>
      <div class="market__grid">
        <article v-for="c in group.items" :key="c.id" class="card">
          <div
            v-if="failedIcons.has(c.id) || !c.logo_url"
            class="card__logo card__logo--fallback"
            :style="{ background: c.brand_color || 'var(--accent)' }"
          >
            {{ initial(c) }}
          </div>
          <img
            v-else
            class="card__logo"
            :src="iconSrc(c.logo_url)"
            :alt="displayName(c)"
            loading="lazy"
            @error="onIconError(c.id)"
          />
          <div class="card__body">
            <div class="card__name">{{ displayName(c) }}</div>
            <div class="card__desc">{{ c.short_description }}</div>
          </div>
          <a
            v-if="c.website_url"
            class="card__link"
            :href="c.website_url"
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
.market {
  padding: var(--space-5) var(--space-6);
  max-width: 1100px;
  margin: 0 auto;
}
.market__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  flex-wrap: wrap;
}
.market__title {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.market__title h1 {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0;
}
.market__title-icon {
  width: 22px;
  height: 22px;
  color: var(--accent);
}
.market__count {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  background: var(--surface-2);
  padding: 1px 8px;
  border-radius: var(--radius-full);
}
.market__actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.market__search {
  position: relative;
  display: flex;
  align-items: center;
}
.market__search-icon {
  position: absolute;
  left: 10px;
  width: 15px;
  height: 15px;
  color: var(--text-muted);
  pointer-events: none;
}
.market__search input {
  width: 240px;
  padding: 7px 12px 7px 30px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-sm);
}
.market__search input:focus {
  outline: none;
  border-color: var(--accent);
}
.market__note {
  margin: var(--space-3) 0 var(--space-2);
  font-size: var(--fs-sm);
  color: var(--text-muted);
}
.market__state {
  padding: var(--space-6);
  text-align: center;
  color: var(--text-muted);
}
.market__state--error code {
  display: block;
  margin: var(--space-2) auto;
  max-width: 600px;
  padding: var(--space-2);
  background: var(--surface-2);
  border-radius: var(--radius-sm);
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  word-break: break-all;
}
.market__group {
  margin-top: var(--space-5);
}
.market__group-title {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--fs-md);
  font-weight: 600;
  margin: 0 0 var(--space-3);
}
.market__group-count {
  font-size: var(--fs-xs);
  font-weight: 400;
  color: var(--text-muted);
}
.market__grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: var(--space-3);
}
.card {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  transition: border-color var(--transition), box-shadow var(--transition);
}
.card:hover {
  border-color: var(--border-strong);
  box-shadow: var(--shadow-sm);
}
.card__logo {
  flex-shrink: 0;
  width: 40px;
  height: 40px;
  border-radius: var(--radius);
  object-fit: cover;
  background: var(--surface-2);
}
.card__logo--fallback {
  display: flex;
  align-items: center;
  justify-content: center;
  color: #fff;
  font-weight: 600;
  font-size: 16px;
}
.card__body {
  flex: 1;
  min-width: 0;
}
.card__name {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.card__desc {
  margin-top: 2px;
  font-size: var(--fs-xs);
  color: var(--text-muted);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.card__link {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border-radius: var(--radius-sm);
  color: var(--text-muted);
}
.card__link:hover {
  background: var(--surface-2);
  color: var(--accent);
}
.card__link svg {
  width: 15px;
  height: 15px;
}
</style>
