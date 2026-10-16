// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
// This module is launched suspended and assigned to the same owner Job as GUI dev.
// It serves browser fixtures only; no collector build or Tauri/ETW launch.
// Start before importing Vite: native bootstrap, imports and server.listen must
// not leave a silent indefinitely-running preview. The owner also bounds the
// pre-JavaScript identity handshake, where a JS timer cannot help.
const startup = setTimeout(() => {
  console.error("Windows browser preview did not become ready within 30 seconds.");
  process.exit(1); // Closing the owned root also closes its Job descendants.
}, 30000);
try {
  const { startWindowsServer } = await import("./dev-vite-windows.mjs");
  await startWindowsServer({ preview: true });
} finally {
  clearTimeout(startup);
}
