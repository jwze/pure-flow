// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
pub const RUNNING: &str = "正在采集 · 每秒更新";
pub const BASELINE: &str = "已建立基线 · 等待下一个每秒采样";

/// Advance an anchored one-second cadence past completion, never catch up in bursts.
pub fn next_tick(
    deadline: std::time::Instant,
    completed: std::time::Instant,
) -> std::time::Instant {
    let steps = completed
        .saturating_duration_since(deadline)
        .as_secs()
        .saturating_add(1);
    deadline + std::time::Duration::from_secs(steps)
}
