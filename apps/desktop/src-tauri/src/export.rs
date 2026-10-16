// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Native user-selected CSV destination. No WebView-provided file path.
use std::path::PathBuf;

pub fn select_csv_path() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_file_name("PureFlow-history.csv")
        .add_filter("CSV", &["csv"])
        .save_file()
}
pub fn save_selected_csv(path: Option<PathBuf>, csv: &str) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    std::fs::write(&path, csv).map_err(|error| format!("无法保存 CSV：{error}"))?;
    Ok(Some(path.display().to_string()))
}
pub fn save_csv(csv: &str) -> Result<Option<String>, String> {
    save_selected_csv(select_csv_path(), csv)
}
