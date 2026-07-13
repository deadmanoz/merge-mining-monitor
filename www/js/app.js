import { initApp } from "./boot.js?v=0.2.1";

// The static frontend is a graph of ES modules rooted here: importing boot.js
// pulls in every leaf module through real import edges (no globalThis bus). In a
// non-browser context (the node module tests) importing this file links the whole
// graph without booting, which is the import smoke test.
function hasBrowserDocument() {
  return typeof window !== "undefined" && typeof document !== "undefined" && !!document.body;
}

export async function bootApp() {
  await initApp();
}

if (hasBrowserDocument()) {
  bootApp().catch((error) => {
    console.error("merge-mining-monitor frontend failed to boot", error);
  });
}
