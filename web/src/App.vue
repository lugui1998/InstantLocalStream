<script setup lang="ts">
import { onMounted } from 'vue'
import DroppedFramesChart from '@/components/DroppedFramesChart.vue'
import StreamVideo from '@/components/StreamVideo.vue'
import { useViewer } from '@/composables/useViewer'

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

    <details class="diagnostics">
      <summary>
        <span>Diagnostics</span>
        <span class="diagnostics-summary">Playback, decoder, group, and synchronization details</span>
      </summary>
      <div class="diagnostics-content">
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
        </section>

        <DroppedFramesChart :samples="droppedFrameSamples" />

        <section v-if="status.groups?.length" class="group-profiles" aria-label="Available transcode groups">
          <div class="meta-label">Available transcode targets</div>
          <div class="group-profile-list">
            <div v-for="profile in status.groups" :key="profile.id" class="group-profile">
              <span>{{ profile.id.replace('-', ' ') }}</span>
              <span>{{ profile.quality }} · {{ profile.fps }} FPS · {{ formatBitrate(profile.bitrate_bps) }} target · {{ profile.codec ?? '—' }}</span>
              <span>{{ profile.state }}</span>
            </div>
          </div>
        </section>
      </div>
    </details>
  </main>
</template>
