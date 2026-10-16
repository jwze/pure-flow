// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
// Windows-only npm preview bridge. EOF closes its own Job owner on parent loss.
import { spawn } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const powershell = resolve(process.env.SystemRoot ?? "C:\\Windows", "System32/WindowsPowerShell/v1.0/powershell.exe");
const owner = spawn(powershell, [
  "-NoLogo", "-NoProfile", "-NonInteractive", "-File",
  resolve(desktop, "scripts/dev-windows.ps1"), "-Preview", "-PreviewParentInput",
], { cwd: desktop, stdio: ["pipe", "inherit", "inherit"], windowsHide: true });
let failed = false;
let stopping = false;
function stopOwner(code = 0) {
  failed ||= code !== 0;
  if (!stopping) {
    stopping = true;
    owner.stdin.end();
  }
}
for (const event of ["uncaughtException", "unhandledRejection"]) {
  process.once(event, error => {
    console.error("Windows preview bridge failed:", error);
    stopOwner(1);
  });
}
process.once("exit", () => { owner.stdin.destroy(); });
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.once(signal, () => { stopOwner(); });
}
owner.stdin.on("error", error => {
  if (error.code !== "EPIPE" && error.code !== "ERR_STREAM_DESTROYED") {
    console.error("Windows preview owner input failed:", error);
    failed = true;
  }
});
owner.once("error", error => {
  console.error("Windows preview owner could not start:", error);
  failed = true;
  process.exitCode = 1;
});
owner.once("exit", code => { process.exitCode = failed ? 1 : (code ?? 1); });
