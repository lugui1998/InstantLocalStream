<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from 'vue'
import type { BootstrapProgress } from '@/composables/useViewer'

const emit = defineEmits<{ unmute: []; 'live-edge': [video: HTMLVideoElement]; 'frame-rendered': [] }>()
const video = ref<HTMLVideoElement | null>(null)

const props = defineProps<{
  stream: MediaStream | null
  catchUpDelayMs: number | null
  bootstrapProgress: BootstrapProgress | null
}>()
let frameCallback = 0

function playbackRateFor(delayMs: number | null) {
  if (delayMs === null || delayMs < 100) return 1
  if (delayMs >= 1_000) return 1.2
  if (delayMs >= 500) return 1.12
  if (delayMs >= 250) return 1.08
  return 1.04
}

function applyCatchUpRate() {
  const element = video.value
  if (!element) return
  const nextRate = playbackRateFor(props.catchUpDelayMs)
  if (element.playbackRate !== nextRate) {
    element.preservesPitch = true
    element.playbackRate = nextRate
  }
}

function followLiveEdge() {
  const element = video.value
  if (!element) return
  emit('frame-rendered')
  const { seekable } = element
  if (seekable.length > 0) {
    const liveEdge = seekable.end(seekable.length - 1)
    if (Number.isFinite(liveEdge) && liveEdge - element.currentTime > 0.25) {
      emit('live-edge', element)
    }
  }
  if ('requestVideoFrameCallback' in element) {
    frameCallback = element.requestVideoFrameCallback(() => followLiveEdge())
  }
}

watch([video, () => props.stream], ([element, stream]) => {
  if (element) {
    element.srcObject = stream
    if (frameCallback && 'cancelVideoFrameCallback' in element) {
      element.cancelVideoFrameCallback(frameCallback)
      frameCallback = 0
    }
    if (stream && 'requestVideoFrameCallback' in element) {
      frameCallback = element.requestVideoFrameCallback(() => followLiveEdge())
    }
  }
}, { immediate: true })

watch(() => props.catchUpDelayMs, applyCatchUpRate, { immediate: true })

onBeforeUnmount(() => {
  if (video.value && frameCallback && 'cancelVideoFrameCallback' in video.value) {
    video.value.cancelVideoFrameCallback(frameCallback)
  }
  if (video.value) video.value.playbackRate = 1
})
</script>

<template>
  <section class="video-frame" aria-label="Stream">
    <div v-if="!stream && bootstrapProgress" class="video-bootstrap" role="status" aria-live="polite">
      <span class="video-bootstrap-indicator" aria-hidden="true" />
      <div class="video-bootstrap-title">{{ bootstrapProgress.title }}</div>
      <div class="video-bootstrap-detail">{{ bootstrapProgress.detail }}</div>
    </div>
    <video
      ref="video"
      autoplay
      muted
      playsinline
      controls
      @click="emit('unmute')"
      @loadeddata="emit('frame-rendered')"
      @loadedmetadata="video && emit('live-edge', video)"
    />
  </section>
</template>
