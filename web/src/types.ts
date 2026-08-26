export interface StreamStatus {
  status?: string
  stream_enabled?: boolean
  viewers?: number
  max_viewers?: number
  media_error?: string | null
  audio_enabled?: boolean
  quality?: string
  fps?: string | number
  bitrate_bps?: number
  encoder_delay_ms?: number | null
  stale_encoded_frames?: number
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

export interface PingAcknowledgement {
  sentAt: number
  serverTime: number
  encoderDelayMs?: number | null
  staleEncodedFrames?: number
  mediaStatus?: string
  mediaError?: string | null
}

export interface WebRtcAnswer {
  type: RTCSdpType
  sdp: string
}
