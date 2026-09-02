// The shared cross-link gesture: jump to the tree view at a BTC height.
//
// Extracted from the delta view so the findings view's evidence anchors can
// reuse it (a copy would trip the jscpd duplication gate, and the staleness
// guards below are exactly the part a re-implementation would get wrong).
//
// activateHeightLookup only rewrites query state, so on its own this would
// leave the Height box empty and the camera wherever it was: renderTree
// reapplies the stored pan/zoom, so after any earlier tree use the target can
// land off-screen. Mirror the rail's own commit instead: fill the control,
// activate (which loads, because the mutator marked the tree dirty), then
// centre once the window exists.

import { centerCameraOnHeight } from "./api-client.js?v=0.7.11";
import { state, writeForm } from "./frontend-state.js?v=0.7.11";
import { activateHeightLookup } from "./tree-query-state.js?v=0.7.11";

async function showInTree(height) {
  // A navigation gesture, so it takes the epoch with it: a navigator request
  // still in flight has to discard its result rather than overwrite this jump.
  state.navEpoch += 1;
  const epoch = state.navEpoch;
  activateHeightLookup(height);
  // writeForm, not a direct assignment: activateHeightLookup also clears
  // treeTime in query state, and leaving the old Date/Time value in the input
  // would show both lookups populated and block a later Height commit, which
  // refuses when the sibling field is non-empty.
  writeForm();
  // Dynamic import: a static edge here would close the module cycle with
  // view-shell, which imports the views that call this.
  const shell = await import("./view-shell.js?v=0.7.11");
  await shell.activateView("tree");
  // The load is awaited, so a gesture made during it owns the camera now.
  // Centring the superseded height would drag the camera off the user's actual
  // target, and that transform is stored and survives into the winning render.
  if (state.navEpoch !== epoch) return;
  if (state.query.view !== "tree" || state.query.treeHeight !== String(height)) return;
  centerCameraOnHeight(String(height));
}

export { showInTree };
