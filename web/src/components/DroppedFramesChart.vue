<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import uPlot from 'uplot'
import 'uplot/dist/uPlot.min.css'
import type { PlaybackMetricPoint } from '@/types'

const props = defineProps<{
  samples: PlaybackMetricPoint[]
}>()

const host = ref<HTMLElement | null>(null)
let chart: uPlot | null = null
let resizeObserver: ResizeObserver | null = null

function chartData(): uPlot.AlignedData {
  const samples = props.samples
  const latest = samples.at(-1)?.capturedAt ?? performance.now()
  const windowStart = latest - 15_000
  const timestamps = [windowStart, ...samples.map(sample => sample.capturedAt), latest]
  const dropped = [0, ...samples.map(sample => sample.dropped), 0]
  return [timestamps, dropped]
}

function renderChart() {
  const container = host.value
  if (!container) return
  const width = Math.max(1, Math.floor(container.clientWidth))
  const data = chartData()
  if (chart) {
    chart.setSize({ width, height: 150 })
    chart.setData(data)
    return
  }
  chart = new uPlot({
    width,
    height: 150,
    cursor: { show: false },
    legend: { show: false },
    scales: {
      y: {
        range: (_plot, _min, max) => [0, Math.max(3, Math.ceil(max) + 1)],
      },
    },
    series: [
      {},
      {
        label: 'Dropped frames',
        stroke: '#f07d7d',
        width: 2,
        points: { show: true, size: 7, fill: '#f07d7d' },
      },
    ],
    axes: [
      { show: false },
      {
        stroke: '#89939d',
        grid: { stroke: '#303944', width: 1 },
        ticks: { stroke: '#46515d', width: 1 },
        values: (_plot, values) => values.map(value => `${Math.max(0, Math.round(value))}`),
      },
    ],
  }, data, container)
}

watch(() => props.samples, renderChart, { deep: true })

onMounted(() => {
  renderChart()
  if (host.value) {
    resizeObserver = new ResizeObserver(renderChart)
    resizeObserver.observe(host.value)
  }
})

onBeforeUnmount(() => {
  resizeObserver?.disconnect()
  chart?.destroy()
  chart = null
})
</script>

<template>
  <section class="dropped-frames-chart" aria-label="Dropped frames over the last 15 seconds">
    <div class="chart-header">
      <div>
        <div class="meta-label">Dropped frames</div>
        <div class="chart-subtitle">Last 15 seconds · per one-second sample</div>
      </div>
      <div class="chart-unit">frames / sample</div>
    </div>
    <div ref="host" class="chart-host" />
  </section>
</template>
