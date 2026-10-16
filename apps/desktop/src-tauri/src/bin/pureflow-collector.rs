// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(windows)]
#[path = "../helper.rs"]
#[allow(dead_code)] // Shared transport includes both GUI and helper roles.
mod helper;
fn main() {
    #[cfg(windows)]
    if let Err(error) = helper::run_helper(std::env::args().skip(1)) {
        eprintln!("PureFlow collector: {error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("This helper is only supported on Windows");
        std::process::exit(1);
    }
}
