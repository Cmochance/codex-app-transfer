<script setup lang="ts">
import { computed, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useMcpRecovery } from '@/composables/useMcpRecovery'
import AppModal from '@/components/ui/AppModal.vue'
import AppButton from '@/components/ui/AppButton.vue'

const mcp = useMcpRecovery()
const { show: toast } = useToast()

// 勾选的 server_key(本地多选状态)。
const selected = ref<Set<string>>(new Set())

const allKeys = computed(() => mcp.entries.value.map((e) => e.key))
const allSelected = computed(
  () => allKeys.value.length > 0 && allKeys.value.every((k) => selected.value.has(k)),
)
const selectedKeys = computed(() => allKeys.value.filter((k) => selected.value.has(k)))

function toggleRow(key: string, checked: boolean) {
  const next = new Set(selected.value)
  if (checked) next.add(key)
  else next.delete(key)
  selected.value = next
}
function toggleSelectAll(checked: boolean) {
  selected.value = checked ? new Set(allKeys.value) : new Set()
}

// 操作即时生效 + composable 内 refresh 刷新列表;勾选剪掉已不在列表的;列表清空则关弹窗。
function afterAction() {
  selected.value = new Set(
    [...selected.value].filter((k) => allKeys.value.includes(k)),
  )
  if (mcp.entries.value.length === 0) mcp.closeModal()
}
async function doRestore(keys: string[]) {
  if (!keys.length || mcp.busy.value) return
  try {
    const n = await mcp.restore(keys)
    toast(tFmt('mcp.restoreDone', { count: n }))
    afterAction()
  } catch (e) {
    toast((e as Error).message, 'error')
  }
}
async function doRemove(keys: string[]) {
  if (!keys.length || mcp.busy.value) return
  try {
    const n = await mcp.remove(keys)
    toast(tFmt('mcp.recovery.removeDone', { count: n }))
    afterAction()
  } catch (e) {
    toast((e as Error).message, 'error')
  }
}
async function doIgnore(keys: string[]) {
  if (!keys.length || mcp.busy.value) return
  try {
    const n = await mcp.ignore(keys)
    toast(tFmt('mcp.recovery.ignoreDone', { count: n }))
    afterAction()
  } catch (e) {
    toast((e as Error).message, 'error')
  }
}
</script>

<template>
  <AppModal
    v-if="mcp.open.value"
    :title="t('mcp.restorePromptTitle')"
    wide
    @close="mcp.closeModal()"
  >
    <div class="mcp-rec">
      <p class="mcp-rec__intro">
        {{ tFmt('mcp.restorePromptBody', { count: mcp.entries.value.length }) }}
      </p>

      <div class="mcp-rec__toolbar">
        <label class="mcp-rec__check">
          <input
            type="checkbox"
            :checked="allSelected"
            @change="toggleSelectAll(($event.target as HTMLInputElement).checked)"
          />
          {{ t('mcp.recovery.selectAll') }}
        </label>
        <div class="mcp-rec__toolbar-actions">
          <AppButton
            size="sm"
            variant="primary"
            :disabled="!selectedKeys.length || mcp.busy.value"
            :label="t('mcp.recovery.restoreSelected')"
            @click="doRestore(selectedKeys)"
          />
          <AppButton
            size="sm"
            variant="danger"
            :disabled="!selectedKeys.length || mcp.busy.value"
            :label="t('mcp.recovery.removeSelected')"
            @click="doRemove(selectedKeys)"
          />
          <AppButton
            size="sm"
            variant="ghost"
            :disabled="!allKeys.length || mcp.busy.value"
            :label="t('mcp.recovery.ignoreAll')"
            @click="doIgnore(allKeys)"
          />
        </div>
      </div>

      <ul class="mcp-rec__list">
        <li v-for="e in mcp.entries.value" :key="e.key" class="mcp-rec__row">
          <label class="mcp-rec__check">
            <input
              type="checkbox"
              :checked="selected.has(e.key)"
              @change="toggleRow(e.key, ($event.target as HTMLInputElement).checked)"
            />
          </label>
          <span class="mcp-rec__key" :title="e.key">{{ e.key }}</span>
          <span v-if="e.ignored" class="mcp-rec__badge">{{ t('mcp.recovery.ignored') }}</span>
          <span class="mcp-rec__spacer" />
          <AppButton
            size="sm"
            variant="secondary"
            :disabled="mcp.busy.value"
            :label="t('mcp.recovery.restore')"
            @click="doRestore([e.key])"
          />
          <AppButton
            size="sm"
            variant="ghost"
            :disabled="mcp.busy.value"
            :label="t('mcp.recovery.remove')"
            @click="doRemove([e.key])"
          />
          <AppButton
            v-if="!e.ignored"
            size="sm"
            variant="ghost"
            :disabled="mcp.busy.value"
            :label="t('mcp.recovery.ignore')"
            @click="doIgnore([e.key])"
          />
        </li>
      </ul>
    </div>
  </AppModal>
</template>

<style scoped>
.mcp-rec {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 420px;
}
.mcp-rec__intro {
  margin: 0;
  font-size: 13px;
  line-height: 1.5;
  color: var(--text-secondary, #6b7280);
}
.mcp-rec__toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  flex-wrap: wrap;
}
.mcp-rec__toolbar-actions {
  display: flex;
  gap: 6px;
}
.mcp-rec__check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  cursor: pointer;
}
.mcp-rec__list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 50vh;
  overflow-y: auto;
}
.mcp-rec__row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  border-radius: 8px;
  background: var(--surface-2, rgba(127, 127, 127, 0.06));
}
.mcp-rec__key {
  font-family: var(--font-mono, monospace);
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 240px;
}
.mcp-rec__badge {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 6px;
  background: rgba(127, 127, 127, 0.18);
  color: var(--text-secondary, #6b7280);
}
.mcp-rec__spacer {
  flex: 1;
}
</style>
