// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
// One in-process Vite server: no npm/shell/grandchild chain to orphan on dev shutdown.
import { createServer } from "vite";

if (process.platform === "win32") {
  await import("./dev-launch-preview-windows.mjs");
} else {
const server = await createServer();
let closing;
function close(code = 0) {
  closing ??= server.close().finally(() => { process.exitCode = code; });
  return closing;
}
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.once(signal, () => { void close(); });
}
process.once("uncaughtException", error => {
  console.error(error.message);
  void close(1);
});
process.once("unhandledRejection", error => {
  console.error(error instanceof Error ? error.message : String(error));
  void close(1);
});
await server.listen();
server.printUrls();
}
