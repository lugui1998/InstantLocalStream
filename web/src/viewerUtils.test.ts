import { describe, expect, it } from 'vitest'
import { diagnosticRecordingLimitMs, isTerminalSocketDisconnect, playbackRateFor, preferredRecordingMimeType, retryDelayFor, sessionGoodbyeMessage } from './viewerUtils'
import { normalizeCodecName } from './composables/useViewer'

describe('viewer timing policies', () => {
  it('normalizes display and MIME spellings before reporting codec failures', () => {
    expect(normalizeCodecName('H.264')).toBe('h264')
    expect(normalizeCodecName('video/H264')).toBe('h264')
    expect(normalizeCodecName('H.265')).toBe('h265')
    expect(normalizeCodecName('video/HEVC')).toBe('h265')
  })

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
    expect(playbackRateFor(1_000, true)).toBe(1)
  })

  it('explains terminal host session endings without suggesting a retry', () => {
    expect(sessionGoodbyeMessage('host_shutdown')).toBe('The host application closed.')
    expect(sessionGoodbyeMessage('token_changed')).toBe('The host created a new viewer link.')
    expect(sessionGoodbyeMessage('future_reason')).toBe('The host ended this viewing session.')
  })

  it('distinguishes an intentional server disconnect from retryable network loss', () => {
    expect(isTerminalSocketDisconnect('io server disconnect')).toBe(true)
    expect(isTerminalSocketDisconnect('transport close')).toBe(false)
    expect(isTerminalSocketDisconnect('ping timeout')).toBe(false)
  })

  it('prefers an Opus WebM recording and falls back to a supported container', () => {
    expect(preferredRecordingMimeType(mime => mime === 'video/webm;codecs=vp9,opus'))
      .toBe('video/webm;codecs=vp9,opus')
    expect(preferredRecordingMimeType(mime => mime === 'video/mp4'))
      .toBe('video/mp4')
    expect(preferredRecordingMimeType(() => false)).toBe('')
    expect(diagnosticRecordingLimitMs).toBe(30_000)
  })
})
