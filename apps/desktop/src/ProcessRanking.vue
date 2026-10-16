<!-- @author jvz -->
<!-- PureFlow watermark: jvz | Original author of PureFlow; see LICENSE. -->
<script setup lang="ts">
import { computed } from "vue";
import type { ProcessTraffic, SortKey } from "./lib/types.ts";
import { prepareProcesses, rankedProcesses } from "./lib/traffic.ts";
const props = defineProps<{ processes: ProcessTraffic[]; empty: string; hint: string }>();
const search = defineModel<string>("search", { default: "" });
const order = defineModel<SortKey>("order", { default: "total" });
const prepared = computed(() => prepareProcesses(props.processes));
const rows = computed(() => rankedProcesses(prepared.value, order.value, search.value));
const rendered = computed(() => {
  const maximum = rows.value[0]?.[order.value] ?? 0n;
  return rows.value.slice(0, 100).map(row => ({ ...row,
    metricText: row[`${order.value}Text`],
    width: maximum ? `${Number(row[order.value] * 10000n / maximum) / 100}%` : "0%",
  }));
});
</script>
<template>
  <section class="processes" aria-label="进程排行">
    <div class="section-heading">
      <h1>进程排行 <small>{{ rows.length }}</small></h1>
      <select v-model="order" aria-label="排序方式">
        <option value="total">按总量</option><option value="received">按下载</option><option value="sent">按上传</option>
      </select>
    </div>
    <input v-model="search" maxlength="256" type="search" aria-label="搜索进程" placeholder="搜索进程…" />
    <div class="process-list">
      <div v-for="(row, index) in rendered" :key="row.name" class="process-row">
        <span class="rank">{{ index + 1 }}</span>
        <div class="process-content">
          <div class="row-title"><span :title="row.name">{{ row.name }}</span><strong>{{ row.metricText }}</strong></div>
          <div class="track"><i :style="{ width: row.width }"></i></div>
          <div class="row-details"><span>↓ {{ row.receivedText }}</span><span>↑ {{ row.sentText }}</span></div>
        </div>
      </div>
      <div v-if="!rows.length" class="empty">
        <strong>{{ search ? "没有匹配的进程" : empty }}</strong>
        <p>{{ search ? "试试进程名称的一部分。" : hint }}</p>
      </div>
    </div>
    <p v-if="rows.length > 100" class="limit">仅显示前 100 项，搜索可查找其他进程。</p>
  </section>
</template>
