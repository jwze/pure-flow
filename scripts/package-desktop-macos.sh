#!/bin/bash
# @author jvz
# PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
# Build the NEW Tauri/Vue Mac graphical client, never the old Swift application.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "Requires an Apple Silicon Mac running a native arm64 terminal." >&2; exit 1
fi
if ! command -v cargo >/dev/null 2>&1 && [[ -x "$PWD/.tooling/cargo/bin/cargo" ]]; then
  export CARGO_HOME="$PWD/.tooling/cargo"
  export RUSTUP_HOME="$PWD/.tooling/rustup"
  export PATH="$CARGO_HOME/bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || { echo "Install Rust/rustup first." >&2; exit 1; }
command -v npm >/dev/null 2>&1 || { echo "Install Node.js 22.19+ or a compatible LTS and npm." >&2; exit 1; }
export MACOSX_DEPLOYMENT_TARGET=13.0
(cd apps/desktop && npm ci)
cargo check --all-targets --workspace --locked
(cd apps/desktop && npm run tauri -- build --target aarch64-apple-darwin --bundles app)
app="target/aarch64-apple-darwin/release/bundle/macos/PureFlow.app"
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
package_id=$(cargo pkgid -p pureflow-desktop)
version="${package_id##*@}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Unexpected version: $package_id" >&2; exit 1; }
mkdir -p dist
asset="PureFlow-$version-macos-arm64.zip"
ditto -c -k --sequesterRsrc --keepParent "$app" "dist/$asset"
(cd dist && shasum -a 256 "$asset" > "$asset.sha256")
echo "NEW graphical app: $app"
echo "Install ZIP: dist/$asset (ad-hoc signed, not notarized)"
