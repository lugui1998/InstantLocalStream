export const retryDelays = [1_000, 2_000, 5_000, 10_000, 20_000, 30_000] as const

export function retryDelayFor(attempt: number) {
  const index = Math.max(0, Math.min(Math.trunc(attempt), retryDelays.length - 1))
  return retryDelays[index]
}

export function playbackRateFor(delayMs: number | null) {
  if (delayMs === null || delayMs < 100) return 1
  if (delayMs >= 1_000) return 1.2
  if (delayMs >= 500) return 1.12
  if (delayMs >= 250) return 1.08
  return 1.04
}
