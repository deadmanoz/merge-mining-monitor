import { centerCameraOnHeight } from "./api-client.js?v=0.9.3";
import { state, writeForm } from "./frontend-state.js?v=0.9.3";
import { activateHeightLookup } from "./tree-query-state.js?v=0.9.3";

async function showInTree(height) {
  state.navEpoch += 1;
  const epoch = state.navEpoch;
  activateHeightLookup(height);
  writeForm();
  // Dynamic import avoids a static module cycle with view-shell.
  const shell = await import("./view-shell.js?v=0.9.3");
  await shell.activateView("tree");
  if (state.navEpoch !== epoch) return;
  if (state.query.view !== "tree" || state.query.treeHeight !== String(height)) return;
  centerCameraOnHeight(String(height));
}

export { showInTree };
