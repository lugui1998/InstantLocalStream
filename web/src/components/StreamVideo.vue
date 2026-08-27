<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from 'vue'
import type { BootstrapProgress } from '@/composables/useViewer'
import type { RenderedFrameTiming } from '@/types'

const emit = defineEmits<{ unmute: []; 'live-edge': [video: HTMLVideoElement]; 'frame-rendered': [timing?: RenderedFrameTiming] }>()
const video = ref<HTMLVideoElement | null>(null)

const props = defineProps<{
  stream: MediaStream | null
  audioEnabled: boolean
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

function requestUnmute() {
  const hasLiveAudio = props.stream?.getAudioTracks().some(track => track.readyState === 'live')
  if (props.audioEnabled && hasLiveAudio) emit('unmute')
}

type WebRtcFrameMetadata = VideoFrameCallbackMetadata & {
  captureTime?: number
  receiveTime?: number
  rtpTimestamp?: number
}

function followLiveEdge(_now: number, metadata: WebRtcFrameMetadata) {
  const element = video.value
  if (!element) return
  emit('frame-rendered', {
    expectedDisplayTimeMs: metadata.expectedDisplayTime,
    presentationTimeMs: metadata.presentationTime,
    ...(typeof metadata.captureTime === 'number' ? { captureTimeMs: metadata.captureTime } : {}),
    ...(typeof metadata.receiveTime === 'number' ? { receiveTimeMs: metadata.receiveTime } : {}),
    ...(typeof metadata.processingDuration === 'number' ? { processingDurationMs: metadata.processingDuration * 1_000 } : {}),
    ...(typeof metadata.rtpTimestamp === 'number' ? { rtpTimestamp: metadata.rtpTimestamp } : {}),
  })
  const { seekable } = element
  if (seekable.length > 0) {
    const liveEdge = seekable.end(seekable.length - 1)
    if (Number.isFinite(liveEdge) && liveEdge - element.currentTime > 0.25) {
      emit('live-edge', element)
    }
  }
  if ('requestVideoFrameCallback' in element) {
    frameCallback = element.requestVideoFrameCallback((now, nextMetadata) => followLiveEdge(now, nextMetadata))
  }
}

watch([video, () => props.stream], ([element, stream]) => {
  if (element) {
    // A rebuilt peer may now carry audio.  Stay muted at attachment time so
    // autoplay remains reliable, but do not disable native controls: once an
    // audio track arrives the browser enables its volume UI for the viewer.
    element.muted = true
    element.srcObject = stream
    if (frameCallback && 'cancelVideoFrameCallback' in element) {
      element.cancelVideoFrameCallback(frameCallback)
      frameCallback = 0
    }
    if (stream && 'requestVideoFrameCallback' in element) {
      frameCallback = element.requestVideoFrameCallback((now, metadata) => followLiveEdge(now, metadata))
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
      @click="requestUnmute"
      @loadeddata="emit('frame-rendered')"
      @loadedmetadata="video && emit('live-edge', video)"
    />
  </section>
</template>
