import { computed, onBeforeUnmount, ref } from 'vue'
import { io, type Socket } from 'socket.io-client'
import type { AuthoritativeStreamSettings, FrameTimingAcknowledgement, GroupAssignment, PingAcknowledgement, PlaybackMetricPoint, RenderedFrameTiming, SessionReady, StreamStatus, ViewerBootstrap, ViewerStats, ViewerVideoCapability, WebRtcAnswer } from '@/types'
import { retryDelayFor } from '@/viewerUtils'

const bootstrapProbeTimeoutMs = 5_000
const bootstrapProbeMaxBytes = 1_024 * 1_024
const bootstrapAssignmentTimeoutMs = 1_500
const decodeStartupTimeoutMs = 3_000
const codecFallbackTimeoutMs = 3_000
const playbackMetricWindowMs = 15_000

export interface BootstrapProgress {
  title: string
  detail: string
}

const waitingForStreamMessage = 'Connected. Waiting for stream to start'

function clientIdentifier() {
  return crypto.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`
}

function waitForIce(peer: RTCPeerConnection, signal: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    if (peer.iceGatheringState === 'complete') return resolve()
    const abort = () => {
      cleanup()
      reject(new DOMException('ICE gathering aborted', 'AbortError'))
    }
    const timeout = window.setTimeout(() => {
      cleanup()
      reject(new Error('ICE gathering timed out'))
    }, 8_000)
    function cleanup() {
      window.clearTimeout(timeout)
      peer.removeEventListener('icegatheringstatechange', complete)
      signal.removeEventListener('abort', abort)
    }
    function complete() {
      if (peer.iceGatheringState === 'complete') {
        cleanup()
        resolve()
      }
    }
    peer.addEventListener('icegatheringstatechange', complete)
    signal.addEventListener('abort', abort, { once: true })
    if (signal.aborted) abort()
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
  const captureToDisplayDelayMs = ref<number | null>(null)
  const captureToReceiveDelayMs = ref<number | null>(null)
  const receiveToDisplayDelayMs = ref<number | null>(null)
  const frameProcessingDelayMs = ref<number | null>(null)
  const frameDelayMode = ref<'host-correlated' | 'browser-estimated' | null>(null)
  const frameTimingUncertaintyMs = ref<number | null>(null)
  const encoderDelayMs = ref<number | null>(null)
  const decodeTimeMs = ref<number | null>(null)
  const group = ref<GroupAssignment | null>(null)
  const negotiatedCodec = ref<string | null>(null)
  const bootstrapProgress = ref<BootstrapProgress | null>(null)
  const mediaStatus = ref<string | null>(null)
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
  let resumeAfterStreamReset = false
  let negotiationAbortController: AbortController | null = null
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
  const playbackSamples: PlaybackMetricPoint[] = []
  const droppedFrameSamples = ref<PlaybackMetricPoint[]>([])
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
  let lastFrameTimingRequestAt = 0
  let lastHostFrameTimingAt = 0
  let videoElement: HTMLVideoElement | null = null
  let statsFailures = 0
  let statsNextAllowedAt = 0
  let hostFrameTimingExpiryTimer: number | null = null
  let authoritativeSettingsRevision = -1
  // These values describe the m-lines in the current offer, not merely the
  // newest host status.  A host change that alters either value requires a
  // clean offer; an existing peer cannot grow an audio m-line in place.
  let negotiatedAudioEnabled: boolean | null = null
  let negotiatedMediaSessionRevision: number | null = null

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
    droppedFrameSamples.value = playbackSamples.map(sample => ({ ...sample }))
    if (droppedTotal !== null) framesDropped.value = playbackSamples.reduce((total, sample) => total + sample.dropped, 0)
    if (freezeTotal !== null) freezeCount.value = playbackSamples.reduce((total, sample) => total + sample.freezes, 0)
  }

  function mergeStatus(next: StreamStatus) {
    const wasStopped = statusIndicatesStopped(status.value)
    const wasResetting = statusIndicatesResetting(status.value)
    if (typeof next.settings_revision === 'number') {
      if (next.settings_revision < authoritativeSettingsRevision) return false
      authoritativeSettingsRevision = Math.max(authoritativeSettingsRevision, next.settings_revision)
    }
    status.value = { ...status.value, ...next }
    if (next.group !== undefined) {
      group.value = next.group
    } else if (group.value && next.groups) {
      const authoritative = next.groups.find(candidate => candidate.id === group.value?.id)
      if (authoritative) group.value = { ...group.value, ...authoritative }
    }
    updateAssignedCodec(next.codec ?? next.group?.codec)
    if (statusIndicatesResetting(next)) {
      pauseForStreamReset()
    } else if (statusIndicatesStopped(next)) {
      pauseForStoppedStream()
    } else if (streamIsRunning() && (wasStopped || wasResetting)) {
      resumeAfterStreamStart(wasResetting ? 'Stream restarting' : 'Stream starting')
    }
    if (next.status === 'error' && next.media_error) connection.value = `Media error: ${next.media_error}`
    reconcileHostMediaTopology()
    return true
  }

  function streamIsRunning() {
    return !statusIndicatesResetting(status.value) && status.value.stream_enabled !== false
  }

  function setInactiveStreamMessage() {
    const message = statusIndicatesResetting(status.value)
      ? 'Stream reset in progress'
      : signalState.value === 'Connected'
        ? waitingForStreamMessage
        : 'Connecting to stream'
    connection.value = message
    bootstrapProgress.value = { title: message, detail: '' }
  }

  function statusIndicatesResetting(value: StreamStatus) {
    return value.stream_resetting === true || value.status === 'resetting'
  }

  function statusIndicatesStopped(value: StreamStatus) {
    return !statusIndicatesResetting(value) && (value.stream_enabled === false
      || (value.stream_enabled === undefined && value.status === 'stopped')
    )
  }

  function pauseForStoppedStream() {
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    if (disconnectedTimer !== null) window.clearTimeout(disconnectedTimer)
    if (codecFallbackTimer !== null) window.clearTimeout(codecFallbackTimer)
    reconnectTimer = null
    disconnectedTimer = null
    codecFallbackTimer = null
    awaitingCodecFallback = false
    failedCodec = null
    resumeAfterStreamReset = false
    clearPeer()
    bootstrapProgress.value = null
    setInactiveStreamMessage()
  }

  function pauseForStreamReset() {
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    if (disconnectedTimer !== null) window.clearTimeout(disconnectedTimer)
    if (codecFallbackTimer !== null) window.clearTimeout(codecFallbackTimer)
    reconnectTimer = null
    disconnectedTimer = null
    codecFallbackTimer = null
    awaitingCodecFallback = false
    failedCodec = null
    resumeAfterStreamReset = true
    clearPeer()
    setInactiveStreamMessage()
  }

  function resumeAfterStreamStart(reason = 'Stream starting') {
    if (!initialSessionReady || stopping) return
    connection.value = reason
    bootstrapProgress.value = { title: reason, detail: '' }
    if (!bootstrapComplete) {
      resumeAfterStreamReset = false
      void bootstrapInitialWebRtc()
    } else if (!initialWebRtcStarted) {
      resumeAfterStreamReset = false
      startInitialWebRtc()
    } else if (!peer && !negotiating) {
      resumeAfterStreamReset = false
      scheduleReconnect(reason, true)
    }
  }

  function applyAuthoritativeSettings(update: AuthoritativeStreamSettings) {
    if (!Number.isFinite(update.revision) || update.revision < authoritativeSettingsRevision) return
    mergeStatus({ ...update.status, settings_revision: update.revision })
  }

  function reconcileHostMediaTopology() {
    if (!peer || negotiatedAudioEnabled === null) return
    const hostAudioEnabled = status.value.audio_enabled === true
    const audioTopologyChanged = hostAudioEnabled !== negotiatedAudioEnabled
    const hostSessionRevision = status.value.media_session_revision
    const sessionRestartRequired = status.value.stream_enabled === true
      && typeof hostSessionRevision === 'number'
      && negotiatedMediaSessionRevision !== null
      && hostSessionRevision !== negotiatedMediaSessionRevision
    if (audioTopologyChanged || sessionRestartRequired) {
      restartWebRtcSession(audioTopologyChanged
        ? 'Audio configuration restarting'
        : 'Host stream restarting')
    }
  }

  function applyGroupAssignment(assignment: GroupAssignment) {
    if (typeof assignment.settings_revision === 'number') {
      if (assignment.settings_revision < authoritativeSettingsRevision) return
      authoritativeSettingsRevision = Math.max(authoritativeSettingsRevision, assignment.settings_revision)
    }
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
    if (!initialSessionReady) return
    bootstrapPromise = (async () => {
      if (!streamIsRunning()) {
        setInactiveStreamMessage()
        bootstrapPromise = null
        return
      }
      connection.value = 'Measuring connection'
      const metrics = await probeBootstrap()
      if (!streamIsRunning()) {
        bootstrapProgress.value = null
        setInactiveStreamMessage()
        bootstrapPromise = null
        return
      }
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
      if (!streamIsRunning()) {
        bootstrapProgress.value = null
        setInactiveStreamMessage()
        bootstrapPromise = null
        return
      }
      bootstrapComplete = true
      bootstrapProgress.value = { title: 'Connecting to stream', detail: '' }
      startInitialWebRtc()
    })()
    return bootstrapPromise
  }

  function startInitialWebRtc() {
    if (!initialSessionReady || !bootstrapComplete || initialWebRtcStarted || stopping || !streamIsRunning()) {
      if (!stopping && !streamIsRunning()) setInactiveStreamMessage()
      return
    }
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
      bootstrapProgress.value = { title: 'Connecting to stream', detail: '' }
      requestStatus()
      ping()
    })
    socket.on('disconnect', () => {
      signalState.value = 'Reconnecting'
      if (!videoStream.value) setInactiveStreamMessage()
    })
    socket.on('connect_error', () => {
      signalState.value = 'Reconnecting'
      if (!videoStream.value) setInactiveStreamMessage()
    })
    socket.on('session.ready', (ready: SessionReady) => {
      const resumedSignalingSession = initialSessionReady
      // This snapshot begins a new authoritative signaling session. The host
      // process may have restarted while this page stayed open, in which case
      // its settings and media revision counters legitimately start over.
      // Keeping the old high-water mark would reject every update from the
      // replacement host and strand the page in its last reset state.
      authoritativeSettingsRevision = -1
      initialSessionReady = true
      mergeStatus(ready.status)
      if (streamIsRunning()) {
        if (resumedSignalingSession && initialWebRtcStarted) {
          restartWebRtcSession('Stream reconnecting')
        } else {
          resumeAfterStreamStart()
        }
      } else {
        setInactiveStreamMessage()
      }
    })
    socket.on('status.snapshot', mergeStatus)
    socket.on('status.changed', mergeStatus)
    socket.on('stream.settings', applyAuthoritativeSettings)
    socket.on('group.bootstrap', applyGroupAssignment)
    socket.on('group.assignment', applyGroupAssignment)
  }

  function requestStatus() {
    socket?.emit('status.request', (snapshot: StreamStatus) => mergeStatus(snapshot))
  }

  function updateServerClockOffset(sampledOffsetMs: number) {
    serverClockOffsetMs = serverClockOffsetMs === null
      ? sampledOffsetMs
        : serverClockOffsetMs * 0.8 + sampledOffsetMs * 0.2
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
        updateServerClockOffset(sampledOffsetMs)
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
    if (performance.now() < statsNextAllowedAt) return
    const current = peer
    let reports: RTCStatsReport
    try {
      reports = await current.getStats()
      statsFailures = 0
      statsNextAllowedAt = 0
      if (mediaStatus.value?.startsWith('Statistics temporarily unavailable')) mediaStatus.value = null
    } catch {
      if (peer !== current || stopping) return
      statsFailures += 1
      const retryAfterMs = retryDelayFor(statsFailures - 1)
      statsNextAllowedAt = performance.now() + retryAfterMs
      mediaStatus.value = `Statistics temporarily unavailable; retrying in ${Math.round(retryAfterMs / 1_000)} seconds.`
      return
    }
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
    const renderedQuality = videoElement?.getVideoPlaybackQuality?.()
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
      if (!['connected', 'completed'].includes(current.iceConnectionState)) {
        connection.value = 'Connecting media path'
        decodeStartupTimer = window.setTimeout(() => void checkStartupDecodeFailure(current), decodeStartupTimeoutMs)
        return
      }
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

  function smoothDelay(current: number | null, sample: number) {
    return current === null ? sample : current * 0.8 + sample * 0.2
  }

  function applyRenderedFrameTiming(
    timing: RenderedFrameTiming,
    captureTimePerformanceMs: number,
    mode: 'host-correlated' | 'browser-estimated',
    frameEncoderDelayMs?: number,
  ) {
    const rawCaptureToDisplay = timing.expectedDisplayTimeMs - captureTimePerformanceMs
    let captureToDisplay = rawCaptureToDisplay
    let captureToReceive: number | null = null
    let receiveToDisplay: number | null = null
    if (timing.receiveTimeMs !== undefined) {
      const rawCaptureToReceive = timing.receiveTimeMs - captureTimePerformanceMs
      receiveToDisplay = timing.expectedDisplayTimeMs - timing.receiveTimeMs
      if (mode === 'host-correlated') {
        const hostEncode = frameEncoderDelayMs
          ?? encoderDelayMs.value
          ?? status.value.encoder_delay_ms
          ?? 0
        captureToReceive = Math.max(rawCaptureToReceive, hostEncode)
        captureToDisplay = captureToReceive + receiveToDisplay
      } else {
        captureToReceive = rawCaptureToReceive
      }
    }
    if (captureToDisplay < 0 || captureToDisplay > 60_000) return false
    captureToDisplayDelayMs.value = smoothDelay(captureToDisplayDelayMs.value, captureToDisplay)
    frameDelayMode.value = mode
    if (captureToReceive !== null && receiveToDisplay !== null) {
      if (captureToReceive >= 0 && captureToReceive <= 60_000) {
        captureToReceiveDelayMs.value = smoothDelay(captureToReceiveDelayMs.value, captureToReceive)
      }
      if (receiveToDisplay >= 0 && receiveToDisplay <= 60_000) {
        receiveToDisplayDelayMs.value = smoothDelay(receiveToDisplayDelayMs.value, receiveToDisplay)
      }
    }
    if (timing.processingDurationMs !== undefined
      && timing.processingDurationMs >= 0
      && timing.processingDurationMs <= 60_000) {
      frameProcessingDelayMs.value = smoothDelay(frameProcessingDelayMs.value, timing.processingDurationMs)
    }
    return true
  }

  function requestHostFrameTiming(timing: RenderedFrameTiming) {
    if (timing.rtpTimestamp === undefined || !socket) return
    const requestedAt = performance.now()
    if (requestedAt - lastFrameTimingRequestAt < 500) return
    lastFrameTimingRequestAt = requestedAt
    const requestedAtEpochMs = Date.now()
    const currentPeer = peer
    socket.timeout(1_500).emit(
      'viewer.frameTiming',
      { rtpTimestamp: timing.rtpTimestamp },
      (error: Error | null, acknowledgement: FrameTimingAcknowledgement) => {
        if (error
          || peer !== currentPeer
          || acknowledgement?.rtpTimestamp !== timing.rtpTimestamp
          || typeof acknowledgement.captureTimeUnixMs !== 'number') {
          expireHostFrameTimingIfStale()
          return
        }
        const roundTripMs = Math.max(0, performance.now() - requestedAt)
        frameTimingUncertaintyMs.value = smoothDelay(
          frameTimingUncertaintyMs.value,
          roundTripMs / 2,
        )
        const sampledOffsetMs = acknowledgement.serverTime - (requestedAtEpochMs + roundTripMs / 2)
        updateServerClockOffset(sampledOffsetMs)
        if (serverClockOffsetMs === null) return
        const captureTimePerformanceMs = acknowledgement.captureTimeUnixMs
          - serverClockOffsetMs
          - performance.timeOrigin
        const accepted = applyRenderedFrameTiming(
          timing,
          captureTimePerformanceMs,
          'host-correlated',
          typeof acknowledgement.encoderDelayMs === 'number'
            ? acknowledgement.encoderDelayMs
            : undefined,
        )
        if (accepted) {
          lastHostFrameTimingAt = performance.now()
          scheduleHostFrameTimingExpiry()
        }
      },
    )
  }

  function expireHostFrameTimingIfStale() {
    if (frameDelayMode.value !== 'host-correlated'
      || lastHostFrameTimingAt === 0
      || performance.now() - lastHostFrameTimingAt <= 3_000) return
    frameDelayMode.value = null
    captureToDisplayDelayMs.value = null
    captureToReceiveDelayMs.value = null
    receiveToDisplayDelayMs.value = null
    frameProcessingDelayMs.value = null
    frameTimingUncertaintyMs.value = null
    if (hostFrameTimingExpiryTimer !== null) window.clearTimeout(hostFrameTimingExpiryTimer)
    hostFrameTimingExpiryTimer = null
  }

  function scheduleHostFrameTimingExpiry() {
    if (hostFrameTimingExpiryTimer !== null) window.clearTimeout(hostFrameTimingExpiryTimer)
    hostFrameTimingExpiryTimer = window.setTimeout(() => {
      hostFrameTimingExpiryTimer = null
      expireHostFrameTimingIfStale()
    }, 3_100)
  }

  function noteVideoFrameRendered(timing?: RenderedFrameTiming) {
    videoFrameRendered = true
    if (decodeStartupTimer !== null) window.clearTimeout(decodeStartupTimer)
    decodeStartupTimer = null
    if (!timing) return
    expireHostFrameTimingIfStale()
    if (timing.captureTimeMs !== undefined && frameDelayMode.value !== 'host-correlated') {
      applyRenderedFrameTiming(timing, timing.captureTimeMs, 'browser-estimated')
    }
    requestHostFrameTiming(timing)
  }

  function clearPeer() {
    negotiationAbortController?.abort()
    negotiationAbortController = null
    if (statsTimer !== null) window.clearInterval(statsTimer)
    if (disconnectedTimer !== null) window.clearTimeout(disconnectedTimer)
    if (decodeStartupTimer !== null) window.clearTimeout(decodeStartupTimer)
    if (hostFrameTimingExpiryTimer !== null) window.clearTimeout(hostFrameTimingExpiryTimer)
    statsTimer = null
    disconnectedTimer = null
    decodeStartupTimer = null
    hostFrameTimingExpiryTimer = null
    const current = peer
    peer = null
    negotiatedAudioEnabled = null
    negotiatedMediaSessionRevision = null
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
    droppedFrameSamples.value = []
    framesDropped.value = null
    freezeCount.value = null
    jitterBufferDelayMs.value = null
    catchUpDelayMs.value = null
    playoutDelayMs.value = null
    captureToDisplayDelayMs.value = null
    captureToReceiveDelayMs.value = null
    receiveToDisplayDelayMs.value = null
    frameProcessingDelayMs.value = null
    frameDelayMode.value = null
    frameTimingUncertaintyMs.value = null
    lastFrameTimingRequestAt = 0
    lastHostFrameTimingAt = 0
    statsFailures = 0
    statsNextAllowedAt = 0
  }

  function scheduleReconnect(reason: string, immediate = false) {
    if (stopping || !streamIsRunning()) {
      if (!stopping) setInactiveStreamMessage()
      return
    }
    if (awaitingCodecFallback || reconnectTimer !== null || negotiating) return
    connection.value = reason
    const delay = immediate ? 0 : retryDelayFor(reconnectAttempt++) + Math.round(Math.random() * 250)
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null
      startWebRtc().catch(handleWebRtcFailure)
    }, delay)
  }

  function handleWebRtcFailure(error: unknown) {
    clearPeer()
    if (!streamIsRunning()) {
      setInactiveStreamMessage()
      return
    }
    connection.value = error instanceof Error ? error.message : 'WebRTC failed'
    scheduleReconnect('WebRTC reconnecting')
  }

  function restartWebRtcSession(reason = 'Group assignment restarting') {
    reconnectAttempt = 0
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
    reconnectTimer = null
    clearPeer()
    connection.value = reason
    if (negotiating) {
      restartRequested = true
      return
    }
    scheduleReconnect(reason, true)
  }

  async function startWebRtc() {
    if (stopping || !streamIsRunning() || negotiating) return
    negotiating = true
    clearPeer()
    connection.value = 'Negotiating'
    const current = new RTCPeerConnection({ iceServers: [] })
    peer = current
    const offerAudioEnabled = status.value.audio_enabled === true
    const offerMediaSessionRevision = status.value.media_session_revision
    negotiatedAudioEnabled = offerAudioEnabled
    negotiatedMediaSessionRevision = typeof offerMediaSessionRevision === 'number'
      ? offerMediaSessionRevision
      : null
    videoFrameRendered = false
    const controller = new AbortController()
    negotiationAbortController = controller
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
      if (offerAudioEnabled) current.addTransceiver('audio', { direction: 'recvonly' })
      const stream = new MediaStream()
      current.addEventListener('track', ({ track }) => {
        if (peer !== current || stream.getTracks().some(({ id }) => id === track.id)) return
        stream.addTrack(track)
        // Audio normally arrives after video. Replacing the wrapper stream
        // makes that topology change observable to Vue; assigning the same
        // MediaStream again leaves StreamVideo unaware of the new audio track
        // so the native media controls see the complete track set.
        videoStream.value = new MediaStream(stream.getTracks())
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
      await waitForIce(current, controller.signal)
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
        if (negotiationAbortController === controller) negotiationAbortController = null
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
      if (negotiationAbortController === controller) negotiationAbortController = null
      negotiating = false
      if (restartRequested) {
        restartRequested = false
        scheduleReconnect('Group assignment restarting', true)
      } else if (resumeAfterStreamReset && streamIsRunning()) {
        resumeAfterStreamReset = false
        scheduleReconnect('Stream restarting', true)
      }
    }
  }

  function setVideoElement(element: HTMLVideoElement | null) {
    videoElement = element
  }

  function reportPlaybackError() {
    mediaStatus.value = 'Video playback was blocked by the browser. Select the stream controls to start playback.'
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
    bootstrapProgress.value = { title: 'Connecting to stream', detail: '' }
    connectSignal()
    pingTimer = window.setInterval(ping, 2_000)
    window.addEventListener('online', reconnect)
  }

  function reconnect() {
    reconnectAttempt = 0
    requestStatus()
    if (!streamIsRunning()) {
      setInactiveStreamMessage()
      return
    }
    if (!initialWebRtcStarted) {
      void bootstrapInitialWebRtc()
      startInitialWebRtc()
      return
    }
    scheduleReconnect('WebRTC reconnecting', true)
  }

  function stop() {
    stopping = true
    resumeAfterStreamReset = false
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
  return { videoStream, status, connection, mediaStatus, rttMs, jitterMs, bitrateBps, lossRate, availableIncomingBitrateBps, framesDropped, freezeCount, droppedFrameSamples, jitterBufferDelayMs, catchUpDelayMs, playoutDelayMs, captureToDisplayDelayMs, captureToReceiveDelayMs, receiveToDisplayDelayMs, frameProcessingDelayMs, frameDelayMode, frameTimingUncertaintyMs, encoderDelayMs, decodeTimeMs, group, activeCodec, bootstrapProgress, synchronizationMode, viewers, quality, start, stop, setVideoElement, reportPlaybackError, seekToLiveEdge, noteVideoFrameRendered, ping }
}
