// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Shared speed units and phase labels for the menu bar and tray tooltip.
pub fn speed(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "—".into();
    }
    let (number, unit) = if value >= 1e9 {
        (value / 1e9, "GB/s")
    } else if value >= 1e6 {
        (value / 1e6, "MB/s")
    } else if value >= 1e3 {
        (value / 1e3, "KB/s")
    } else {
        return format!("{value:.0} B/s");
    };
    if number < 1000.0 {
        format!("{number:.1} {unit}")
    } else {
        format!("{value:.0e} B/s")
    }
}
pub fn lines(phase: &str, upload: f64, download: f64) -> [String; 2] {
    let state = match phase {
        "paused" => Some("已暂停"),
        "error" => Some("采集异常"),
        "starting" => Some("—"),
        _ => None,
    };
    [
        state.map_or_else(|| speed(upload), str::to_owned),
        state.map_or_else(|| speed(download), str::to_owned),
    ]
}

pub fn tooltip(lines: &[String; 2], status: &str) -> String {
    // Speeds first: long diagnostics must never hide either line in the Windows tooltip.
    let text = format!(
        "净流 PureFlow\n↑ {}\n↓ {}\n{}\n左键查看统计 · 右键快捷操作",
        lines[0], lines[1], status
    );
    #[cfg(windows)]
    {
        // NOTIFYICONDATAW::szTip holds 128 UTF-16 units including its terminator.
        let mut used = 0;
        text.chars()
            .take_while(|c| {
                used += c.len_utf16();
                used <= 127
            })
            .collect()
    }
    #[cfg(not(windows))]
    text
}
