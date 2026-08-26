<script setup lang="ts">
import { onMounted } from 'vue'
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
  freezeCount,
  jitterBufferDelayMs,
  catchUpDelayMs,
  playoutDelayMs,
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
  const encoder = encoderDelayMs.value ?? status.value.encoder_delay_ms ?? null
  const browserPlayout = playoutDelayMs.value
    ?? (jitterBufferDelayMs.value === null
      ? null
      : jitterBufferDelayMs.value + (decodeTimeMs.value ?? 0))
  if (encoder !== null) {
    // Encoder age is measured from a raw source frame entering the host bus to
    // its encoded access unit. Add browser playout/decode and a conservative
    // one-way control-path estimate when available; this is much closer to
    // capture-to-display delay than jitter-buffer delay alone.
    const transport = rttMs.value === null ? 0 : rttMs.value / 2
    const total = encoder + (browserPlayout ?? 0) + transport
    const playout = browserPlayout === null ? '' : ` / client ${Math.round(browserPlayout)} ms`
    return `${Math.round(total)} ms est. end-to-end / encode ${Math.round(encoder)} ms${playout}`
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
    <StreamVideo :stream="videoStream" :catch-up-delay-ms="catchUpDelayMs" :bootstrap-progress="bootstrapProgress" @unmute="unmute" @live-edge="seekToLiveEdge" @frame-rendered="noteVideoFrameRendered" />

    <section class="data-grid" aria-label="Stream details">
      <div class="data-cell">
        <div class="meta-label">Stream</div>
        <div class="meta-value">{{ quality }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Estimated delay</div>
        <div class="meta-value">{{ formatDelay() }}</div>
      </div>
      <div class="data-cell">
        <div class="meta-label">Network</div>
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

    <section v-if="status.groups?.length" class="group-profiles" aria-label="Available transcode groups">
      <div class="meta-label">Available transcode groups</div>
      <div class="group-profile-list">
        <div v-for="profile in status.groups" :key="profile.id" class="group-profile">
          <span>{{ profile.id.replace('-', ' ') }}</span>
          <span>{{ profile.quality }} · {{ profile.fps }} FPS · {{ formatBitrate(profile.bitrate_bps) }} · {{ profile.codec ?? '—' }}</span>
          <span>{{ profile.state }}</span>
        </div>
      </div>
    </section>
  </main>
</template>
