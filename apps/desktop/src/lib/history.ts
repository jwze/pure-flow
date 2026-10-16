// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
import type { HistoryQuery, HistoryRange } from "./types.ts";

export function dateInput(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}
function localDate(value: string): Date {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!match) throw new Error("请选择完整日期。" );
  const date = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
  if (dateInput(date) !== value) throw new Error("日期无效。" );
  return date;
}
export function historyQuery(
  range: HistoryRange, from: string, to: string, network: string | null, now = new Date(),
): HistoryQuery {
  const start = range === "custom" ? localDate(from) : new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const end = range === "custom" ? localDate(to) : new Date(start);
  if (range === "yesterday") {
    start.setDate(start.getDate() - 1);
    // end is today's midnight
  } else {
    if (range === "week") start.setDate(start.getDate() - 6);
    end.setDate(end.getDate() + 1);
  }
  if (start.getTime() >= end.getTime()) throw new Error("开始日期不能晚于结束日期。" );
  return { from_ms: start.getTime(), to_ms: end.getTime(), network };
}
export function coverageText(covered: number, duration?: number): string {
  const ms = Math.max(0, Number.isFinite(covered) ? covered : 0);
  const hours = ms / 3600000;
  const label = hours >= 1 ? `${hours.toFixed(1)} 小时` : `${(ms / 60000).toFixed(1)} 分钟`;
  return duration && duration > 0
    ? `${label} · ${Math.min(100, ms / duration * 100).toFixed(1)}% 所选时段`
    : label;
}
