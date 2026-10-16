<!-- @author jvz -->
<!-- PureFlow watermark: jvz | Original author of PureFlow; see LICENSE. -->
<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, shallowRef, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { History, HistoryQuery, HistoryRange, PanelState, Preferences, Snapshot, SnapshotResponse, SortKey, StorageStatus, Tab, UpdateInfo } from "./lib/types.ts";
import { byteValue, formatBytes, formatRate, totalBytes } from "./lib/traffic.ts";
import { coverageText, dateInput, historyQuery } from "./lib/history.ts";
import { RequestGate } from "./lib/request-gate.ts";
import { RequestLane } from "./lib/request-lane.ts";
import ProcessRanking from "./ProcessRanking.vue";
import TrafficTrend from "./TrafficTrend.vue";
import { version as packageVersion } from "../package.json";

const native = !!(window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
const tab = ref<Tab>("live");
const snapshot = shallowRef<Snapshot | null>(null);
let snapshotRevision: string | null = null;
const liveError = ref("");
const livePending = ref(false);
const shellError = ref("");
const pending = ref(false);
const history = shallowRef<History | null>(null);
const loadedQuery = ref<HistoryQuery | null>(null);
const historyError = ref("");
const historyPending = ref(false);
const range = ref<HistoryRange>("today");
const from = ref(dateInput(new Date()));
const to = ref(from.value);
const network = ref<string | null>(null);
const networks = ref<string[]>([]);
const queryError = ref("");
const liveSearch = ref("");
const liveSort = ref<SortKey>("total");
const historySearch = ref("");
const historySort = ref<SortKey>("total");
const storage = shallowRef<StorageStatus | null>(null);
const storagePending = ref(false);
const storageMessage = ref("");
const storageError = ref("");
const stateError = ref("");
let stateReady = false;
const exportPending = ref(false);
const exportMessage = ref("");
const exportError = ref("");
const preferences = ref<Preferences | null>(null);
const preferencesPending = ref(false);
const preferencesError = ref("");
const autostartPending = ref(false);
const update = ref<UpdateInfo | null>(null);
const updatePending = ref(false);
const updateError = ref("");
const version = ref(packageVersion);
const permissionPending = ref(false);
const permissionMessage = ref("");
async function requestNetworkPermission() {
  if (!canRequest("settings") || permissionPending.value) return;
  permissionPending.value = true;
  const epoch = viewEpoch;
  try {
    const message = await invoke<string>("request_network_permission");
    if (!stopped && epoch === viewEpoch) permissionMessage.value = message;
  }
  catch (e) { if (!stopped && epoch === viewEpoch) permissionMessage.value = String(e); }
  finally { permissionPending.value = false; }
}
const phase = computed(() => !native || liveError.value ? "error" : snapshot.value?.phase ?? "starting");
const phaseText = computed(() => ({ starting: "建立基线", running: "记录中", paused: "已暂停", error: "采集异常" })[phase.value]);
const activeTotal = computed(() => tab.value === "history" ? history.value?.total : snapshot.value?.total);
const total = computed(() => activeTotal.value ? formatBytes(totalBytes(activeTotal.value)) : "—");
const time = computed(() => snapshot.value?.last_sample_ms ? new Date(snapshot.value.last_sample_ms).toLocaleTimeString("zh-CN", { hour12: false }) : "等待采样");
const query = computed(() => {
  try { return historyQuery(range.value, from.value, to.value, network.value); }
  catch { return null; }
});
const historyCoverage = computed(() => history.value && loadedQuery.value ? coverageText(history.value.covered_ms, loadedQuery.value.to_ms - loadedQuery.value.from_ms) : "—");

const liveLane = new RequestLane(busy => { livePending.value = busy; });
const historyLane = new RequestLane(busy => { historyPending.value = busy; });
const preferencesLane = new RequestLane(busy => { preferencesPending.value = busy; });
const updateLane = new RequestLane(busy => { updatePending.value = busy; });
const controlGate = new RequestGate();
const exportGate = new RequestGate();
const autostartGate = new RequestGate();
const shellGate = new RequestGate();
let stateWorker: Promise<void> | null = null;
let queuedState: PanelState | null = null;
const storageLane = new RequestLane(busy => { storagePending.value = busy; });
const unlisteners: UnlistenFn[] = [];
let stopped = false;
let visible = false;
let viewEpoch = 0;
let historyCancellation: Promise<unknown> = Promise.resolve();
let cancellationPending = false;
let aboutCheckPending = false;
let aboutCheckQueued = false;
let aboutFocusPending = false;
function focusAbout() {
  if (!aboutFocusPending || !canRequest("settings")) return;
  const about = document.getElementById("about-pureflow");
  if (!about) return;
  about.scrollIntoView({ block: "start" });
  about.focus({ preventScroll: true });
  aboutFocusPending = false;
}
async function checkAboutNavigation() {
  if (!canRequest()) return;
  aboutCheckQueued = true;
  if (aboutCheckPending) return;
  aboutCheckPending = true;
  try {
    while (aboutCheckQueued && canRequest()) {
      aboutCheckQueued = false;
      if (await invoke<boolean>("take_about_navigation")) {
        if (stopped) return;
        aboutFocusPending = true;
        tab.value = "settings";
        await nextTick();
        focusAbout();
      }
    }
  } catch (e) { if (!stopped) shellError.value = `无法打开关于信息：${String(e)}`; }
  finally { aboutCheckPending = false; }
}
function cancelHistory() {
  if (cancellationPending) return;
  cancellationPending = true;
  historyCancellation = invoke("cancel_history").catch(() => {}).finally(() => { cancellationPending = false; });
}
const canRequest = (target?: Tab) => native && stateReady && visible && !stopped && (!target || tab.value === target);
function refresh() {
  if (!canRequest("live") || pending.value) return;
  liveLane.submitLatest(() => invoke<SnapshotResponse>("get_snapshot", { knownRevision: snapshotRevision }), next => {
    if (next.snapshot) snapshot.value = next.snapshot;
    snapshotRevision = next.revision;
    liveError.value = "";
  }, e => { liveError.value = `本机采集服务不可用：${String(e)}`; });
}
function panelState(): PanelState {
  return { tab: tab.value, range: range.value, from: from.value, to: to.value, network: network.value,
    live_search: liveSearch.value, live_sort: liveSort.value, history_search: historySearch.value, history_sort: historySort.value };
}
function savePanelState(): Promise<void> {
  if (!native || !stateReady || stopped) return Promise.resolve();
  queuedState = panelState();
  if (!stateWorker) {
    stateWorker = (async () => {
      while (queuedState) {
        const state = queuedState; queuedState = null;
        try {
          await invoke<void>("save_panel_state", { state });
          if (!stopped) stateError.value = "";
        } catch (e) {
          if (!stopped) stateError.value = `无法保留页面状态：${String(e)}`;
        }
      }
    })().finally(() => { stateWorker = null; });
  }
  return stateWorker;
}
function loadStorage() {
  if (!canRequest("settings")) return;
  storageLane.submit(() => invoke<StorageStatus>("get_storage_status"), next => { storage.value = next; storageError.value = ""; }, e => { storageError.value = String(e); });
}
async function maintainStorage(command: "clear_cache" | "compact_history") {
  if (!canRequest("settings") || storagePending.value) return;
  storageMessage.value = ""; storageError.value = "";
  const epoch = viewEpoch;
  storageLane.submit(async () => {
    if (command === "clear_cache") await savePanelState();
    return invoke<string>(command);
  }, message => {
    storageMessage.value = message;
    if (epoch === viewEpoch) loadStorage();
  }, e => { storageError.value = String(e); });
}
function loadHistory() {
  if (!canRequest("history")) return;
  if (historyLane.busy) cancelHistory();
  historyLane.invalidate();
  history.value = null;
  loadedQuery.value = null;
  historyError.value = "";
  exportMessage.value = "";
  exportError.value = "";
  queryError.value = "";
  let next: HistoryQuery;
  try { next = historyQuery(range.value, from.value, to.value, network.value); }
  catch (e) { queryError.value = e instanceof Error ? e.message : String(e); return; }
  const epoch = viewEpoch;
  historyLane.submit(async () => {
    // Await cancellation before starting its successor, so a late cancel cannot
    // interrupt the newest query. Do not issue work after hiding/tab switching.
    await historyCancellation;
    if (epoch !== viewEpoch || !canRequest("history")) throw new Error("历史请求已取消");
    return invoke<History>("get_history", { query: next });
  }, value => {
    history.value = value;
    loadedQuery.value = next;
    // Preserve the selected SSID even if the result contains no traffic for it.
    networks.value = [...new Set([...value.networks, ...(next.network === null ? [] : [next.network])])].sort();
  }, e => { historyError.value = String(e); });
}
function loadPreferences() {
  if (!canRequest("settings") || autostartPending.value) return;
  preferencesError.value = "";
  preferencesLane.submit(() => invoke<Preferences>("get_preferences"), value => { preferences.value = value; }, e => { preferencesError.value = String(e); });
}
function invalidateViews() {
  viewEpoch += 1;
  if (native && stateReady && historyLane.busy) cancelHistory();
  liveLane.invalidate(); historyLane.invalidate(); preferencesLane.invalidate(); updateLane.invalidate(); storageLane.invalidate();
  controlGate.invalidate(); exportGate.invalidate(); autostartGate.invalidate();
}
function activate() {
  if (!canRequest()) return;
  if (tab.value === "live") refresh();
  else if (tab.value === "history") loadHistory();
  else { loadPreferences(); loadStorage(); }
}
function setVisible(next: boolean) {
  shellGate.invalidate();
  visible = next;
  invalidateViews();
  if (next) { activate(); focusAbout(); void checkAboutNavigation(); }
}
watch([tab, range, from, to, network, liveSearch, liveSort, historySearch, historySort], savePanelState, { flush: "sync" });
watch(tab, () => { invalidateViews(); activate(); });
watch([range, from, to, network], () => {
  exportGate.invalidate();
  loadHistory();
});
async function action(command: "hide_panel" | "quit_app" | "set_paused" | "retry_sampling", args?: Record<string, unknown>) {
  if (!canRequest() || pending.value) return;
  pending.value = true;
  liveLane.invalidate();
  controlGate.invalidate();
  if (command === "hide_panel" || command === "quit_app") {
    // Stop requests immediately; an eventual visibility event can only confirm it.
    savePanelState();
    invalidateViews();
    visible = false;
  }
  await controlGate.attempt(async () => {
    if (command === "hide_panel" || command === "quit_app") await savePanelState();
    return invoke(command, args);
  }, () => {
    if (command === "hide_panel" || command === "quit_app") return;
    liveError.value = "";
  }, e => {
    if (command === "hide_panel" || command === "quit_app") { visible = true; shellError.value = String(e); }
    else liveError.value = String(e);
  });
  pending.value = false;
  if (canRequest("live")) refresh();
}
async function exportHistory() {
  if (!canRequest("history") || exportPending.value || historyPending.value || !loadedQuery.value) return;
  const selected = { ...loadedQuery.value };
  exportPending.value = true;
  exportMessage.value = ""; exportError.value = "";
  exportGate.invalidate();
  await exportGate.attempt(() => invoke<string | null>("export_history", { query: selected }), path => {
    exportMessage.value = path === null ? "已取消导出。" : `已保存 CSV：${path}`;
  }, e => { exportError.value = String(e); });
  exportPending.value = false;
}
async function setAutostart(event: Event) {
  const checkbox = event.target as HTMLInputElement;
  const enabled = checkbox.checked;
  checkbox.checked = preferences.value?.autostart ?? false;
  if (!canRequest("settings") || autostartPending.value || !preferences.value) return;
  autostartPending.value = true;
  const epoch = viewEpoch;
  preferencesLane.invalidate(); autostartGate.invalidate();
  preferencesError.value = "";
  await autostartGate.attempt(() => invoke<Preferences>("set_autostart", { enabled }), next => { preferences.value = next; }, e => { preferencesError.value = String(e); });
  autostartPending.value = false;
  // Reconcile a mutation that finished while the popup was hidden/tab switched.
  if (epoch !== viewEpoch && canRequest("settings")) loadPreferences();
}
function checkUpdate() {
  if (preferences.value?.updates_via_releases_page) return;
  if (!canRequest("settings") || updatePending.value) return;
  updateError.value = ""; update.value = null;
  updateLane.submit(() => invoke<UpdateInfo>("check_update"), next => { update.value = next; version.value = next.current_version; }, e => { updateError.value = String(e); });
}
function openUpdate() {
  const viaReleasesPage = preferences.value?.updates_via_releases_page === true;
  if (!canRequest("settings") || updatePending.value) return;
  if (!viaReleasesPage && (!update.value?.available || !update.value.url)) return;
  const url = viaReleasesPage ? "https://115613.xyz/gitea/jvz/pure-flow/releases" : update.value!.url!;
  updateError.value = "";
  // Native validates the trusted URL and opens it; WebView never navigates itself.
  updateLane.submit(() => invoke<void>("open_update", { url }), () => {}, e => { updateError.value = String(e); });
}
function tabKey(event: KeyboardEvent) {
  const tabs: Tab[] = ["live", "history", "settings"];
  const index = tabs.indexOf(tab.value);
  const next = event.key === "ArrowRight" ? (index + 1) % 3
    : event.key === "ArrowLeft" ? (index + 2) % 3
    : event.key === "Home" ? 0 : event.key === "End" ? 2 : -1;
  if (next < 0) return;
  event.preventDefault();
  tab.value = tabs[next]!;
  document.getElementById(`tab-${tab.value}`)?.focus();
}
function escape(event: KeyboardEvent) { if (event.key === "Escape") void action("hide_panel"); }
onMounted(async () => {
  document.addEventListener("keydown", escape);
  if (!native) return;
  try {
    const off = await listen<boolean>("panel-visibility", event => { setVisible(event.payload); });
    if (stopped) { off(); return; }
    unlisteners.push(off);
    const offSnapshot = await listen<string>("snapshot-updated", event => {
      if (event.payload !== snapshotRevision) refresh();
    });
    if (stopped) { offSnapshot(); return; }
    unlisteners.push(offSnapshot);
    const offError = await listen<string>("panel-error", event => { shellError.value = event.payload; });
    if (stopped) { offError(); return; }
    unlisteners.push(offError);
    const offAbout = await listen("panel-about", () => { void checkAboutNavigation(); });
    if (stopped) { offAbout(); return; }
    unlisteners.push(offAbout);
    try {
      const state = await invoke<PanelState>("get_panel_state");
      if (stopped) return;
      tab.value = state.tab; range.value = state.range;
      from.value = state.from || from.value; to.value = state.to || to.value;
      network.value = state.network; networks.value = state.network === null ? [] : [state.network];
      liveSearch.value = state.live_search; liveSort.value = state.live_sort;
      historySearch.value = state.history_search; historySort.value = state.history_sort;
    } catch (e) { stateError.value = String(e); }
    // Flush restore watchers while requests and state saves remain disabled.
    await nextTick();
    if (stopped) return;
    stateReady = true;
    await shellGate.attempt(() => invoke<{ panel_visible: boolean; cache_error?: string | null; panel_error?: string | null }>("get_shell_state"), state => {
      if (state.cache_error) shellError.value = `缓存清理未完成：${state.cache_error}`;
      else if (state.panel_error) shellError.value = state.panel_error;
      setVisible(state.panel_visible);
    }, e => { shellError.value = `无法读取弹层状态：${String(e)}`; });
    // A visibility event during this read already activates its retained state.
    const current = await getVersion();
    if (!stopped) version.value = current;
  } catch (e) { if (!stopped) shellError.value = String(e); }
});
onUnmounted(() => {
  stopped = true; visible = false; shellGate.invalidate(); invalidateViews();
  for (const off of unlisteners) off();
  document.removeEventListener("keydown", escape);
});
</script>

<template>
  <main class="popup">
    <header class="header">
      <div class="brand"><span class="mark" aria-hidden="true">⌁</span><strong>净流 <small>PureFlow</small></strong></div>
      <div class="header-actions">
        <span class="status" :class="phase"><i></i>{{ native ? phaseText : '浏览器预览' }}</span>
        <button class="icon-button" aria-label="收起统计弹层" title="收起 · Esc" :disabled="!native || pending" @click="action('hide_panel')">×</button>
      </div>
    </header>
    <nav class="tabs" role="tablist" aria-label="统计页面" @keydown="tabKey">
      <button v-for="item in ([{ id: 'live', label: '实时' }, { id: 'history', label: '历史' }, { id: 'settings', label: '设置' }] as const)" :id="`tab-${item.id}`" :key="item.id" role="tab" :tabindex="tab === item.id ? 0 : -1" :aria-selected="tab === item.id" :aria-controls="`panel-${item.id}`" :class="{ selected: tab === item.id }" @click="tab = item.id">{{ item.label }}</button>
    </nav>
    <div class="body">
      <p v-if="!native" class="preview-note">浏览器预览不采集、不伪造数据。请启动 净流 PureFlow，从菜单栏／托盘打开弹层。</p>
      <p v-if="shellError || stateError" class="error" role="alert">{{ shellError || stateError }}</p>
      <section :id="`panel-${tab}`" role="tabpanel" :aria-labelledby="`tab-${tab}`">
        <template v-if="tab === 'live'">
          <div v-if="liveError || snapshot?.phase === 'error'" role="alert" class="error">
            <p>{{ liveError || snapshot?.status }}</p>
            <p>Windows 每次启动需为独立采集 helper 授权 UAC。拒绝后不会自动反复请求；准备好后手动重试。</p>
            <button :disabled="!native || pending" @click="action('retry_sampling')">{{ pending ? '请求中…' : '重新采集' }}</button>
          </div>
          <p v-if="native && livePending && !snapshot" class="loading" role="status">正在读取采集状态…</p>
          <section class="summary" aria-label="本次运行累计流量" :aria-busy="livePending || pending">
            <div class="section-caption"><span>本次运行累计 · 全网络</span><span>覆盖 {{ snapshot ? coverageText(snapshot.covered_ms) : '—' }}</span></div>
            <div class="session-total">{{ total }}</div>
            <div class="totals"><span>↓ 下载 <strong>{{ activeTotal ? formatBytes(byteValue(activeTotal.received)) : '—' }}</strong></span><span>↑ 上传 <strong>{{ activeTotal ? formatBytes(byteValue(activeTotal.sent)) : '—' }}</strong></span></div>
            <div class="rates">
              <span><i class="down-dot"></i>下载速度<strong>{{ snapshot && phase === 'running' ? formatRate(snapshot.download_bps) : '—' }}</strong></span>
              <span><i class="up-dot"></i>上传速度<strong>{{ snapshot && phase === 'running' ? formatRate(snapshot.upload_bps) : '—' }}</strong></span>
            </div>
          </section>
          <TrafficTrend :points="snapshot?.points ?? []" />
          <ProcessRanking v-model:search="liveSearch" v-model:order="liveSort" :processes="snapshot?.processes ?? []" :empty="snapshot?.paused ? '统计已暂停' : '等待进程流量记录'" hint="先建立整机基线；进程预热与批次确认可能稍晚，排行为本次运行累计。" />
          <details class="diagnostics">
            <summary>统计说明与采集提示 <span>{{ snapshot?.warnings.length ?? 0 }}</span></summary>
            <p>实时显示本次运行累计；历史从本机数据库独立查询。暂停、休眠及采集异常期间不计为有效覆盖。</p>
            <p>整机来自物理网卡；进程可能包含代理／VPN 转发，请勿相加当作整机账单。短时进程可能漏计。</p>
            <p v-if="snapshot?.status">{{ snapshot.status }}</p>
            <ul><li v-for="warning in snapshot?.warnings ?? []" :key="warning">{{ warning }}</li></ul>
          </details>
        </template>
        <template v-else-if="tab === 'history'">
          <section class="history-filters" aria-label="历史查询条件">
            <div class="filter-row">
              <select v-model="range" aria-label="历史时段"><option value="today">今日</option><option value="yesterday">昨日</option><option value="week">最近 7 天</option><option value="custom">自定义日期</option></select>
              <select v-model="network" aria-label="历史网络 SSID"><option :value="null">全网络</option><option v-for="ssid in networks" :key="ssid" :value="ssid">{{ ssid }}</option></select>
              <button :disabled="!native || historyPending" @click="loadHistory">刷新</button>
            </div>
            <div v-if="range === 'custom'" class="date-row"><label>从 <input v-model="from" type="date" aria-label="开始日期" /></label><label>至 <input v-model="to" type="date" aria-label="结束日期" /></label></div>
            <p class="note">按本机日期，含结束日。全网络为后端整机汇总，不叠加 SSID；选定 SSID 只查该网络。</p>
          </section>
          <p v-if="queryError || historyError" class="error" role="alert">{{ queryError || historyError }}</p>
          <p v-if="historyPending" class="loading" role="status">正在读取本机历史…</p>
          <section class="summary" aria-label="历史累计流量" :aria-busy="historyPending">
            <div class="section-caption"><span class="network-label" :title="network ?? '全网络'">{{ network ?? '全网络' }} · 所选时段</span><button :disabled="!native || !history || historyPending || exportPending || !query" @click="exportHistory">{{ exportPending ? '保存中…' : '导出 CSV' }}</button></div>
            <div class="session-total">{{ total }}</div>
            <div class="totals"><span>↓ 下载 <strong>{{ activeTotal ? formatBytes(byteValue(activeTotal.received)) : '—' }}</strong></span><span>↑ 上传 <strong>{{ activeTotal ? formatBytes(byteValue(activeTotal.sent)) : '—' }}</strong></span></div>
            <p class="coverage">有效覆盖 {{ historyCoverage }}</p>
            <p class="note">覆盖仅指有效观测时间；未覆盖不等于无流量，今日包含尚未发生的时间。</p>
          </section>
          <p v-if="exportMessage" class="message" role="status">{{ exportMessage }}</p><p v-if="exportError" class="error" role="alert">{{ exportError }}</p>
          <TrafficTrend :points="history?.points ?? []" />
          <ProcessRanking v-model:search="historySearch" v-model:order="historySort" :processes="history?.processes ?? []" :empty="history ? '此时段暂无进程记录' : '尚无历史结果'" hint="调整日期或网络后重试；缺测不代表零流量。" />
          <p class="note">CSV 使用原生保存对话框选择位置；取消不会写入文件。进程排行不是整机总量的分摊账单。</p>
        </template>
        <template v-else>
          <section id="about-pureflow" class="settings-card" tabindex="-1" aria-labelledby="about-title">
            <div class="section-heading"><h1 id="about-title">关于 PureFlow</h1><span class="note">v{{ version }}</span></div>
            <p><strong>净流 PureFlow</strong> · 本地流量与进程统计</p>
            <p>Developed by jvz</p>
            <p class="note">Copyright (c) 2026 jvz (PureFlow) · 原作者 jvz；MIT 许可见 LICENSE。</p>
          </section>
          <section class="settings-card">
            <h1>启动与采集</h1>
            <label class="checkbox-label"><input type="checkbox" :checked="preferences?.autostart ?? false" :disabled="!native || !preferences || preferencesPending || autostartPending" @change="setAutostart" /> 登录时启动净流</label>
            <p v-if="preferencesPending || autostartPending" class="note" role="status">{{ autostartPending ? '正在保存…' : '正在读取设置…' }}</p>
            <div v-if="preferencesError" class="error" role="alert"><p>{{ preferencesError }}</p><button :disabled="!native || preferencesPending || autostartPending" @click="loadPreferences">重试读取</button></div>
            <p>登录启动不绕过权限。Windows 每次启动（包括登录启动）都会为独立采集器请求 UAC；界面始终保持普通权限。暂停/继续复用授权，不重复提权。</p>
            <p>拒绝授权会显示采集异常；到「实时」手动重新采集，不自动反复弹出 UAC。</p>
          </section>
          <section class="settings-card">
            <div class="section-heading"><h1>版本与更新</h1><span class="note">{{ version ? `v${version}` : '版本待读取' }}</span></div>
            <div v-if="preferences?.updates_via_releases_page" class="update-actions"><button :disabled="!native || updatePending" @click="openUpdate">{{ updatePending ? '正在打开…' : '打开发布页' }}</button></div>
            <template v-else>
              <div class="update-actions"><button :disabled="!native || updatePending" @click="checkUpdate">{{ updatePending ? '请求中…' : '检查更新' }}</button><button v-if="update?.available && update.url" :disabled="updatePending" @click="openUpdate">前往更新</button></div>
              <p v-if="update" role="status">{{ update.status }}<span v-if="update.latest_version"> · 最新 v{{ update.latest_version }}</span></p>
            </template>
            <p v-if="updateError" class="error" role="alert">{{ updateError }}</p>
            <p v-if="preferences?.updates_via_releases_page">仅在点击时用系统浏览器打开 Gitea 发布页；应用不查询在线版本，不自动下载安装。</p>
            <p v-else>检查更新会访问更新服务；仅在检查后打开受信任下载页面，不自动下载安装。</p>
          </section>
          <section class="settings-card">
            <h1>Wi-Fi 网络归属</h1>
            <p>SSID 仅用于本地历史筛选。系统可能要求位置隐私权限；不记录位置坐标。拒绝后仍统计全网络，多网卡归属不明确时不标记 SSID。</p>
            <button :disabled="!native || permissionPending" @click="requestNetworkPermission">{{ permissionPending ? '请求中…' : 'Wi-Fi 隐私授权 / 设置' }}</button>
            <p v-if="permissionMessage" role="status">{{ permissionMessage }}</p>
          </section>
          <section class="settings-card">
            <div class="section-heading"><h1>本地记录与隐私</h1><button :disabled="!native || storagePending" @click="loadStorage">{{ storagePending ? '处理中…' : '刷新占用' }}</button></div>
            <template v-if="storage">
              <p>历史数据库 {{ formatBytes(BigInt(storage.database_bytes)) }} · 界面缓存 {{ formatBytes(BigInt(storage.cache_bytes)) }}</p>
              <p class="note">数据目录：{{ storage.data_path }}<br />缓存目录：{{ storage.cache_path }}</p>
            </template>
            <div class="update-actions"><button :disabled="!native || storagePending" @click="maintainStorage('clear_cache')">清理界面缓存</button><button :disabled="!native || storagePending || !storage?.paused" @click="maintainStorage('compact_history')">压缩历史数据库</button></div>
            <p class="note">清缓存将在弹层连续收起 60 秒后释放界面并清理；重新打开会延后，不删除历史。数据库压缩需先暂停记录；仅回收空闲空间，不删除记录。占用仅手动刷新，不后台轮询。</p>
            <p v-if="storageMessage" class="message" role="status">{{ storageMessage }}</p><p v-if="storageError" class="error" role="alert">{{ storageError }}</p>
            <p>流量与进程统计保存在本机数据库，不上传流量记录；不读取网页内容。无账号、无云端同步。</p>
            <p><strong>分钟记录保留 30 天，日汇总保留 365 天。</strong>较早时段的趋势与覆盖以可用日汇总为准，不补造分钟采样。</p>
            <p>实时累计随本次运行重新开始，历史不会因退出或暂停而清空。导出仅由你在原生保存对话框中选定位置。</p>
            <p class="note">macOS / Windows 均每秒更新实时数据，常规历史约每 10 秒批量保存；查询、导出、暂停及正常退出提前提交。实时累计不代表全部已保存。异常终止可能丢失最近尚未落盘的记录，磁盘阻塞时可能超过 10 秒；存储失败需显式重试，不静默丢弃待写数据。暂停仅安排提前保存，不等待磁盘写入完成。</p>
          </section>
        </template>
      </section>
    </div>
    <footer class="footer">
      <span class="last-sample">{{ tab === 'live' ? `${time} · ${snapshot?.phase === 'running' ? snapshot.status : '等待有效采样'}` : tab === 'history' ? '本地历史 · 缺测不补零' : '净流 PureFlow · 本地优先' }}</span>
      <div><button :disabled="!native || tab !== 'live' || pending || !snapshot || snapshot.phase === 'error' || snapshot.phase === 'starting'" @click="action('set_paused', { paused: !snapshot?.paused })">{{ pending ? '请求中…' : snapshot?.paused ? '继续记录' : '暂停记录' }}</button><button class="quiet" :disabled="!native || pending" @click="action('quit_app')">退出</button></div>
    </footer>
  </main>
</template>
