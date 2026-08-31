export interface StreamStatus {
  status?: string
  stream_enabled?: boolean
  stream_resetting?: boolean
  viewers?: number
  max_viewers?: number
  media_error?: string | null
  audio_enabled?: boolean
  quality?: string
  fps?: string | number
  bitrate_bps?: number
  settings_revision?: number
  media_session_revision?: number
  encoder_delay_ms?: number | null
  stale_encoded_frames?: number
  encoder_backlog_restarts?: number
  codec?: string | null
  group?: GroupAssignment | null
  groups?: GroupAssignment[]
  sync_mode?: string
  synchronization_mode?: string
}

export interface GroupAssignment {
  id: string
  label: string
  quality: string
  fps: string | number
  bitrate_bps: number
  codec?: string | null
  state: string
  reason: string | null
  restart?: boolean
  settings_revision?: number
  sync_mode?: string
}

export interface ViewerBootstrap {
  downloadBps: number
  latencyMs: number
  timedOut: boolean
  videoCapabilities: ViewerVideoCapability[]
}

export interface ViewerVideoCapability {
  mimeType: string
  sdpFmtpLine?: string
  parameters?: Record<string, string>
}

export interface SessionReady {
  version: string
  media: string
  status: StreamStatus
}

export type SessionGoodbyeReason = 'host_shutdown' | 'token_changed'

export interface SessionGoodbye {
  reason: SessionGoodbyeReason
  reconnect: false
}

export interface AuthoritativeStreamSettings {
  revision: number
  status: StreamStatus
}

export interface ViewerStats {
  rttMs: number
  jitterMs: number
  bitrateBps: number
  lossRate: number
  availableIncomingBitrateBps?: number
  framesDropped?: number
  freezeCount?: number
  jitterBufferDelayMs?: number
  decodeTimeMs?: number
  visibilityState?: DocumentVisibilityState
}

export interface RenderedFrameTiming {
  expectedDisplayTimeMs: number
  presentationTimeMs: number
  captureTimeMs?: number
  receiveTimeMs?: number
  processingDurationMs?: number
  rtpTimestamp?: number
}

export interface FrameTimingAcknowledgement {
  rtpTimestamp: number
  captureTimeUnixMs?: number | null
  encoderDelayMs?: number | null
  serverTime: number
}

export interface PlaybackMetricPoint {
  capturedAt: number
  dropped: number
  freezes: number
}

export interface PingAcknowledgement {
  sentAt: number
  serverTime: number
  encoderDelayMs?: number | null
  staleEncodedFrames?: number
  encoderBacklogRestarts?: number
  mediaStatus?: string
  mediaError?: string | null
}

export interface WebRtcAnswer {
  type: RTCSdpType
  sdp: string
}
