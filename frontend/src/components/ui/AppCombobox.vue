<script setup lang="ts">
// 可输入下拉框:保留自由输入(自定义模型 id),同时把 options(获取到的模型)
// 作为下拉选项,点选即填入。面板锚定输入框下沿、固定高度可滚,点外/Esc 关闭。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import IconChevronDown from '~icons/lucide/chevron-down'

const props = withDefaults(
  defineProps<{ options?: string[]; placeholder?: string }>(),
  { options: () => [], placeholder: '' },
)
const model = defineModel<string>({ default: '' })
// 显式点选某选项时触发(区别于自由键入), 供调用方做预填等副作用
const emit = defineEmits<{ select: [value: string] }>()
const open = ref(false)
const root = ref<HTMLElement>()

// 输入为空 / 已选中某选项时列全部;正在键入部分文本时按其过滤候选
const filtered = computed(() => {
  const q = model.value.trim().toLowerCase()
  if (!q || props.options.some((o) => o.toLowerCase() === q)) return props.options
  return props.options.filter((o) => o.toLowerCase().includes(q))
})

function pick(o: string) {
  model.value = o
  open.value = false
  emit('select', o)
}
function onFocus() {
  if (props.options.length) open.value = true
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
        v-model="model"
        class="combo__input"
        :placeholder="placeholder"
        autocomplete="off"
        spellcheck="false"
        @focus="onFocus"
      />
      <button
        type="button"
        class="combo__chevron"
        :class="{ open }"
        :disabled="!options.length"
        :aria-label="open ? 'collapse' : 'expand'"
        @click="open = !open"
      >
        <IconChevronDown />
      </button>
    </div>
    <div v-if="open && filtered.length" class="combo__panel">
      <button
        v-for="o in filtered"
        :key="o"
        type="button"
        class="combo__option"
        :class="{ sel: o === model }"
        @click="pick(o)"
      >
        {{ o }}
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
