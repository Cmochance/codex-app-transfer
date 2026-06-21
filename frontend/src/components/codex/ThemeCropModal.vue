<script setup lang="ts">
// 16:9 crop 弹窗 — 全屏暗背景 + 中央舞台显示原图,叠居中 16:9 选区:拖动调位置、
// 滚轮缩放、确认 → canvas drawImage 缩到 16:9 JPEG(宽 ≤2048)。Codex Desktop 背景是
// 宽屏,16:9 让裁切预览与实际背景一致(扩充自 #522)。自建全屏 overlay(非 AppModal):
// 需要暗底 + 自定义舞台/选区,与设置型 modal 形态不同。
import { onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { t } from '@/i18n'
import { useToast } from '@/composables/useToast'
import AppButton from '@/components/ui/AppButton.vue'

// 选区宽高比 16:9(高 = 宽 / ASPECT)。
const ASPECT = 16 / 9

const props = defineProps<{ src: string }>()
const emit = defineEmits<{ confirm: [dataUri: string]; cancel: [] }>()
const { show: toast } = useToast()

const imgEl = ref<HTMLImageElement>()
const stageEl = ref<HTMLDivElement>()
const ready = ref(false)

// box 状态(相对 stage 像素 = 显示坐标);非响应,经 applyBox() 推到响应式 boxStyle。
// 只存宽 boxW,高由 boxW / ASPECT 派生,保证恒为 16:9。
let boxX = 0
let boxY = 0
let boxW = 0
let stageW = 0
let stageH = 0
const boxStyle = reactive({ left: '0px', top: '0px', width: '0px', height: '0px' })

function clampBox() {
  // 宽上限:既不超 stage 宽,也不让派生高超 stage 高(boxW/ASPECT ≤ stageH)
  const maxW = Math.min(stageW, stageH * ASPECT)
  if (boxW > maxW) boxW = maxW
  if (boxW < 80) boxW = Math.min(80, maxW)
  let boxH = boxW / ASPECT
  if (boxX < 0) boxX = 0
  if (boxY < 0) boxY = 0
  if (boxX + boxW > stageW) boxX = stageW - boxW
  if (boxY + boxH > stageH) boxY = stageH - boxH
}
function applyBox() {
  clampBox()
  boxStyle.left = `${boxX}px`
  boxStyle.top = `${boxY}px`
  boxStyle.width = `${boxW}px`
  boxStyle.height = `${boxW / ASPECT}px`
}

function onImgLoad() {
  const img = imgEl.value
  if (!img) return
  stageW = img.offsetWidth
  stageH = img.offsetHeight
  boxW = Math.min(stageW, stageH * ASPECT) * 0.9
  const boxH = boxW / ASPECT
  boxX = (stageW - boxW) / 2
  boxY = (stageH - boxH) / 2
  applyBox()
  ready.value = true
}
function onImgError() {
  toast(`${t('theme.uploadFailed')}: ${t('theme.uploadDecodeFailed')}`, 'error')
  emit('cancel')
}

// 拖动 + 滚轮缩放 — window 级 listener 在 unmount 清理(防多次打开累积 leak)
let dragging = false
let dragOX = 0
let dragOY = 0
function onMouseDown(e: MouseEvent) {
  if (!stageEl.value) return
  dragging = true
  const r = stageEl.value.getBoundingClientRect()
  dragOX = e.clientX - r.left - boxX
  dragOY = e.clientY - r.top - boxY
  e.preventDefault()
}
function onMouseMove(e: MouseEvent) {
  if (!dragging || !stageEl.value) return
  const r = stageEl.value.getBoundingClientRect()
  boxX = e.clientX - r.left - dragOX
  boxY = e.clientY - r.top - dragOY
  applyBox()
}
function onMouseUp() {
  dragging = false
}
function onWheel(e: WheelEvent) {
  e.preventDefault()
  const boxH = boxW / ASPECT
  const cx = boxX + boxW / 2
  const cy = boxY + boxH / 2
  boxW *= e.deltaY < 0 ? 1.05 : 0.95
  boxX = cx - boxW / 2
  boxY = cy - boxW / ASPECT / 2
  applyBox()
}

onMounted(() => {
  window.addEventListener('mousemove', onMouseMove)
  window.addEventListener('mouseup', onMouseUp)
})
onBeforeUnmount(() => {
  window.removeEventListener('mousemove', onMouseMove)
  window.removeEventListener('mouseup', onMouseUp)
})

function onConfirm() {
  const img = imgEl.value
  if (!img || !ready.value) return
  // 显示坐标 → 原图坐标
  const scaleX = img.naturalWidth / stageW
  const scaleY = img.naturalHeight / stageH
  const boxH = boxW / ASPECT
  const sx = boxX * scaleX
  const sy = boxY * scaleY
  const sw = boxW * scaleX
  const sh = boxH * scaleY
  // 输出恒为 16:9;宽不放大(≤2048 且 ≤ 选区原图宽),高按比例派生。
  const outW = Math.min(2048, Math.round(sw))
  const outH = Math.round(outW / ASPECT)
  const canvas = document.createElement('canvas')
  canvas.width = outW
  canvas.height = outH
  const ctx = canvas.getContext('2d')
  if (!ctx) {
    // canvas 2d 上下文不可用(webview 里近乎不可能)— 对齐 onImgError,surface + 关闭。
    toast(`${t('theme.uploadFailed')}: ${t('theme.uploadDecodeFailed')}`, 'error')
    emit('cancel')
    return
  }
  ctx.imageSmoothingQuality = 'high'
  ctx.drawImage(img, sx, sy, sw, sh, 0, 0, outW, outH)
  emit('confirm', canvas.toDataURL('image/jpeg', 0.92))
}
</script>

<template>
  <Teleport to="body">
    <div class="crop-overlay" @click.self="emit('cancel')">
      <div class="crop-panel">
        <div class="crop-title">{{ t('theme.cropTitle') }}</div>
        <div ref="stageEl" class="crop-stage" @mousedown="onMouseDown" @wheel="onWheel">
          <img
            ref="imgEl"
            class="crop-img"
            :src="props.src"
            alt=""
            @load="onImgLoad"
            @error="onImgError"
          />
          <div class="crop-box" :style="boxStyle" />
        </div>
        <div class="crop-actions">
          <AppButton variant="ghost" size="sm" :label="t('common.cancel')" @click="emit('cancel')" />
          <AppButton variant="primary" size="sm" :disabled="!ready" :label="t('theme.cropConfirm')" @click="onConfirm" />
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.crop-overlay {
  position: fixed;
  inset: 0;
  z-index: 1100;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-direction: column;
  background: rgba(0, 0, 0, 0.78);
}
.crop-panel {
  background: #1a1a1a;
  border: 1px solid #444;
  border-radius: 12px;
  padding: 18px;
  max-width: 90vw;
  max-height: 90vh;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.crop-title {
  color: #eee;
  font-size: 15px;
  font-weight: 600;
}
.crop-stage {
  position: relative;
  display: inline-block;
  background: #000;
  border-radius: 6px;
  overflow: hidden;
  cursor: move;
  user-select: none;
  line-height: 0;
}
.crop-img {
  display: block;
  max-width: 70vw;
  max-height: 65vh;
  width: auto;
  height: auto;
  pointer-events: none;
}
.crop-box {
  position: absolute;
  border: 2px solid rgba(255, 255, 255, 0.95);
  box-shadow: 0 0 0 9999px rgba(0, 0, 0, 0.55);
  box-sizing: border-box;
  pointer-events: none;
}
.crop-actions {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
}
</style>
