import { computed, onBeforeUnmount, ref } from 'vue'
import { io, type Socket } from 'socket.io-client'
import type { GroupAssignment, PingAcknowledgement, SessionReady, StreamStatus, ViewerBootstrap, ViewerStats, ViewerVideoCapability, WebRtcAnswer } from '@/types'

const retryDelays = [1_000, 2_000, 5_000, 10_000, 20_000, 30_000]
const bootstrapProbeTimeoutMs = 5_000
const bootstrapProbeMaxBytes = 1_024 * 1_024
const bootstrapAssignmentTimeoutMs = 1_500
const decodeStartupTimeoutMs = 3_000
const codecFallbackTimeoutMs = 3_000
const playbackMetricWindowMs = 15_000

interface PlaybackMetricSample {
  capturedAt: number
  dropped: number
  freezes: number
}

export interface BootstrapProgress {
  title: string
  detail: string
}

function clientIdentifier() {
  return crypto.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`
}

function waitForIce(peer: RTCPeerConnection) {
  return new Promise<void>((resolve, reject) => {
    if (peer.iceGatheringState === 'complete') return resolve()
    const timeout = window.setTimeout(() => {
      peer.removeEventListener('icegatheringstatechange', complete)
      reject(new Error('ICE gathering timed out'))
    }, 8_000)
    function complete() {
      if (peer.iceGatheringState === 'complete') {
        window.clearTimeout(timeout)
        peer.removeEventListener('icegatheringstatechange', complete)
        resolve()
      }
    }
    peer.addEventListener('icegatheringstatechange', complete)
  })
}

function normalizeCodecName(codec: string | null | undefined) {
  const normalized = codec?.trim().toLowerCase()
  return normalized || null
}

function formatProbeBytes(bytes: number) {
  if (bytes < 1_024) return `${bytes} B`
  if (bytes < 1_024 * 1_024) return `${Math.max(1, Math.round(bytes / 1_024))} KiB`
  return `${(bytes / (1_024 * 1_024)).toFixed(1)} MiB`
}

function formatCodecName(codec: string | null | undefined) {
  const normalized = normalizeCodecName(codec)
  if (!normalized) return null
  const name = normalized.includes('/') ? normalized.slice(normalized.lastIndexOf('/') + 1) : normalized
  if (name === 'h264' || name === 'avc') return 'H.264'
  if (name === 'vp8') return 'VP8'
  if (name === 'vp9') return 'VP9'
  if (name === 'av1' || name === 'av01') return 'AV1'
  return name.toUpperCase()
}

function normalizeCodecParameters(parameters: unknown) {
  if (!parameters || typeof parameters !== 'object' || Array.isArray(parameters)) return undefined
  const entries = Object.entries(parameters)
    .filter(([, value]) => ['string', 'number', 'boolean'].includes(typeof value))
    .map(([key, value]) => [key, String(value)] as const)
    .sort(([left], [right]) => left.localeCompare(right))
  return entries.length > 0 ? Object.fromEntries(entries) : undefined
}

function videoCapabilities(): ViewerVideoCapability[] {
  const receiver = globalThis.RTCRtpReceiver
  if (!receiver || typeof receiver.getCapabilities !== 'function') return []
  try {
    const capabilities = receiver.getCapabilities('video')
    const seen = new Set<string>()
    return (capabilities?.codecs ?? []).flatMap((codec) => {
      const mimeType = codec.mimeType?.trim().toLowerCase()
      if (!mimeType) return []
      const sdpFmtpLine = codec.sdpFmtpLine?.trim() || undefined
      const parameters = normalizeCodecParameters((codec as RTCRtpCodec & { parameters?: unknown }).parameters)
      const key = JSON.stringify([mimeType, sdpFmtpLine, parameters])
      if (seen.has(key)) return []
      seen.add(key)
      return [{
        mimeType,
        ...(sdpFmtpLine ? { sdpFmtpLine } : {}),
        ...(parameters ? { parameters } : {}),
      }]
    })
  } catch {
    return []
  }
}

export function useViewer() {
  const tokenPath = location.pathname.replace(/\/$/, '') || '/'
  const token = decodeURIComponent(tokenPath.split('/').filter(Boolean).at(-1) ?? '')
  const clientId = clientIdentifier()
  const videoStream = ref<MediaStream | null>(null)
  const status = ref<StreamStatus>({})
  const connection = ref('Connecting')
  const signalState = ref('Connecting')
  const rttMs = ref<number | null>(null)
  const jitterMs = ref<number | null>(null)
  const bitrateBps = ref<number | null>(null)
  const lossRate = ref<number | null>(null)
  const availableIncomingBitrateBps = ref<number | null>(null)
  const framesDropped = ref<number | null>(null)
  const freezeCount = ref<number | null>(null)
  const jitterBufferDelayMs = ref<number | null>(null)
  const catchUpDelayMs = ref<number | null>(null)
  const playoutDelayMs = ref<number | null>(null)
  const encoderDelayMs = ref<number | null>(null)
  const decodeTimeMs = ref<number | null>(null)
  const group = ref<GroupAssignment | null>(null)
  const negotiatedCodec = ref<string | null>(null)
  const bootstrapProgress = ref<BootstrapProgress | null>(null)
  let socket: Socket | null = null
  let peer: RTCPeerConnection | null = null
  let statsTimer: number | null = null
  let pingTimer: number | null = null
  let reconnectTimer: number | null = null
  let disconnectedTimer: number | null = null
  let decodeStartupTimer: number | null = null
  let codecFallbackTimer: number | null = null
  let reconnectAttempt = 0
  let negotiating = false
  let restartRequested = false
  let stopping = false
  let lastBytes: number | null = null
  let lastPackets: number | null = null
  let lastLost: number | null = null
  let lastStatsAt = 0
  let lastJitterBufferDelayTotal: number | null = null
  let lastJitterBufferTargetDelayTotal: number | null = null
  let lastJitterBufferMinimumDelayTotal: number | null = null
  let lastJitterBufferEmittedCount: number | null = null
  let serverClockOffsetMs: number | null = null
  let lastPlayoutDelayAt = 0
  let lastDroppedFramesTotal: number | null = null
  let lastFreezeCountTotal: number | null = null
  const playbackSamples: PlaybackMetricSample[] = []
  let initialSessionReady = false
  let initialWebRtcStarted = false
  let bootstrapComplete = false
  let bootstrapAssignmentReceived = false
  let bootstrapPromise: Promise<void> | null = null
  let finishBootstrapAssignmentWait: (() => void) | null = null
  let assignedCodec: string | null = null
  let failedCodec: string | null = null
  let awaitingCodecFallback = false
  let videoFrameRendered = false
  let startupNoMediaChecks = 0

  const viewers = computed(() => typeof status.value.viewers === 'number'
    ? `${status.value.viewers} / ${status.value.max_viewers ?? '—'}`
    : '—')

  const quality = computed(() => `${group.value?.quality ?? status.value.quality ?? '—'} · ${group.value?.fps ?? status.value.fps ?? '—'} FPS · ${formatBitrate(group.value?.bitrate_bps ?? status.value.bitrate_bps)}`)
  const synchronizationMode = computed(() => group.value?.sync_mode ?? status.value.sync_mode ?? status.value.synchronization_mode ?? 'Independent')
  const activeCodec = computed(() => negotiatedCodec.value ?? formatCodecName(group.value?.codec ?? status.value.codec) ?? '—')

  function formatBitrate(value: number | null | undefined) {
    if (!value || value <= 0) return '—'
    return value >= 1_000_000 ? `${(value / 1_000_000).toFixed(1)} Mbps` : `${Math.round(value / 1_000)} kbps`
  }

  function updatePlaybackWindow(now: number, droppedTotal: number | null, freezeTotal: number | null) {
    let droppedDelta = 0
    let freezeDelta = 0
    if (droppedTotal !== null) {
      if (lastDroppedFramesTotal !== null) droppedDelta = Math.max(0, droppedTotal - lastDroppedFramesTotal)
      lastDroppedFramesTotal = droppedTotal
    }
    if (freezeTotal !== null) {
      if (lastFreezeCountTotal !== null) freezeDelta = Math.max(0, freezeTotal - lastFreezeCountTotal)
      lastFreezeCountTotal = freezeTotal
    }
    if (droppedTotal === null && freezeTotal === null) return
    playbackSamples.push({ capturedAt: now, dropped: droppedDelta, freezes: freezeDelta })
    while (playbackSamples[0] && now - playbackSamples[0].capturedAt > playbackMetricWindowMs) playbackSamples.shift()
    if (droppedTotal !== null) framesDropped.value = playbackSamples.reduce((total, sample) => total + sample.dropped, 0)
    if (freezeTotal !== null) freezeCount.value = playbackSamples.reduce((total, sample) => total + sample.freezes, 0)
  }

  function mergeStatus(next: StreamStatus) {
    status.value = { ...status.value, ...next }
    if (next.group !== undefined) group.value = next.group
    updateAssignedCodec(next.codec ?? next.group?.codec)
    if (next.stream_enabled === false) connection.value = 'Waiting for stream'
    if (next.status === 'error' && next.media_error) connection.value = `Media error: ${next.media_error}`
  }

  function applyGroupAssignment(assignment: GroupAssignment) {
    group.value = assignment
    const fallbackAssigned = updateAssignedCodec(assignment.codec)
    if (awaitingCodecFallback) return
    if (!initialWebRtcStarted) {
      bootstrapAssignmentReceived = true
      finishBootstrapAssignmentWait?.()
      return
    }
    if (assignment.restart && !fallbackAssigned) restartWebRtcSession()
  }

  function updateAssignedCodec(codec: string | null | undefined) {
    const nextCodec = normalizeCodecName(codec)
    if (!nextCodec) return false
    assignedCodec = nextCodec
    if (awaitingCodecFallback && nextCodec !== failedCodec) {
      awaitingCodecFallback = false
      failedCodec = null
      if (codecFallbackTimer !== null) window.clearTimeout(codecFallbackTimer)
      codecFallbackTimer = null
      restartWebRtcSession()
      return true
    }
    return false
  }

  async function probeBootstrap(): Promise<ViewerBootstrap> {
    const startedAt = performance.now()
    const controller = new AbortController()
    let timedOut = false
    let firstByteAt: number | null = null
    const timeout = window.setTimeout(() => {
      timedOut = true
      controller.abort()
    }, bootstrapProbeTimeoutMs)
    let bytes = 0
    bootstrapProgress.value = {
      title: 'Measuring connection',
      detail: 'Starting a short network check…',
    }
    try {
      const response = await fetch(`${tokenPath}/api/probe?nonce=${encodeURIComponent(clientIdentifier())}`, {
        cache: 'no-store',
        credentials: 'omit',
        signal: controller.signal,
      })
      if (response.ok) {
        const expectedBytes = Number(response.headers.get('content-length') ?? 0)
        const reader = response.body?.getReader()
        if (reader) {
          for (;;) {
            const { done, value } = await reader.read()
            if (done) break
            if (firstByteAt === null) firstByteAt = performance.now()
            bytes += value.byteLength
            const elapsedSeconds = Math.max(0.1, (performance.now() - startedAt) / 1_000)
            const expected = expectedBytes > 0 ? ` of ${formatProbeBytes(expectedBytes)}` : ''
            bootstrapProgress.value = {
              title: 'Measuring connection',
              detail: `${formatProbeBytes(bytes)}${expected} received · ${formatBitrate(bytes * 8 / elapsedSeconds)}`,
            }
            if (bytes >= bootstrapProbeMaxBytes) {
              controller.abort()
              break
            }
          }
        } else {
          bytes = (await response.arrayBuffer()).byteLength
          firstByteAt = performance.now()
        }
      }
    } catch {
      // A failed probe is still reported so the server can use its fallback assignment.
    } finally {
      window.clearTimeout(timeout)
    }
    const elapsedMs = Math.max(1, performance.now() - startedAt)
    return {
      downloadBps: Math.round(bytes * 8_000 / elapsedMs),
      latencyMs: Math.round(Math.max(1, (firstByteAt ?? performance.now()) - startedAt)),
      timedOut,
      videoCapabilities: videoCapabilities(),
    }
  }

  async function bootstrapInitialWebRtc() {
    if (bootstrapPromise) return bootstrapPromise
    bootstrapPromise = (async () => {
      connection.value = 'Measuring connection'
      const metrics = await probeBootstrap()
      bootstrapProgress.value = metrics.timedOut
        ? { title: 'Selecting starting quality', detail: 'The check was slow, so the host is selecting a safe stream.' }
        : { title: 'Selecting starting quality', detail: `Measured ${formatBitrate(metrics.downloadBps)}. Choosing the best initial group…` }
      socket?.emit('viewer.bootstrap', metrics)
      if (!bootstrapAssignmentReceived) {
        connection.value = 'Waiting for group assignment'
        await new Promise<void>((resolve) => {
          let finished = false
          const finish = () => {
            if (finished) return
            finished = true
            window.clearTimeout(timeout)
            finishBootstrapAssignmentWait = null
            resolve()
          }
          const timeout = window.setTimeout(finish, bootstrapAssignmentTimeoutMs)
          finishBootstrapAssignmentWait = finish
          if (bootstrapAssignmentReceived) finish()
        })
      }
      bootstrapComplete = true
      bootstrapProgress.value = { title: 'Connecting to stream', detail: 'Negotiating the low-latency video session…' }
      startInitialWebRtc()
    })()
    return bootstrapPromise
  }

  function startInitialWebRtc() {
    if (!initialSessionReady || !bootstrapComplete || initialWebRtcStarted || stopping) return
    initialWebRtcStarted = true
    void startWebRtc().catch(handleWebRtcFailure)
  }

  function connectSignal() {
    socket = io({
      path: '/ws',
      auth: { token, clientId },
      transports: ['polling', 'websocket'],
      reconnection: true,
      reconnectionAttempts: Infinity,
      reconnectionDelay: 1_000,
      reconnectionDelayMax: 30_000,
    })
    socket.on('connect', () => {
      signalState.value = 'Connected'
      requestStatus()
      ping()
      void bootstrapInitialWebRtc()
    })
    socket.on('disconnect', () => { signalState.value = 'Reconnecting' })
    socket.on('connect_error', () => { signalState.value = 'Reconnecting' })
    socket.on('session.ready', (ready: SessionReady) => {
      mergeStatus(ready.status)
      initialSessionReady = true
      startInitialWebRtc()
    })
    socket.on('status.snapshot', mergeStatus)
    socket.on('status.changed', mergeStatus)
    socket.on('group.bootstrap', applyGroupAssignment)
    socket.on('group.assignment', applyGroupAssignment)
  }

  function requestStatus() {
    socket?.emit('status.request', (snapshot: StreamStatus) => mergeStatus(snapshot))
  }

  function ping() {
    const sentAt = performance.now()
    const sentEpochMs = Date.now()
    socket?.timeout(5_000).emit('control.ping', { sentAt }, (error: Error | null, ack: PingAcknowledgement) => {
      if (error || ack?.sentAt !== sentAt) return
      const roundTripMs = Math.max(0, performance.now() - sentAt)
      rttMs.value = roundTripMs
      if (typeof ack.serverTime === 'number') {
        const midpointEpochMs = sentEpochMs + roundTripMs / 2
        const sampledOffsetMs = ack.serverTime - midpointEpochMs
        serverClockOffsetMs = serverClockOffsetMs === null
          ? sampledOffsetMs
            : serverClockOffsetMs * 0.8 + sampledOffsetMs * 0.2
      }
      if (typeof ack.encoderDelayMs === 'number' && ack.encoderDelayMs >= 0) {
        encoderDelayMs.value = ack.encoderDelayMs
      } else if (typeof status.value.encoder_delay_ms === 'number') {
        encoderDelayMs.value = status.value.encoder_delay_ms
      }
      if (typeof ack.mediaError === 'string' && ack.mediaError) {
        connection.value = `Media error: ${ack.mediaError}`
      }
    })
  }

  async function updateStats() {
    if (!peer) return
    const current = peer
    const reports = await current.getStats()
    if (peer !== current) return
    let candidateRtt: number | null = null
    let fallbackCandidateRtt: number | null = null
    let inboundJitter: number | null = null
    let bytes: number | null = null
    let packets: number | null = null
    let lost: number | null = null
    let availableBitrate: number | null = null
    let dropped: number | null = null
    let freezes: number | null = null
    let jitterBufferDelayTotal: number | null = null
    let jitterBufferTargetDelayTotal: number | null = null
    let jitterBufferMinimumDelayTotal: number | null = null
    let jitterBufferEmittedCount: number | null = null
    let decodeTime: number | null = null
    let inboundCodecId: string | null = null
    let estimatedPlayoutTimestamp: number | null = null
    reports.forEach((report) => {
      if (report.type === 'candidate-pair' && report.state === 'succeeded') {
        const selected = report.selected === true || report.nominated === true
        if (typeof report.currentRoundTripTime === 'number') {
          if (selected) candidateRtt = report.currentRoundTripTime * 1_000
          else if (fallbackCandidateRtt === null) fallbackCandidateRtt = report.currentRoundTripTime * 1_000
        }
        if (selected && typeof report.availableIncomingBitrate === 'number') {
          availableBitrate = report.availableIncomingBitrate
        }
      }
      if (report.type === 'inbound-rtp' && report.kind === 'video') {
        if (typeof report.codecId === 'string') inboundCodecId = report.codecId
        if (typeof report.estimatedPlayoutTimestamp === 'number') {
          estimatedPlayoutTimestamp = report.estimatedPlayoutTimestamp
        }
        if (typeof report.jitter === 'number') inboundJitter = report.jitter * 1_000
        if (typeof report.bytesReceived === 'number') bytes = report.bytesReceived
        if (typeof report.packetsReceived === 'number') packets = report.packetsReceived
        if (typeof report.packetsLost === 'number') lost = report.packetsLost
        if (typeof report.framesDropped === 'number') dropped = report.framesDropped
        if (typeof report.freezeCount === 'number') freezes = report.freezeCount
        if (typeof report.jitterBufferDelay === 'number' && typeof report.jitterBufferEmittedCount === 'number' && report.jitterBufferEmittedCount > 0) {
          jitterBufferDelayTotal = report.jitterBufferDelay
          jitterBufferEmittedCount = report.jitterBufferEmittedCount
          if (typeof report.jitterBufferTargetDelay === 'number') {
            jitterBufferTargetDelayTotal = report.jitterBufferTargetDelay
          }
          if (typeof report.jitterBufferMinimumDelay === 'number') {
            jitterBufferMinimumDelayTotal = report.jitterBufferMinimumDelay
          }
        }
        if (typeof report.totalDecodeTime === 'number' && typeof report.framesDecoded === 'number' && report.framesDecoded > 0) {
          decodeTime = report.totalDecodeTime * 1_000 / report.framesDecoded
        }
      }
    })
    const renderedQuality = document.querySelector<HTMLVideoElement>('video')?.getVideoPlaybackQuality?.()
    if (renderedQuality && typeof renderedQuality.droppedVideoFrames === 'number') {
      dropped = renderedQuality.droppedVideoFrames
    }
    if (inboundCodecId) {
      const codecReport = reports.get(inboundCodecId)
      if (codecReport && typeof codecReport.mimeType === 'string') {
        negotiatedCodec.value = formatCodecName(codecReport.mimeType)
      }
    }
    rttMs.value = candidateRtt ?? fallbackCandidateRtt ?? rttMs.value
    jitterMs.value = inboundJitter
    if (jitterBufferDelayTotal !== null && jitterBufferEmittedCount !== null) {
      const emittedDelta = lastJitterBufferEmittedCount === null
        ? 0
        : jitterBufferEmittedCount - lastJitterBufferEmittedCount
      if (emittedDelta > 0 && lastJitterBufferDelayTotal !== null) {
        const delayMs = Math.max(0, (jitterBufferDelayTotal - lastJitterBufferDelayTotal) * 1_000 / emittedDelta)
        const targetMs = jitterBufferTargetDelayTotal !== null && lastJitterBufferTargetDelayTotal !== null
          ? Math.max(0, (jitterBufferTargetDelayTotal - lastJitterBufferTargetDelayTotal) * 1_000 / emittedDelta)
          : null
        const playoutBufferMs = targetMs ?? delayMs
        jitterBufferDelayMs.value = playoutBufferMs
        if (jitterBufferMinimumDelayTotal !== null && lastJitterBufferMinimumDelayTotal !== null) {
          const minimumMs = Math.max(0, (jitterBufferMinimumDelayTotal - lastJitterBufferMinimumDelayTotal) * 1_000 / emittedDelta)
          catchUpDelayMs.value = Math.max(0, playoutBufferMs - minimumMs)
        } else {
          catchUpDelayMs.value = playoutBufferMs
        }
      }
      lastJitterBufferDelayTotal = jitterBufferDelayTotal
      lastJitterBufferTargetDelayTotal = jitterBufferTargetDelayTotal
      lastJitterBufferMinimumDelayTotal = jitterBufferMinimumDelayTotal
      lastJitterBufferEmittedCount = jitterBufferEmittedCount
    }
    decodeTimeMs.value = decodeTime
    const now = performance.now()
    if (estimatedPlayoutTimestamp !== null && serverClockOffsetMs !== null) {
      const sampledDelayMs = Date.now() + serverClockOffsetMs - estimatedPlayoutTimestamp
      if (sampledDelayMs >= -250 && sampledDelayMs <= 60_000) {
        playoutDelayMs.value = playoutDelayMs.value === null
          ? sampledDelayMs
          : playoutDelayMs.value * 0.7 + sampledDelayMs * 0.3
        lastPlayoutDelayAt = now
      }
    } else if (lastPlayoutDelayAt > 0 && now - lastPlayoutDelayAt > 5_000) {
      playoutDelayMs.value = null
    }
    updatePlaybackWindow(now, dropped, freezes)
    if (bytes !== null && packets !== null && lost !== null) {
      bitrateBps.value = lastBytes !== null && lastStatsAt ? Math.max(0, (bytes - lastBytes) * 8_000 / Math.max(1, now - lastStatsAt)) : 0
      const receivedDelta = lastPackets === null ? 0 : packets - lastPackets
      const lostDelta = lastLost === null ? 0 : Math.max(0, lost - lastLost)
      lossRate.value = receivedDelta + lostDelta > 0 ? lostDelta / (receivedDelta + lostDelta) : 0
      lastBytes = bytes
      lastPackets = packets
      lastLost = lost
      lastStatsAt = now
      const usableIncomingCapacity = availableBitrate !== null
        && bitrateBps.value > 0
        && availableBitrate < bitrateBps.value * 0.9
        ? null
        : availableBitrate
      availableIncomingBitrateBps.value = usableIncomingCapacity
      const metrics: ViewerStats = {
        rttMs: rttMs.value ?? 0,
        jitterMs: jitterMs.value ?? 0,
        bitrateBps: bitrateBps.value,
        lossRate: lossRate.value,
        ...(usableIncomingCapacity === null ? {} : { availableIncomingBitrateBps: usableIncomingCapacity }),
        ...(dropped === null ? {} : { framesDropped: dropped }),
        ...(freezes === null ? {} : { freezeCount: freezes }),
        ...(jitterBufferDelayMs.value === null ? {} : { jitterBufferDelayMs: jitterBufferDelayMs.value }),
        ...(decodeTime === null ? {} : { decodeTimeMs: decodeTime }),
        visibilityState: document.visibilityState,
      }
      socket?.emit('viewer.stats', metrics)
    }
  }

  async function checkStartupDecodeFailure(current: RTCPeerConnection) {
    if (peer !== current || stopping || videoFrameRendered) return
    let bytesReceived = 0
    let framesDecoded: number | null = null
    try {
      const reports = await current.getStats()
      reports.forEach((report) => {
        if (report.type !== 'inbound-rtp' || report.kind !== 'video') return
        if (typeof report.bytesReceived === 'number') bytesReceived = Math.max(bytesReceived, report.bytesReceived)
        if (typeof report.framesDecoded === 'number') {
          framesDecoded = framesDecoded === null ? report.framesDecoded : Math.max(framesDecoded, report.framesDecoded)
        }
      })
    } catch {
      // Rendering remains a useful fallback signal when stats are unavailable.
    }
    if (peer !== current || stopping || videoFrameRendered) return
    if (bytesReceived === 0) {
      startupNoMediaChecks += 1
      if (startupNoMediaChecks >= 2) {
        reportCodecFailure(current, 'no_rtp_video_received_after_negotiation')
        return
      }
      connection.value = 'Waiting for first video frame'
      decodeStartupTimer = window.setTimeout(() => void checkStartupDecodeFailure(current), decodeStartupTimeoutMs)
      return
    }
    if (framesDecoded !== null && framesDecoded > 0) {
      startupNoMediaChecks = 0
      connection.value = 'Waiting for rendered video frame'
      decodeStartupTimer = window.setTimeout(() => void checkStartupDecodeFailure(current), decodeStartupTimeoutMs)
      return
    }
    reportCodecFailure(current, 'rtp_received_without_decoded_frames')
  }

  function reportCodecFailure(current: RTCPeerConnection, reason: string) {
    if (peer !== current || awaitingCodecFallback || stopping) return
    failedCodec = assignedCodec
    awaitingCodecFallback = true
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    reconnectTimer = null
    clearPeer()
    connection.value = 'Video decode failed; waiting for fallback'
    socket?.emit('viewer.codecFailure', { codec: failedCodec ?? 'unknown', reason })
    codecFallbackTimer = window.setTimeout(() => {
      codecFallbackTimer = null
      if (!awaitingCodecFallback || stopping) return
      awaitingCodecFallback = false
      failedCodec = null
      scheduleReconnect('Retrying the assigned codec', true)
    }, codecFallbackTimeoutMs)
  }

  function noteVideoFrameRendered() {
    videoFrameRendered = true
    if (decodeStartupTimer !== null) window.clearTimeout(decodeStartupTimer)
    decodeStartupTimer = null
  }

  function clearPeer() {
    if (statsTimer !== null) window.clearInterval(statsTimer)
    if (disconnectedTimer !== null) window.clearTimeout(disconnectedTimer)
    if (decodeStartupTimer !== null) window.clearTimeout(decodeStartupTimer)
    statsTimer = null
    disconnectedTimer = null
    decodeStartupTimer = null
    const current = peer
    peer = null
    current?.close()
    videoStream.value = null
    negotiatedCodec.value = null
    startupNoMediaChecks = 0
    lastBytes = lastPackets = lastLost = null
    lastStatsAt = 0
    lastJitterBufferDelayTotal = null
    lastJitterBufferTargetDelayTotal = null
    lastJitterBufferMinimumDelayTotal = null
    lastJitterBufferEmittedCount = null
    lastPlayoutDelayAt = 0
    lastDroppedFramesTotal = null
    lastFreezeCountTotal = null
    playbackSamples.length = 0
    framesDropped.value = null
    freezeCount.value = null
    jitterBufferDelayMs.value = null
    catchUpDelayMs.value = null
    playoutDelayMs.value = null
  }

  function scheduleReconnect(reason: string, immediate = false) {
    if (stopping || awaitingCodecFallback || reconnectTimer !== null || negotiating) return
    connection.value = reason
    const delay = immediate ? 0 : retryDelays[Math.min(reconnectAttempt++, retryDelays.length - 1)] + Math.round(Math.random() * 250)
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null
      startWebRtc().catch(handleWebRtcFailure)
    }, delay)
  }

  function handleWebRtcFailure(error: unknown) {
    clearPeer()
    connection.value = error instanceof Error ? error.message : 'WebRTC failed'
    scheduleReconnect('WebRTC reconnecting')
  }

  function restartWebRtcSession() {
    reconnectAttempt = 0
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    reconnectTimer = null
    clearPeer()
    connection.value = 'Group assignment restarting'
    if (negotiating) {
      restartRequested = true
      return
    }
    scheduleReconnect('Group assignment restarting', true)
  }

  async function startWebRtc() {
    if (stopping || negotiating) return
    negotiating = true
    clearPeer()
    connection.value = 'Negotiating'
    const current = new RTCPeerConnection({ iceServers: [] })
    peer = current
    videoFrameRendered = false
    try {
      const videoTransceiver = current.addTransceiver('video', { direction: 'recvonly' })
      const videoReceiver = videoTransceiver.receiver
      if (videoReceiver && 'playoutDelayHint' in videoReceiver) {
        try {
          ;(videoReceiver as RTCRtpReceiver & { playoutDelayHint?: number }).playoutDelayHint = 0
        } catch {
          // The hint is optional and browser-dependent.
        }
      }
      if (status.value.audio_enabled) current.addTransceiver('audio', { direction: 'recvonly' })
      const stream = new MediaStream()
      current.addEventListener('track', ({ track }) => {
        if (peer !== current || stream.getTracks().some(({ id }) => id === track.id)) return
        stream.addTrack(track)
        videoStream.value = stream
        bootstrapProgress.value = null
        connection.value = 'Receiving'
        reconnectAttempt = 0
      })
      current.addEventListener('iceconnectionstatechange', () => {
        if (peer !== current) return
        if (['connected', 'completed'].includes(current.iceConnectionState)) {
          if (disconnectedTimer !== null) window.clearTimeout(disconnectedTimer)
          disconnectedTimer = null
        } else if (['failed', 'closed'].includes(current.iceConnectionState)) {
          scheduleReconnect(`WebRTC ${current.iceConnectionState}`, true)
        } else if (current.iceConnectionState === 'disconnected' && disconnectedTimer === null) {
          connection.value = 'WebRTC disconnected'
          disconnectedTimer = window.setTimeout(() => scheduleReconnect('WebRTC reconnecting', true), 3_000)
        }
      })
      current.addEventListener('connectionstatechange', () => {
        if (peer === current && ['failed', 'closed'].includes(current.connectionState)) scheduleReconnect(`WebRTC ${current.connectionState}`, true)
      })
      await current.setLocalDescription(await current.createOffer())
      await waitForIce(current)
      const controller = new AbortController()
      const timeout = window.setTimeout(() => controller.abort(), 8_000)
      let response: Response
      try {
        response = await fetch(`${tokenPath}/api/offer`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ type: current.localDescription?.type, sdp: current.localDescription?.sdp, clientId }),
          signal: controller.signal,
        })
      } finally {
        window.clearTimeout(timeout)
      }
      if (!response.ok) throw new Error(`Offer failed with HTTP ${response.status}`)
      await current.setRemoteDescription(await response.json() as WebRtcAnswer)
      if (peer === current) {
        connection.value = 'Negotiated'
        void updateStats()
        statsTimer = window.setInterval(() => void updateStats(), 1_000)
        decodeStartupTimer = window.setTimeout(() => void checkStartupDecodeFailure(current), decodeStartupTimeoutMs)
      }
    } finally {
      negotiating = false
      if (restartRequested) {
        restartRequested = false
        scheduleReconnect('Group assignment restarting', true)
      }
    }
  }

  function unmute() {
    const element = document.querySelector<HTMLVideoElement>('video')
    if (element) {
      element.muted = false
      void element.play()
    }
  }

  function seekToLiveEdge(video: HTMLVideoElement) {
    const { seekable } = video
    if (!seekable || seekable.length === 0) return
    const liveEdge = seekable.end(seekable.length - 1)
    if (Number.isFinite(liveEdge)) {
      try {
        video.currentTime = liveEdge
      } catch {
        // A WebRTC MediaStream is usually not seekable; keep the browser's live policy.
      }
    }
  }

  function start() {
    connectSignal()
    pingTimer = window.setInterval(ping, 2_000)
    window.addEventListener('online', reconnect)
  }

  function reconnect() {
    reconnectAttempt = 0
    requestStatus()
    if (!initialWebRtcStarted) {
      void bootstrapInitialWebRtc()
      startInitialWebRtc()
      return
    }
    scheduleReconnect('WebRTC reconnecting', true)
  }

  function stop() {
    stopping = true
    window.removeEventListener('online', reconnect)
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    if (pingTimer !== null) window.clearInterval(pingTimer)
    if (codecFallbackTimer !== null) window.clearTimeout(codecFallbackTimer)
    pingTimer = null
    codecFallbackTimer = null
    clearPeer()
    socket?.disconnect()
    socket = null
    bootstrapProgress.value = null
  }

  onBeforeUnmount(stop)
  return { videoStream, status, connection, rttMs, jitterMs, bitrateBps, lossRate, availableIncomingBitrateBps, framesDropped, freezeCount, jitterBufferDelayMs, catchUpDelayMs, playoutDelayMs, encoderDelayMs, decodeTimeMs, group, activeCodec, bootstrapProgress, synchronizationMode, viewers, quality, start, stop, unmute, seekToLiveEdge, noteVideoFrameRendered, ping }
}
