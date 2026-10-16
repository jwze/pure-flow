// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { mkdirSync, copyFileSync, renameSync, rmSync } from "node:fs";

// The WebView never launches a privileged shell. This builds the fixed, separately
// elevated collector executable; Tauri's Windows bundle places it beside the GUI.
if (process.platform === "win32") {
  const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
  const dev = process.argv.includes("--dev");
  const args = ["build", "--locked", "-p", "pureflow-desktop", "--no-default-features", "--features", "windows-collector", "--bin", "pureflow-collector"];
  if (!dev) args.push("--release", "--target", "x86_64-pc-windows-msvc");
  const result = spawnSync("cargo", args, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
  if (!dev) {
    const destination = resolve(root, "apps/desktop/src-tauri/helper");
    mkdirSync(destination, { recursive: true });
    const temporary = resolve(destination, "pureflow-collector.exe.tmp");
    try {
      copyFileSync(resolve(root, "target/x86_64-pc-windows-msvc/release/pureflow-collector.exe"), temporary);
      renameSync(temporary, resolve(destination, "pureflow-collector.exe"));
    } finally {
      rmSync(temporary, { force: true });
    }
  }
}
