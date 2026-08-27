<script setup lang="ts">
import { onMounted } from 'vue'
import DroppedFramesChart from '@/components/DroppedFramesChart.vue'
import StreamVideo from '@/components/StreamVideo.vue'
import { useViewer } from '@/composables/useViewer'

const {
  videoStream,
  status,
  connection,
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
  unmute,
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
    const processing = receiver || frameProcessingDelayMs.value === null
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
    <StreamVideo :stream="videoStream" :audio-enabled="status.audio_enabled === true" :catch-up-delay-ms="catchUpDelayMs" :bootstrap-progress="bootstrapProgress" @unmute="unmute" @live-edge="seekToLiveEdge" @frame-rendered="noteVideoFrameRendered" />

    <section class="data-grid" aria-label="Stream details">
      <div class="data-cell">
        <div class="meta-label">Assigned target</div>
        <div class="meta-value">{{ quality }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">{{ delayLabel() }}</div>
        <div class="meta-value">{{ formatDelay() }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Measured receive</div>
        <div class="meta-value">{{ formatNetwork() }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Session</div>
        <div class="meta-value" :class="{ error: connection.includes('error') || connection.includes('failed') }">{{ connection }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Viewers</div>
        <div class="meta-value">{{ viewers }}</div>
      </div>
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
  </main>
</template>
