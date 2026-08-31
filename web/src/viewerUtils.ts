export const retryDelays = [1_000, 2_000, 5_000, 10_000, 20_000, 30_000] as const

export function retryDelayFor(attempt: number) {
  const index = Math.max(0, Math.min(Math.trunc(attempt), retryDelays.length - 1))
  return retryDelays[index]
}

export function sessionGoodbyeMessage(reason: string) {
  if (reason === 'token_changed') return 'The host created a new viewer link.'
  if (reason === 'host_shutdown') return 'The host application closed.'
  return 'The host ended this viewing session.'
}

export function isTerminalSocketDisconnect(reason: string) {
  return reason === 'io server disconnect'
}

export function playbackRateFor(delayMs: number | null, hasLiveAudio = false) {
  // The media element carries both tracks. Speeding it up to recover video
  // latency also invokes the browser's audio time-stretcher, which can produce
  // audible warble/crackle. Prefer stable audio and let WebRTC drop late video.
  if (hasLiveAudio) return 1
  if (delayMs === null || delayMs < 100) return 1
  if (delayMs >= 1_000) return 1.2
  if (delayMs >= 500) return 1.12
  if (delayMs >= 250) return 1.08
  return 1.04
}
