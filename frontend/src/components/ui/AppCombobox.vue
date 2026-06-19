<script setup lang="ts">
// 可输入下拉框:保留自由输入(自定义值),同时把 options 作为下拉选项点选即填。
// 选项支持 string 或 {value,label}:显示 label、存 value(如 Gemini 模型显示
// displayName 但存原始 model id)。string 选项等价 {value:s,label:s},向后兼容。
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import IconChevronDown from '~icons/lucide/chevron-down'

type Opt = { value: string; label: string }
const props = withDefaults(
  defineProps<{ options?: Array<string | Opt>; placeholder?: string }>(),
  { options: () => [], placeholder: '' },
)
const model = defineModel<string>({ default: '' })
// 显式点选某选项时触发(区别于自由键入), 供调用方做预填等副作用
const emit = defineEmits<{ select: [value: string] }>()
const open = ref(false)
const root = ref<HTMLElement>()

const opts = computed<Opt[]>(() =>
  (props.options || []).map((o) => (typeof o === 'string' ? { value: o, label: o } : o)),
)
// 输入框显示文本:选中项显示其 label, 否则显示原始值(自定义/未匹配)
const text = ref('')
function syncText() {
  text.value = opts.value.find((o) => o.value === model.value)?.label ?? model.value
}
// model 外部变化(预填/缓存)或 options(label)到达后刷新显示文本
watch([() => model.value, opts], syncText, { immediate: true })

// 输入为空 / 文本等于某选项 label → 列全部;键入部分文本时按 label 过滤
const filtered = computed(() => {
  const q = text.value.trim().toLowerCase()
  if (!q || opts.value.some((o) => o.label.toLowerCase() === q)) return opts.value
  return opts.value.filter((o) => o.label.toLowerCase().includes(q))
})

function onInput(e: Event) {
  const v = (e.target as HTMLInputElement).value
  text.value = v
  model.value = v // 自由输入 = 直接作为值
  if (opts.value.length) open.value = true
}
function pick(o: Opt) {
  model.value = o.value
  text.value = o.label
  open.value = false
  emit('select', o.value)
}
function onFocus() {
  if (opts.value.length) open.value = true
}
function onDocPointer(e: PointerEvent) {
  if (open.value && root.value && !root.value.contains(e.target as Node)) open.value = false
}
function onKey(e: KeyboardEvent) {
  if (open.value && e.key === 'Escape') open.value = false
}
onMounted(() => {
  document.addEventListener('pointerdown', onDocPointer)
  document.addEventListener('keydown', onKey)
})
onUnmounted(() => {
  document.removeEventListener('pointerdown', onDocPointer)
  document.removeEventListener('keydown', onKey)
})
</script>

<template>
  <div ref="root" class="combo">
    <div class="combo__field" :class="{ open }">
      <input
        :value="text"
        class="combo__input"
        :placeholder="placeholder"
        autocomplete="off"
        spellcheck="false"
        @input="onInput"
        @focus="onFocus"
      />
      <button
        type="button"
        class="combo__chevron"
        :class="{ open }"
        :disabled="!opts.length"
        :aria-label="open ? 'collapse' : 'expand'"
        @click="open = !open"
      >
        <IconChevronDown />
      </button>
    </div>
    <div v-if="open && filtered.length" class="combo__panel">
      <button
        v-for="o in filtered"
        :key="o.value"
        type="button"
        class="combo__option"
        :class="{ sel: o.value === model }"
        @click="pick(o)"
      >
        {{ o.label }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.combo {
  position: relative;
  width: 260px;
  max-width: 100%;
}
.combo__field {
  display: flex;
  align-items: center;
  width: 100%;
  height: 30px;
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  transition: border-color var(--transition), box-shadow var(--transition);
}
.combo__field:focus-within,
.combo__field.open {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.combo__input {
  flex: 1;
  min-width: 0;
  height: 100%;
  padding: 0 var(--space-3);
  border: none;
  border-radius: var(--radius);
  background: transparent;
  color: var(--text);
  font-size: var(--fs-base);
  font-family: inherit;
}
.combo__input:focus {
  outline: none;
}
.combo__input::placeholder {
  color: var(--text-muted);
}
.combo__chevron {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 100%;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--text-muted);
  cursor: pointer;
}
.combo__chevron:disabled {
  cursor: default;
  opacity: 0.4;
}
.combo__chevron svg {
  width: 14px;
  height: 14px;
  transition: transform var(--transition);
}
.combo__chevron.open svg {
  transform: rotate(180deg);
}
.combo__panel {
  position: absolute;
  top: calc(100% + 2px);
  left: 0;
  z-index: 100;
  width: 100%;
  max-height: 220px;
  overflow-y: auto;
  padding: var(--space-1);
  background: var(--surface);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  box-shadow: var(--shadow-md);
}
.combo__option {
  display: block;
  width: 100%;
  padding: var(--space-2);
  border: none;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  text-align: left;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  cursor: pointer;
}
.combo__option:hover {
  background: var(--surface-hover);
}
.combo__option.sel {
  color: var(--accent);
  font-weight: 600;
}
</style>
