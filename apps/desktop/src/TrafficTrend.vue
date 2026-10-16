<!-- @author jvz -->
<!-- PureFlow watermark: jvz | Original author of PureFlow; see LICENSE. -->
<script setup lang="ts">
import { computed } from "vue";
import type { SamplePoint } from "./lib/types.ts";
import { formatRate, trendPaths } from "./lib/traffic.ts";
const props = defineProps<{ points: SamplePoint[] }>();
const paths = computed(() => trendPaths(props.points, 320, 70));
const timestamp = (point?: SamplePoint) => point ? new Date(point.unix_ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false }) : "—";
</script>
<template>
  <section class="trend" aria-label="下载与上传速度趋势">
    <div class="section-caption"><span><i class="down-dot"></i>下载 <i class="up-dot"></i>上传</span><span>峰值 {{ points.length ? formatRate(paths.max) : "—" }}</span></div>
    <svg v-if="points.some(p => !p.gap)" viewBox="0 0 320 70" role="img" aria-label="速度趋势，空白表示缺测，不作插值">
      <path class="trend-axis" d="M4,64 H316" />
      <path v-for="(path, i) in paths.download" :key="`d${i}`" class="trend-down" :d="path" />
      <path v-for="(path, i) in paths.upload" :key="`u${i}`" class="trend-up" :d="path" />
    </svg>
    <p v-else class="trend-empty">暂无有效趋势采样</p>
    <div class="section-caption"><span>{{ timestamp(points[0]) }}</span><span>{{ timestamp(points.at(-1)) }}</span></div>
    <p class="note">断线留白为缺测，不计作零流量；横轴按采样顺序显示。</p>
  </section>
</template>
