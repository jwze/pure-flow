// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
import type { Bytes, ProcessTraffic, SamplePoint, SortKey } from "./types.ts";

export function byteValue(value: string): bigint {
  if (!/^\d+$/.test(value) || value.length > 20)
    throw new Error("无效的字节计数");
  const result = BigInt(value);
  if (result > 18446744073709551615n) throw new Error("字节计数超出 u64");
  return result;
}
export function totalBytes(bytes: Bytes): bigint {
  return byteValue(bytes.received) + byteValue(bytes.sent);
}
export function formatBytes(value: bigint): string {
  const units = [
    ["TB", 1000000000000n],
    ["GB", 1000000000n],
    ["MB", 1000000n],
    ["KB", 1000n],
  ] as const;
  for (const [unit, divisor] of units) {
    if (value >= divisor) {
      const rounded = (value * 100n + divisor / 2n) / divisor;
      return `${rounded / 100n}.${(rounded % 100n).toString().padStart(2, "0")} ${unit}`;
    }
  }
  return `${value} B`;
}
export function formatRate(value: number): string {
  if (!Number.isFinite(value) || value < 0) return "—";
  return `${formatBytes(BigInt(Math.round(value)))}/s`;
}
export interface PreparedProcess {
  name: string;
  needle: string;
  received: bigint;
  sent: bigint;
  total: bigint;
  receivedText: string;
  sentText: string;
  totalText: string;
}
// Parse and format each counter once per snapshot, not inside sort/render loops.
export function prepareProcesses(rows: ProcessTraffic[]): PreparedProcess[] {
  return rows.map(row => {
    const received = byteValue(row.bytes.received);
    const sent = byteValue(row.bytes.sent);
    const total = received + sent;
    return { name: row.name, needle: row.name.toLocaleLowerCase(), received, sent, total,
      receivedText: formatBytes(received), sentText: formatBytes(sent), totalText: formatBytes(total) };
  });
}
export function rankedProcesses(
  rows: PreparedProcess[], order: SortKey, search: string,
): PreparedProcess[] {
  const needle = search.trim().toLocaleLowerCase();
  return rows.filter(row => row.needle.includes(needle)).sort((a, b) =>
    a[order] === b[order] ? a.name.localeCompare(b.name) : a[order] > b[order] ? -1 : 1);
}
export function trendPaths(
  points: SamplePoint[],
  width = 700,
  height = 148,
): { download: string[]; upload: string[]; max: number } {
  const result = { download: [] as string[], upload: [] as string[], max: 1 };
  if (!points.length) return result;
  const rates = points
    .filter((p) => !p.gap)
    .flatMap((p) => [p.download_bps, p.upload_bps])
    .filter((v) => Number.isFinite(v) && v >= 0);
  result.max = rates.reduce((max, rate) => Math.max(max, rate), 1);
  // Sample order is stable even when the wall clock changes; timestamps are labels only.
  const duration = Math.max(1, points.length - 1);
  for (const [field, target] of [
    ["download_bps", result.download],
    ["upload_bps", result.upload],
  ] as const) {
    let path = "";
    const flush = () => {
      // A zero-length segment with round linecap keeps isolated samples visible.
      if (path) target.push(path.includes(" L") ? path : `${path} L${path.slice(1)}`);
      path = "";
    };
    for (const [index, point] of points.entries()) {
      const rate = point[field];
      if (point.gap || !Number.isFinite(rate) || rate < 0) {
        flush();
        continue;
      }
      const x = 4 + (index / duration) * (width - 8);
      const y = height - 6 - Math.min(1, rate / result.max) * (height - 16);
      path += `${path ? " L" : "M"}${x.toFixed(2)},${y.toFixed(2)}`;
    }
    flush();
  }
  return result;
}
