import { initApp } from "./boot.js?v=0.8.0";

// The static frontend is a graph of ES modules rooted here: importing boot.js
// pulls in every leaf module through real import edges (no globalThis bus).
function hasBrowserDocument() {
  return typeof window !== "undefined" && typeof document !== "undefined" && !!document.body;
}

async function bootApp() {
  await initApp();
}

if (hasBrowserDocument()) {
  bootApp().catch((error) => {
    console.error("merge-mining-monitor frontend failed to boot", error);
  });
}
