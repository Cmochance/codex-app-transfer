<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue'
import { useProxyStore } from '@/stores/proxy'
import { t } from '@/i18n'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppButton from '@/components/ui/AppButton.vue'
import IconTrash from '~icons/lucide/trash-2'

const store = useProxyStore()
let timer: number | undefined

onMounted(async () => {
  await store.loadStatus().catch(() => {})
  await store.loadLogs().catch(() => {})
  timer = window.setInterval(() => {
    if (store.running) store.loadLogs().catch(() => {})
  }, 2000)
})
onUnmounted(() => {
  if (timer) clearInterval(timer)
})

async function onToggle(on: boolean) {
  await store.toggle(on)
  await store.loadLogs().catch(() => {})
}
</script>

<template>
  <div>
    <h1 class="page-title">{{ t('nav.proxy') }}</h1>

    <SettingsGroup>
      <SettingsRow title="本地转发代理" description="启动后 Codex 经本地代理转发到当前提供商上游">
        <AppSwitch :model-value="store.running" @update:model-value="onToggle" />
      </SettingsRow>
      <SettingsRow title="监听端口" :description="`127.0.0.1:${store.port || '—'}`">
        <span class="port mono">{{ store.port || '—' }}</span>
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="实时日志">
      <div class="logs">
        <div v-if="!store.logs.length" class="logs__empty">暂无日志</div>
        <div
          v-for="(l, i) in store.logs"
          :key="i"
          class="logs__line"
          :class="`logs__line--${l.level}`"
        >
          {{ l.message }}
        </div>
      </div>
    </SettingsGroup>

    <div class="page-actions">
      <AppButton variant="ghost" size="sm" :icon="IconTrash" label="清空日志" @click="store.clearLogs()" />
    </div>
  </div>
</template>

<style scoped>
.page-title {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0 0 var(--space-4);
}
.port {
  font-size: var(--fs-md);
  color: var(--text-secondary);
}
.logs {
  max-height: 360px;
  overflow-y: auto;
  padding: var(--space-3) var(--space-4);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
}
.logs__empty {
  color: var(--text-muted);
  text-align: center;
  padding: var(--space-4) 0;
}
.logs__line {
  padding: 2px 0;
  color: var(--text-secondary);
  white-space: pre-wrap;
  word-break: break-all;
}
.logs__line--error {
  color: var(--danger);
}
.logs__line--warn {
  color: var(--warning);
}
.page-actions {
  display: flex;
  justify-content: flex-end;
  margin-top: var(--space-3);
}
</style>
