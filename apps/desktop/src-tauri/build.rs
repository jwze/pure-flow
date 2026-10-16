// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
fn main() {
    #[cfg(feature = "desktop")]
    {
        use sha2::{Digest, Sha256};
        let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
        if windows
            && (std::env::var("PROFILE").as_deref() == Ok("release")
                || std::env::var_os("CARGO_FEATURE_CUSTOM_PROTOCOL").is_some())
        {
            let helper = std::path::Path::new("helper/pureflow-collector.exe");
            println!("cargo:rerun-if-changed={}", helper.display());
            let bytes = std::fs::read(helper)
                .expect("Windows release requires build-helper.mjs before GUI build");
            let digest = Sha256::digest(&bytes);
            println!("cargo:rustc-env=PUREFLOW_HELPER_SHA256={digest:x}");
        }
        tauri_build::build();
    }
}
