<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import DroppedFramesChart from '@/components/DroppedFramesChart.vue'
import StreamVideo from '@/components/StreamVideo.vue'
import { useViewer } from '@/composables/useViewer'
import type { GroupAssignment } from '@/types'
import { diagnosticRecordingLimitMs, preferredRecordingMimeType } from '@/viewerUtils'

const {
  videoStream,
  status,
  mediaStatus,
  rttMs,
  jitterMs,
  bitrateBps,
  lossRate,
  framesDropped,
  droppedFrameSamples,
  freezeCount,
  jitterBufferDelayMs,
  audioPacketsLost,
  audioJitterMs,
  audioConcealmentEvents,
  audioConcealedSamples,
  audioInsertedSamplesForDeceleration,
  audioRemovedSamplesForAcceleration,
  audioJitterBufferDelayMs,
  catchUpDelayMs,
  playoutDelayMs,
  captureToDisplayDelayMs,
  captureToReceiveDelayMs,
  receiveToDisplayDelayMs,
  frameProcessingDelayMs,
  frameDelayMode,
  frameTimingUncertaintyMs,
  encoderDelayMs,
  decodeTimeMs,
  group,
  activeCodec,
  bootstrapProgress,
  synchronizationMode,
  viewers,
  quality,
  start,
  setVideoElement,
  reportPlaybackError,
  reportPlaybackStarted,
  seekToLiveEdge,
  noteVideoFrameRendered,
} = useViewer()

const recording = ref(false)
const recordingSeconds = ref(0)
const recordingError = ref<string | null>(null)
const maxRecordingSeconds = diagnosticRecordingLimitMs / 1_000
let recorder: MediaRecorder | null = null
let recordingChunks: Blob[] = []
let recordingTimer: number | null = null
let recordingStartedAt = 0
let recordingTrackIds = ''
let exportRecordingOnStop = true
let disposed = false
let recordingTrackEndListeners: Array<{ track: MediaStreamTrack, listener: () => void }> = []

const mediaRecorderSupported = typeof MediaRecorder !== 'undefined'

function formatCodecProfile(profile: GroupAssignment) {
  const codec = profile.codec ?? '—'
  const level = profile.h264_level ?? profile.h265_level
  return level ? `${codec} · Level ${level}` : codec
}

const recordingReady = computed(() => {
  if (!mediaRecorderSupported) return false
  const stream = videoStream.value
  return stream?.getVideoTracks().some(track => track.readyState === 'live') === true
    && stream.getAudioTracks().some(track => track.readyState === 'live')
})

const recordingStatus = computed(() => {
  if (recording.value) {
    return `Recording received stream · ${recordingSeconds.value}s / ${maxRecordingSeconds}s…`
  }
  if (recordingError.value) return recordingError.value
  if (!mediaRecorderSupported) return 'MediaRecorder is unavailable in this browser.'
  if (!recordingReady.value) return 'Waiting for live video and audio tracks'
  return 'Ready'
})

function clearRecordingTimer() {
  if (recordingTimer !== null) window.clearInterval(recordingTimer)
  recordingTimer = null
}

function downloadRecording(blob: Blob) {
  const extension = blob.type.includes('mp4') ? 'mp4' : 'webm'
  const timestamp = new Date().toISOString().replace(/[:.]/g, '-')
  const url = URL.createObjectURL(blob)
  const link = document.createElement('a')
  link.href = url
  link.download = `instant-local-stream-received-test-${timestamp}.${extension}`
  link.hidden = true
  document.body.appendChild(link)
  link.click()
  link.remove()
  window.setTimeout(() => URL.revokeObjectURL(url), 30_000)
}

function clearRecordingTrackEndListeners() {
  recordingTrackEndListeners.forEach(({ track, listener }) => track.removeEventListener('ended', listener))
  recordingTrackEndListeners = []
}

function beginRecording(recordingStream: MediaStream) {
  recordingError.value = null
  if (disposed || recording.value || recorder) {
    recordingError.value = 'Another diagnostic recording is already active.'
    return false
  }
  const tracks = recordingStream.getTracks().filter(track => track.readyState === 'live')
  const hasVideo = tracks.some(track => track.kind === 'video')
  const hasAudio = tracks.some(track => track.kind === 'audio')
  if (!hasVideo || !hasAudio) {
    recordingError.value = 'A live video track and test-tone audio track are required.'
    return false
  }

  const mimeType = preferredRecordingMimeType(type => MediaRecorder.isTypeSupported(type))
  const options: MediaRecorderOptions = {
    videoBitsPerSecond: 12_000_000,
    audioBitsPerSecond: 256_000,
    ...(mimeType ? { mimeType } : {}),
  }
  try {
    recorder = new MediaRecorder(recordingStream, options)
  } catch {
    try {
      recorder = new MediaRecorder(recordingStream)
    } catch (error) {
      recorder = null
      recordingError.value = error instanceof Error ? error.message : 'This browser cannot record the stream.'
      return false
    }
  }

  recordingChunks = []
  recordingStartedAt = Date.now()
  recordingTrackIds = tracks.map(track => track.id).sort().join(':')
  exportRecordingOnStop = true
  const activeRecorder = recorder
  activeRecorder.addEventListener('dataavailable', (event) => {
    if (event.data.size > 0) recordingChunks.push(event.data)
  })
  activeRecorder.addEventListener('error', (event) => {
    const mediaError = (event as Event & { error?: DOMException }).error
    recordingError.value = mediaError?.message ?? 'The browser recording failed.'
  })
  activeRecorder.addEventListener('stop', () => {
    clearRecordingTimer()
    clearRecordingTrackEndListeners()
    recording.value = false
    recordingSeconds.value = Math.max(0, Math.round((Date.now() - recordingStartedAt) / 1_000))
    if (exportRecordingOnStop && recordingChunks.length > 0) {
      downloadRecording(new Blob(recordingChunks, {
        type: activeRecorder.mimeType || mimeType || 'video/webm',
      }))
    } else if (exportRecordingOnStop) {
      recordingError.value = 'The browser did not produce recording data.'
    }
    recorder = null
    recordingChunks = []
    recordingTrackIds = ''
  }, { once: true })
  try {
    activeRecorder.start(1_000)
  } catch (error) {
    recorder = null
    recordingChunks = []
    recordingTrackIds = ''
    recordingError.value = error instanceof Error ? error.message : 'The browser could not start recording.'
    return false
  }
  tracks.forEach((track) => {
    const listener = () => {
      if (recorder === activeRecorder) stopDiagnosticRecording()
    }
    track.addEventListener('ended', listener, { once: true })
    recordingTrackEndListeners.push({ track, listener })
  })
  recording.value = true
  recordingSeconds.value = 0
  recordingTimer = window.setInterval(() => {
    recordingSeconds.value = Math.max(0, Math.floor((Date.now() - recordingStartedAt) / 1_000))
    if (recordingSeconds.value >= maxRecordingSeconds) stopDiagnosticRecording()
  }, 250)
  return true
}

function startReceivedStreamRecording() {
  if (recording.value) return
  const source = videoStream.value
  if (!recordingReady.value || !source) {
    recordingError.value = 'A live video track and test-tone audio track are required.'
    return
  }
  beginRecording(new MediaStream(source.getTracks()))
}

function stopDiagnosticRecording() {
  if (recorder?.state !== 'inactive') recorder?.stop()
}

watch(videoStream, (stream) => {
  if (!recording.value) return
  const currentTrackIds = stream?.getTracks().map(track => track.id).sort().join(':') ?? ''
  if (currentTrackIds !== recordingTrackIds) {
    recordingError.value = 'The WebRTC stream changed; the partial recording was exported.'
    stopDiagnosticRecording()
  }
})

watch(() => status.value.diagnostics_enabled, (enabled) => {
  if (enabled === false && recording.value) stopDiagnosticRecording()
})

onBeforeUnmount(() => {
  disposed = true
  exportRecordingOnStop = false
  clearRecordingTimer()
  clearRecordingTrackEndListeners()
  if (recorder && recorder.state !== 'inactive') recorder.stop()
})

function formatDelay() {
  if (captureToDisplayDelayMs.value !== null) {
    const recoveries = status.value.encoder_backlog_restarts
      ? ` / encoder recoveries ${status.value.encoder_backlog_restarts}`
      : ''
    const network = captureToReceiveDelayMs.value === null
      ? ''
      : ` / to receiver ${Math.round(captureToReceiveDelayMs.value)} ms`
    const receiver = receiveToDisplayDelayMs.value === null
      ? ''
      : ` / receiver ${Math.round(receiveToDisplayDelayMs.value)} ms`
    const processing = frameProcessingDelayMs.value === null
      ? ''
      : ` / processing ${Math.round(frameProcessingDelayMs.value)} ms`
    const prefix = frameDelayMode.value === 'host-correlated' ? '' : '~'
    const uncertainty = frameDelayMode.value === 'host-correlated'
      && frameTimingUncertaintyMs.value !== null
      ? ` ±${Math.max(1, Math.round(frameTimingUncertaintyMs.value))} ms`
      : ' ms'
    return `${prefix}${Math.round(captureToDisplayDelayMs.value)}${uncertainty} capture→display${network}${receiver}${processing}${recoveries}`
  }
  const encoder = encoderDelayMs.value ?? status.value.encoder_delay_ms ?? null
  const browserPlayout = playoutDelayMs.value
    ?? (jitterBufferDelayMs.value === null
      ? null
      : jitterBufferDelayMs.value + (decodeTimeMs.value ?? 0))
  if (encoder !== null) {
    // The sender-timeline fallback already includes transport when available;
    // adding half RTT would count the network path twice.
    const total = encoder + (browserPlayout ?? 0)
    const playout = browserPlayout === null ? '' : ` / receiver path ${Math.round(browserPlayout)} ms`
    const recoveries = status.value.encoder_backlog_restarts
      ? ` / encoder recoveries ${status.value.encoder_backlog_restarts}`
      : ''
    return `~${Math.round(total)} ms estimate / host ${Math.round(encoder)} ms${playout}${recoveries}`
  }
  if (playoutDelayMs.value !== null) {
    const rtt = rttMs.value === null ? '' : ` / RTT ${Math.round(rttMs.value)} ms`
    return `${Math.round(playoutDelayMs.value)} ms media${rtt}`
  }
  if (jitterBufferDelayMs.value !== null) {
    const decode = decodeTimeMs.value === null ? 0 : decodeTimeMs.value
    const rtt = rttMs.value === null ? '' : ` / RTT ${Math.round(rttMs.value)} ms`
    return `${Math.round(jitterBufferDelayMs.value + decode)} ms playout buffer${rtt}`
  }
  if (rttMs.value === null) return 'Measuring…'
  const jitter = jitterMs.value === null ? '' : ` / jitter ${Math.round(jitterMs.value)} ms`
  return `${Math.round(rttMs.value / 2)} ms control est. / RTT ${Math.round(rttMs.value)} ms${jitter}`
}

function delayLabel() {
  if (captureToDisplayDelayMs.value === null) return 'Estimated delay'
  return frameDelayMode.value === 'host-correlated'
    ? 'Capture → display'
    : 'Frame delay estimate'
}

function delayModeLabel() {
  if (frameDelayMode.value === 'host-correlated') return 'Host-correlated'
  if (frameDelayMode.value === 'browser-estimated') return 'Browser estimate'
  return 'Measuring'
}

function delayModeHint() {
  if (frameDelayMode.value === 'host-correlated') {
    return 'Matched to the host capture clock; uncertainty reflects timing round-trip variation.'
  }
  if (frameDelayMode.value === 'browser-estimated') {
    return 'Estimated from browser frame timing when host-correlated timing is unavailable.'
  }
  return 'Delay measurement is still starting.'
}

function formatNetwork() {
  if (bitrateBps.value === null) return 'Measuring…'
  const bitrate = bitrateBps.value >= 1_000_000
    ? `${(bitrateBps.value / 1_000_000).toFixed(1)} Mbps`
    : `${Math.round(bitrateBps.value / 1_000)} kbps`
  const loss = lossRate.value === null ? '—' : `${(lossRate.value * 100).toFixed(1)}% loss`
  return `${bitrate} · ${loss}`
}

function formatMilliseconds(value: number | null) {
  return value === null ? '—' : `${Math.round(value)} ms`
}

function formatBitrate(value: number) {
  return value >= 1_000_000 ? `${(value / 1_000_000).toFixed(1)} Mbps` : `${Math.round(value / 1_000)} kbps`
}

onMounted(start)
</script>

<template>
  <main>
    <StreamVideo :stream="videoStream" :catch-up-delay-ms="catchUpDelayMs" :bootstrap-progress="bootstrapProgress" @video-element="setVideoElement" @playback-error="reportPlaybackError" @playback-started="reportPlaybackStarted" @live-edge="seekToLiveEdge" @frame-rendered="noteVideoFrameRendered" />

    <p v-if="mediaStatus" class="media-status" role="status" aria-live="polite">{{ mediaStatus }}</p>

    <section class="data-grid" aria-label="Live stream metrics">
      <div class="data-cell">
        <div class="meta-label" title="Quality selected by the host for this viewer">Assigned target</div>
        <div class="meta-value">{{ quality }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label" :title="delayModeHint()">{{ delayLabel() }}</div>
        <div class="meta-value">{{ formatDelay() }}</div>
        <div class="metric-context" :title="delayModeHint()">{{ delayModeLabel() }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label" title="Throughput and loss measured by this browser">Measured receive</div>
        <div class="meta-value">{{ formatNetwork() }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Viewers</div>
        <div class="meta-value">{{ viewers }}</div>
      </div>
    </section>

    <details v-if="status.diagnostics_enabled !== false" class="diagnostics">
      <summary>
        <span>Diagnostics</span>
        <span class="diagnostics-summary">Playback, decoder, group, and synchronization details</span>
      </summary>
      <div class="diagnostics-content">
        <section class="recording-panel" aria-label="Diagnostic stream recording">
          <div class="recording-actions">
            <button
              v-if="recording"
              class="recording-button"
              type="button"
              @click="stopDiagnosticRecording"
            >
              Stop & export
            </button>
            <button
              v-else
              class="recording-button"
              type="button"
              :disabled="!recordingReady"
              @click="startReceivedStreamRecording"
            >
              Record received stream
            </button>
          </div>
          <div v-if="recording || recordingError" class="recording-status" :class="{ error: recordingError }" aria-live="polite">{{ recordingStatus }}</div>
        </section>

        <section class="data-grid diagnostics-grid" aria-label="Detailed stream diagnostics">
          <div class="data-cell">
            <div class="meta-label">Group</div>
            <div class="meta-value">{{ group ? `${group.label} · ${group.state}` : 'Unassigned' }}</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Codec</div>
            <div class="meta-value">{{ activeCodec }}</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Synchronization</div>
            <div class="meta-value">{{ synchronizationMode }}</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Playback · 15s</div>
            <div class="meta-value">{{ framesDropped ?? '—' }} dropped · {{ freezeCount ?? '—' }} freezes</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Buffer / decode</div>
            <div class="meta-value">{{ formatMilliseconds(jitterBufferDelayMs) }} / {{ formatMilliseconds(decodeTimeMs) }}</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Audio receive</div>
            <div class="meta-value">{{ audioPacketsLost ?? '—' }} lost · {{ formatMilliseconds(audioJitterMs) }} jitter</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Audio concealment</div>
            <div class="meta-value">{{ audioConcealedSamples ?? '—' }} samples · {{ audioConcealmentEvents ?? '—' }} events</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Audio pacing</div>
            <div class="meta-value">+{{ audioInsertedSamplesForDeceleration ?? '—' }} inserted · −{{ audioRemovedSamplesForAcceleration ?? '—' }} removed</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Audio buffer</div>
            <div class="meta-value">{{ formatMilliseconds(audioJitterBufferDelayMs) }} avg</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Host audio capture</div>
            <div class="meta-value">{{ status.audio_diagnostics?.capture_raw_sample_drops ?? '—' }} raw samples · {{ status.audio_diagnostics?.capture_chunk_drops ?? '—' }} chunks</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Host audio queues</div>
            <div class="meta-value">{{ status.audio_diagnostics?.pacing_backlog_drops ?? '—' }} pacing · {{ status.audio_diagnostics?.subscriber_queue_drops ?? '—' }} writer drops</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Host audio recovery</div>
            <div class="meta-value">{{ status.audio_diagnostics?.capture_discontinuities ?? '—' }} discontinuities · {{ status.audio_diagnostics?.capture_recovery_gap_packets ?? '—' }} gap packets</div>
          </div>
          <div class="data-cell">
            <div class="meta-label">Host audio errors</div>
            <div class="meta-value">{{ status.audio_diagnostics?.malformed_chunks ?? '—' }} malformed · {{ status.audio_diagnostics?.encode_failures ?? '—' }} encode · {{ status.audio_diagnostics?.write_failures ?? '—' }} write</div>
          </div>
        </section>

        <DroppedFramesChart :samples="droppedFrameSamples" />

        <section v-if="status.groups?.length" class="group-profiles" aria-label="Available transcode groups">
          <div class="meta-label">Available transcode targets</div>
          <div class="group-profile-list">
            <div v-for="profile in status.groups" :key="profile.id" class="group-profile">
              <span>{{ profile.id.replace('-', ' ') }}</span>
              <span>{{ profile.quality }} · {{ profile.fps }} FPS · {{ formatBitrate(profile.bitrate_bps) }} target · {{ formatCodecProfile(profile) }}</span>
              <span>{{ profile.state }}</span>
            </div>
          </div>
        </section>
      </div>
    </details>
  </main>
</template>
