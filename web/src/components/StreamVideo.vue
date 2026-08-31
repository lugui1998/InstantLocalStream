<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import type { BootstrapProgress } from '@/composables/useViewer'
import type { RenderedFrameTiming } from '@/types'
import { playbackRateFor } from '@/viewerUtils'

const emit = defineEmits<{
  'live-edge': [video: HTMLVideoElement]
  'frame-rendered': [timing?: RenderedFrameTiming]
  'video-element': [video: HTMLVideoElement | null]
  'playback-error': []
  'playback-started': []
}>()
const video = ref<HTMLVideoElement | null>(null)

const props = defineProps<{
  stream: MediaStream | null
  catchUpDelayMs: number | null
  bootstrapProgress: BootstrapProgress | null
}>()
let frameCallback = 0
let renderedVideoSize = ''

function refreshVideoDimensions() {
  const element = video.value
  if (!element || element.videoWidth <= 0 || element.videoHeight <= 0) return
  const size = `${element.videoWidth}x${element.videoHeight}`
  if (renderedVideoSize === size) return
  renderedVideoSize = size

  // WebRTC can change the decoded frame size without replacing the MediaStream.
  // Keep the element's intrinsic canvas in sync so Chromium does not retain the
  // old surface when a captured window grows or shrinks.
  element.width = element.videoWidth
  element.height = element.videoHeight
  element.style.aspectRatio = `${element.videoWidth} / ${element.videoHeight}`
}

function applyCatchUpRate() {
  const element = video.value
  if (!element) return
  const hasLiveAudio = props.stream?.getAudioTracks().some(track => track.readyState === 'live') === true
  const nextRate = playbackRateFor(props.catchUpDelayMs, hasLiveAudio)
  if (element.playbackRate !== nextRate) {
    element.preservesPitch = true
    element.playbackRate = nextRate
  }
}

function reportPlayFailure() {
  emit('playback-error')
}

type WebRtcFrameMetadata = VideoFrameCallbackMetadata & {
  captureTime?: number
  receiveTime?: number
  rtpTimestamp?: number
}

function followLiveEdge(_now: number, metadata: WebRtcFrameMetadata) {
  const element = video.value
  if (!element) return
  // Some Chromium/WebRTC paths update videoWidth/videoHeight on the decoded
  // frame without dispatching the media element's resize event. Sampling the
  // intrinsic size at the render boundary catches that case before the next
  // paint.
  refreshVideoDimensions()
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
    // Preserve an explicit user unmute if a later track replaces only the
    // wrapper MediaStream (for example, audio-first delivery followed by video).
    const hasLiveAudio = stream?.getAudioTracks().some(track => track.readyState === 'live') === true
    const preserveUserUnmute = !element.muted && hasLiveAudio
    element.muted = !preserveUserUnmute
    element.srcObject = stream
    applyCatchUpRate()
    renderedVideoSize = ''
    if (stream) refreshVideoDimensions()
    if (stream) void element.play().catch(reportPlayFailure)
    if (frameCallback && 'cancelVideoFrameCallback' in element) {
      element.cancelVideoFrameCallback(frameCallback)
      frameCallback = 0
    }
    if (!stream) {
      element.width = 0
      element.height = 0
      element.style.removeProperty('aspect-ratio')
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
  emit('video-element', null)
})

onMounted(() => emit('video-element', video.value))
</script>

<template>
  <section class="video-frame" aria-label="Stream">
    <div v-if="!stream && bootstrapProgress" class="video-bootstrap" role="status" aria-live="polite">
      <span class="video-bootstrap-indicator" aria-hidden="true" />
      <div class="video-bootstrap-title">{{ bootstrapProgress.title }}</div>
      <div v-if="bootstrapProgress.detail" class="video-bootstrap-detail">{{ bootstrapProgress.detail }}</div>
    </div>
    <video
      ref="video"
      autoplay
      muted
      playsinline
      controls
      @error="reportPlayFailure"
      @playing="emit('playback-started')"
      @loadeddata="emit('frame-rendered')"
      @loadedmetadata="refreshVideoDimensions(); video && emit('live-edge', video)"
      @resize="refreshVideoDimensions"
    />
  </section>
</template>
