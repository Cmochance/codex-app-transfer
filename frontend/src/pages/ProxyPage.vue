<script setup lang="ts">
// 路由(转发)页 — 复刻原版布局(Image #8):① 状态条(运行态 + 端口 + 启停)
// ② 四列统计(总请求/成功/失败/今日) ③ 暗色日志面板(标题行 + 分列 time/level/message)。
import { onMounted, onUnmounted, ref, watch, nextTick } from 'vue'
import { useProxyStore } from '@/stores/proxy'
import { t } from '@/i18n'
import { useToast } from '@/composables/useToast'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import IconPlay from '~icons/lucide/play'
import IconSquare from '~icons/lucide/square'
import IconList from '~icons/lucide/list'
import IconCheck from '~icons/lucide/circle-check'
import IconX from '~icons/lucide/circle-x'
import IconCalendar from '~icons/lucide/calendar-days'
import IconFolder from '~icons/lucide/folder-open'
import IconTrash from '~icons/lucide/trash-2'

const store = useProxyStore()
const { show: toast } = useToast()
let timer: number | undefined

const portInput = ref<number>(18080)
const autoScroll = ref(true)
const logsEl = ref<HTMLElement>()

onMounted(async () => {
  await store.loadStatus().catch(() => {})
  portInput.value = store.port || 18080
  await store.loadLogs().catch(() => {})
  scrollToBottom()
  timer = window.setInterval(() => {
    if (store.running) store.loadLogs().catch(() => {})
  }, 2000)
})
onUnmounted(() => {
  if (timer) clearInterval(timer)
})

// 端口随后端状态同步(用户编辑后下一次 loadStatus 会覆盖,符合原版行为)
watch(
  () => store.port,
  (p) => {
    if (p) portInput.value = p
  },
)
watch(
  () => store.logs.length,
  () => {
    if (autoScroll.value) scrollToBottom()
  },
)
function scrollToBottom() {
  nextTick(() => {
    if (logsEl.value) logsEl.value.scrollTop = logsEl.value.scrollHeight
  })
}

async function onToggle() {
  const turnOn = !store.running
  try {
    await store.toggle(turnOn, turnOn ? portInput.value : undefined)
  } catch (e) {
    toast((e as Error).message || '操作失败', 'error')
  }
  store.loadLogs().catch(() => {})
}
async function onViewLogs() {
  try {
    await store.openLogDir()
  } catch (e) {
    toast((e as Error).message || '打开失败', 'error')
  }
}
async function onClearLogs() {
  try {
    await store.clearLogs()
  } catch (e) {
    toast((e as Error).message || '清除失败', 'error')
  }
}
</script>

<template>
  <div class="proxy">
    <!-- ① 状态条 -->
    <div class="status-card">
      <div class="status-card__left">
        <span class="pulse" :class="{ 'pulse--on': store.running }" />
        <span class="status-text" :class="{ 'status-text--on': store.running }">
          {{ store.running ? t('status.running') : t('status.stopped') }}
        </span>
        <span class="status-sub">{{ t('proxy.localhost') }}</span>
      </div>
      <div class="status-card__right">
        <label class="port-field">
          <span class="port-label">{{ t('proxy.port') }}</span>
          <input v-model.number="portInput" type="number" class="port-input" min="1" max="65535" />
        </label>
        <button class="proxy-toggle" :class="store.running ? 'is-stop' : 'is-start'" @click="onToggle">
          <component :is="store.running ? IconSquare : IconPlay" class="proxy-toggle__icon" />
          <span>{{ store.running ? t('proxy.stop') : t('proxy.start') }}</span>
        </button>
      </div>
    </div>

    <!-- ② 四列统计(单行:图标-数值-文字, 高度与状态条一致) -->
    <div class="stat-grid">
      <div class="stat-card">
        <span class="stat-card__icon"><IconList /></span>
        <strong class="stat-card__value">{{ store.stats.total }}</strong>
        <span class="stat-card__label">{{ t('proxy.stats.total') }}</span>
      </div>
      <div class="stat-card">
        <span class="stat-card__icon stat-card__icon--ok"><IconCheck /></span>
        <strong class="stat-card__value">{{ store.stats.success }}</strong>
        <span class="stat-card__label">{{ t('proxy.stats.success') }}</span>
      </div>
      <div class="stat-card">
        <span class="stat-card__icon stat-card__icon--err"><IconX /></span>
        <strong class="stat-card__value">{{ store.stats.failed }}</strong>
        <span class="stat-card__label">{{ t('proxy.stats.failed') }}</span>
      </div>
      <div class="stat-card">
        <span class="stat-card__icon"><IconCalendar /></span>
        <strong class="stat-card__value">{{ store.stats.today }}</strong>
        <span class="stat-card__label">{{ t('proxy.stats.today') }}</span>
      </div>
    </div>

    <!-- ③ 日志面板(标题行 + 分列) -->
    <div class="log-panel">
      <div class="log-panel__head">
        <button class="log-btn" @click="onViewLogs">
          <IconFolder class="log-btn__icon" />
          <span>{{ t('proxy.viewLog') }}</span>
        </button>
        <button class="log-btn" @click="onClearLogs">
          <IconTrash class="log-btn__icon" />
          <span>{{ t('proxy.clearLog') }}</span>
        </button>
        <label class="autoscroll">
          <span>{{ t('proxy.autoScroll') }}</span>
          <AppSwitch v-model="autoScroll" />
        </label>
      </div>
      <div ref="logsEl" class="log-body">
        <div v-if="!store.logs.length" class="log-empty">暂无日志</div>
        <div v-for="(l, i) in store.logs" :key="i" class="log-row">
          <span class="log-time">{{ l.at }}</span>
          <span class="log-level" :class="`log-level--${l.level}`">{{ (l.level || '').toUpperCase() }}</span>
          <span class="log-msg">{{ l.message }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.proxy {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}

/* ① 状态条 */
.status-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: var(--space-4) var(--space-5);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
}
.status-card__left {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.pulse {
  width: 12px;
  height: 12px;
  border-radius: var(--radius-full);
  background: var(--text-muted);
  flex-shrink: 0;
}
.pulse--on {
  background: var(--success);
  box-shadow: 0 0 0 4px var(--success-soft);
}
.status-text {
  font-size: var(--fs-lg);
  font-weight: 600;
  color: var(--text-muted);
}
.status-text--on {
  color: var(--success);
}
.status-sub {
  font-size: var(--fs-sm);
  color: var(--text-muted);
}
.status-card__right {
  display: flex;
  align-items: center;
  gap: var(--space-4);
}
.port-field {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.port-label {
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  font-weight: 500;
}
.port-input {
  width: 88px;
  height: 32px;
  padding: 0 var(--space-2);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-md);
  font-family: var(--font-mono);
}
.port-input:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.proxy-toggle {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  height: 34px;
  padding: 0 var(--space-4);
  border: none;
  border-radius: var(--radius);
  font-size: var(--fs-md);
  font-weight: 600;
  color: #fff;
}
.proxy-toggle__icon {
  width: 15px;
  height: 15px;
}
.proxy-toggle.is-stop {
  background: var(--danger);
}
.proxy-toggle.is-start {
  background: var(--success);
}

/* ② 四列统计 */
.stat-grid {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: var(--space-3);
}
.stat-card {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
}
.stat-card__icon {
  display: grid;
  place-items: center;
  width: 34px;
  height: 34px;
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent);
  flex-shrink: 0;
}
.stat-card__icon :deep(svg) {
  width: 17px;
  height: 17px;
}
.stat-card__icon--ok {
  background: var(--success-soft);
  color: var(--success);
}
.stat-card__icon--err {
  background: var(--danger-soft);
  color: var(--danger);
}
.stat-card__value {
  font-size: var(--fs-lg);
  font-weight: 700;
  color: var(--text);
  font-variant-numeric: tabular-nums;
}
.stat-card__label {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  white-space: nowrap;
}

/* ③ 暗色日志面板 */
.log-panel {
  background: #11151f;
  border: 1px solid #232a3a;
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.log-panel__head {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid rgba(255, 255, 255, 0.07);
}
.log-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  padding: 0 var(--space-3);
  border: 1px solid rgba(255, 255, 255, 0.18);
  border-radius: var(--radius);
  background: transparent;
  color: #c8cdd8;
  font-size: var(--fs-sm);
}
.log-btn:hover {
  background: rgba(255, 255, 255, 0.06);
}
.log-btn__icon {
  width: 14px;
  height: 14px;
}
.autoscroll {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  color: #c8cdd8;
  font-size: var(--fs-sm);
}
.log-body {
  max-height: 380px;
  overflow-y: auto;
  padding: var(--space-3) var(--space-4);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  line-height: 1.7;
}
.log-empty {
  color: #6b7280;
  text-align: center;
  padding: var(--space-5) 0;
}
.log-row {
  display: grid;
  grid-template-columns: 72px 70px 1fr;
  gap: var(--space-3);
  white-space: pre-wrap;
  word-break: break-all;
}
.log-time {
  color: #6b7280;
}
.log-level {
  color: #5b9bd5;
  font-weight: 600;
  white-space: nowrap;
}
.log-level--success {
  color: #3fb950;
}
.log-level--warn {
  color: #e0a020;
}
.log-level--error {
  color: #ff6b6b;
}
.log-msg {
  color: #d8dce4;
}
</style>
