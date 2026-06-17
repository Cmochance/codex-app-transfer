<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useProvidersStore } from '@/stores/providers'
import { t } from '@/i18n'
import ProviderCard from '@/components/provider/ProviderCard.vue'
import AppButton from '@/components/ui/AppButton.vue'
import IconPlus from '~icons/lucide/plus'

const store = useProvidersStore()
const router = useRouter()
onMounted(() => store.load())

// 诉求2: HTML5 拖拽排序(复刻旧 enableProviderReorder 语义), drop 后乐观更新 + 持久化
const draggingId = ref<string | null>(null)
function onDragStart(id: string) {
  draggingId.value = id
}
function onDragEnd() {
  draggingId.value = null
}
function onDrop(targetId: string) {
  const from = draggingId.value
  draggingId.value = null
  if (!from || from === targetId) return
  const ids = store.list.map((p) => p.id)
  const fi = ids.indexOf(from)
  const ti = ids.indexOf(targetId)
  if (fi < 0 || ti < 0) return
  ids.splice(ti, 0, ids.splice(fi, 1)[0])
  store.reorder(ids)
}

function onEdit(id: string) {
  router.push({ path: '/providers/add', query: { id } })
}
function onRemove(id: string) {
  if (window.confirm('确认删除该提供商？')) store.remove(id)
}
</script>

<template>
  <div class="providers">
    <div class="providers__header">
      <h1 class="providers__title">{{ t('nav.providers') }}</h1>
      <AppButton
        variant="primary"
        size="sm"
        :icon="IconPlus"
        :label="t('providers.add')"
        @click="router.push('/providers/add')"
      />
    </div>

    <div v-if="store.loading" class="providers__hint">加载中…</div>
    <div v-else-if="store.error" class="providers__hint providers__hint--err">{{ store.error }}</div>
    <div v-else-if="!store.list.length" class="providers__hint">暂无提供商，点击右上角添加</div>

    <div v-else class="providers__list">
      <div
        v-for="p in store.list"
        :key="p.id"
        class="providers__item"
        :class="{ 'is-dragging': draggingId === p.id }"
        draggable="true"
        @dragstart="onDragStart(p.id)"
        @dragend="onDragEnd"
        @dragover.prevent
        @drop="onDrop(p.id)"
      >
        <ProviderCard
          :provider="p"
          @enable="store.setDefault(p.id)"
          @edit="onEdit(p.id)"
          @remove="onRemove(p.id)"
        />
      </div>
    </div>
  </div>
</template>

<style scoped>
.providers__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: var(--space-4);
}
.providers__title {
  font-size: var(--fs-xl);
  font-weight: 600;
}
.providers__list {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.providers__item {
  transition: opacity var(--transition);
}
.is-dragging {
  opacity: 0.4;
}
.providers__hint {
  color: var(--text-muted);
  padding: var(--space-6) 0;
  text-align: center;
}
.providers__hint--err {
  color: var(--danger);
}
</style>
