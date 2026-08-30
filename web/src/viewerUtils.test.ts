import { describe, expect, it } from 'vitest'
import { playbackRateFor, retryDelayFor } from './viewerUtils'

describe('viewer timing policies', () => {
  it('backs off retries with a fixed ceiling', () => {
    expect(retryDelayFor(0)).toBe(1_000)
    expect(retryDelayFor(2)).toBe(5_000)
    expect(retryDelayFor(99)).toBe(30_000)
    expect(retryDelayFor(-1)).toBe(1_000)
  })

  it('accelerates playback only when the receiver has fallen behind', () => {
    expect(playbackRateFor(null)).toBe(1)
    expect(playbackRateFor(99)).toBe(1)
    expect(playbackRateFor(100)).toBe(1.04)
    expect(playbackRateFor(250)).toBe(1.08)
    expect(playbackRateFor(500)).toBe(1.12)
    expect(playbackRateFor(1_000)).toBe(1.2)
  })
})
