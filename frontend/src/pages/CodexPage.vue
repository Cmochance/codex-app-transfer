<script setup lang="ts">
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { t } from '@/i18n'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import ManagedMarkdownPanel from '@/components/codex/ManagedMarkdownPanel.vue'
import McpPanel from '@/components/codex/McpPanel.vue'
import ConversationsPanel from '@/components/codex/ConversationsPanel.vue'

type Tab = 'agents' | 'memories' | 'skills' | 'mcp' | 'conversations'
const TABS: Tab[] = ['agents', 'memories', 'skills', 'mcp', 'conversations']

const route = useRoute()
const router = useRouter()

const tabOptions = computed<{ value: Tab; label: string }[]>(() => [
  { value: 'agents', label: t('codex.tabAgents') },
  { value: 'memories', label: t('codex.tabMemories') },
  { value: 'skills', label: t('codex.tabSkills') },
  { value: 'mcp', label: t('codex.tabMcp') },
  { value: 'conversations', label: t('codex.tabConversations') },
])

// 子 tab 同步 ?tab= query(deeplink 友好;旧 SPA 用 ?tab=)
const tab = computed<Tab>({
  get() {
    const q = route.query.tab as Tab | undefined
    return q && TABS.includes(q) ? q : 'agents'
  },
  set(v) {
    router.replace({ query: { ...route.query, tab: v } })
  },
})
</script>

<template>
  <div>
    <div class="codex-subnav">
      <SegmentedControl v-model="tab" :options="tabOptions" />
    </div>

    <!-- KeepAlive 按 key 缓存各子面板状态(切 tab 不丢编辑草稿/选中/滚动)-->
    <KeepAlive>
      <ManagedMarkdownPanel v-if="tab === 'agents'" key="agents" resource="agents" />
      <ManagedMarkdownPanel v-else-if="tab === 'memories'" key="memories" resource="memories" />
      <ManagedMarkdownPanel v-else-if="tab === 'skills'" key="skills" resource="skills" />
      <McpPanel v-else-if="tab === 'mcp'" key="mcp" />
      <ConversationsPanel v-else key="conversations" />
    </KeepAlive>
  </div>
</template>

<style scoped>
.codex-subnav {
  display: flex;
  justify-content: center;
  margin-bottom: var(--space-5);
}
</style>
