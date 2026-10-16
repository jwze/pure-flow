// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
// Windows-only beforeDevCommand. Direct `tauri dev` must not launch unowned Vite.
import { spawn } from "node:child_process";
import { connect } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pipe = process.env.PUREFLOW_WINDOWS_DEV_PIPE;
if (process.platform !== "win32" || !/^\\\\\.\\pipe\\PureFlow-dev-[a-f0-9]{32}$/.test(pipe ?? "")) {
  console.error("Windows dev requires the lifecycle owner. Run npm run dev:windows in apps/desktop (not tauri dev).");
  process.exit(1);
}

let server;
let monitor;
let stopping = false;
let closing;
let failure = 0;
let startupTimer;
let shutdownTimer;

function close(code = 0) {
  failure = Math.max(failure, code);
  process.exitCode = failure;
  stopping = true;
  clearTimeout(startupTimer);
  // Bound shutdown even if a plugin or another startup operation stalls.
  shutdownTimer ??= setTimeout(() => process.exit(failure || 1), 5000);
  shutdownTimer.unref();
  closing ??= Promise.resolve().then(async () => {
    try {
      await server?.close();
    } catch (error) {
      console.error("Windows Vite shutdown failed:", error);
      failure = 1;
    } finally {
      // EOF lets the handle watcher release its owner handle without killing it.
      monitor?.stdin.end();
      process.exitCode = failure;
    }
  });
  return closing;
}
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.once(signal, () => { void close(); });
}
for (const event of ["uncaughtException", "unhandledRejection"]) {
  process.once(event, error => {
    console.error("Windows dev lifecycle failed:", error);
    void close(1);
  });
}

function ownerIdentity() {
  return new Promise((resolveIdentity, reject) => {
    const socket = connect(pipe);
    let data = "";
    const timer = setTimeout(() => {
      socket.destroy(new Error("Timed out binding the Windows dev lifecycle owner. Run npm run dev:windows."));
    }, 10000);
    socket.setEncoding("utf8");
    socket.on("data", chunk => {
      data += chunk;
      if (data.length > 128) socket.destroy(new Error("Invalid Windows dev owner identity."));
      else if (data.includes("\n")) {
        const match = /^(\d{1,10}) (\d{1,20})\n$/.exec(data);
        if (match) {
          socket.end("\n");
          resolveIdentity({ pid: match[1], birth: match[2] });
        } else {
          socket.destroy();
          reject(new Error("Invalid Windows dev owner identity."));
        }
      }
    });
    socket.once("error", reject);
    socket.once("close", () => {
      clearTimeout(timer);
      reject(new Error("Windows dev owner disconnected before sending its identity."));
    });
  });
}

function bindOwner({ pid, birth }) {
  return new Promise((ready, reject) => {
    const powershell = resolve(process.env.SystemRoot ?? "C:\\Windows", "System32/WindowsPowerShell/v1.0/powershell.exe");
    monitor = spawn(powershell, [
      "-NoLogo", "-NoProfile", "-NonInteractive",
      "-File", resolve(desktop, "scripts/dev-windows.ps1"),
      "-WatchOwner", "-OwnerId", pid, "-OwnerBirth", birth,
    ], { cwd: desktop, windowsHide: true, stdio: ["pipe", "pipe", "inherit"] });
    let output = "";
    let watcherReady = false;
    startupTimer = setTimeout(() => {
      reject(new Error("Timed out opening the actual Tauri owner process handle."));
    }, 15000);
    monitor.stdout.setEncoding("utf8");
    monitor.stdout.on("data", chunk => {
      output += chunk;
      if (output === "OWNER_READY\r\n" || output === "OWNER_READY\n") {
        clearTimeout(startupTimer);
        watcherReady = true;
        ready();
      } else if (output.length > 256) {
        reject(new Error("Invalid Windows owner watcher response."));
        void close(1);
      }
    });
    monitor.stdin.on("error", error => {
      if (!stopping) {
        if (!watcherReady) reject(error);
        void close(1);
      }
    });
    monitor.once("error", error => {
      if (!watcherReady) reject(error);
      if (!stopping) void close(1);
    });
    monitor.once("exit", code => {
      clearTimeout(startupTimer);
      if (!watcherReady) reject(new Error(`Windows owner watcher exited before readiness (code ${code}).`));
      if (!stopping) {
        if (code !== 10) console.error(`Windows owner watcher failed (code ${code}); refusing to keep Vite running.`);
        void close(code === 10 ? 0 : 1);
      }
    });
  });
}

function buildHelper() {
  return new Promise((done, reject) => {
    // This and cargo inherit the launcher's job; never detached or shell-wrapped.
    const helper = spawn(process.execPath, [resolve(desktop, "scripts/build-helper.mjs"), "--dev"], {
      cwd: desktop, stdio: "inherit",
    });
    helper.once("error", reject);
    helper.once("exit", code => {
      if (code === 0) done();
      else reject(new Error(`Windows collector build failed (code ${code}).`));
    });
  });
}

export async function startWindowsServer({ preview = false } = {}) {
try {
  await bindOwner(await ownerIdentity());
  if (!stopping && !preview) await buildHelper();
  if (!stopping) {
    // Preserve shared/Mac config. Windows must never choose another host/port.
    server = await createServer({
      root: desktop,
      server: { host: "127.0.0.1", port: 1420, strictPort: true, open: false, watch: { ignored: ["**/src-tauri/**"] } },
    });
    if (process.env.PUREFLOW_WINDOWS_DEV_DIAGNOSTICS === "1") {
      console.error("PureFlow Windows Vite config:", {
        pid: process.pid, mode: preview ? "browser-preview" : "tauri-dev",
        usePolling: server.config.server.watch?.usePolling ?? process.env.CHOKIDAR_USEPOLLING ?? "default (native)",
        interval: server.config.server.watch?.interval ?? process.env.CHOKIDAR_INTERVAL ?? "default",
        ignored: server.config.server.watch?.ignored,
      });
    }
    if (stopping) await server.close();
    else {
      await server.listen();
      if (stopping) await server.close();
      else server.printUrls();
      if (!stopping && process.env.PUREFLOW_WINDOWS_DEV_DIAGNOSTICS === "1") {
        const watched = server.watcher.getWatched();
        console.error("PureFlow Windows watcher summary:", {
          directories: Object.keys(watched).length,
          entries: Object.values(watched).reduce((count, files) => count + files.length, 0),
        });
      }
    }
  }
} catch (error) {
  console.error("Windows dev startup failed:", error instanceof Error ? error.message : error);
  await close(1);
}
}
