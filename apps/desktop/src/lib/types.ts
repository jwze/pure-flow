// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
export interface Bytes {
  received: string;
  sent: string;
}

export interface ProcessTraffic {
  name: string;
  bytes: Bytes;
}

export interface SamplePoint {
  unix_ms: number;
  download_bps: number;
  upload_bps: number;
  gap: boolean;
}

export interface Snapshot {
  phase: "starting" | "running" | "paused" | "error";
  paused: boolean;
  status: string;
  started_at_ms: number;
  last_sample_ms: number | null;
  covered_ms: number;
  total: Bytes;
  download_bps: number;
  upload_bps: number;
  processes: ProcessTraffic[];
  points: SamplePoint[];
  warnings: string[];
}

export interface SnapshotResponse {
  revision: string;
  snapshot: Snapshot | null;
}
export interface StorageStatus {
  data_path: string;
  cache_path: string;
  database_bytes: number;
  cache_bytes: number;
  paused: boolean;
  phase: string;
}
export interface PanelState {
  tab: Tab;
  range: HistoryRange;
  from: string;
  to: string;
  network: string | null;
  live_search: string;
  live_sort: SortKey;
  history_search: string;
  history_sort: SortKey;
}
export type SortKey = "total" | "received" | "sent";
export type Tab = "live" | "history" | "settings";
export type HistoryRange = "today" | "yesterday" | "week" | "custom";

// The backend treats to_ms as exclusive; network=null is the authoritative
// all-network aggregate, never a sum of SSID results in the WebView.
export interface HistoryQuery {
  from_ms: number;
  to_ms: number;
  network: string | null;
}
export interface History {
  total: Bytes;
  covered_ms: number;
  processes: ProcessTraffic[];
  points: SamplePoint[];
  networks: string[];
}
export interface Preferences {
  autostart: boolean;
  updates_via_releases_page?: boolean;
}
export interface UpdateInfo {
  current_version: string;
  latest_version: string | null;
  available: boolean;
  url: string | null;
  status: string;
}
