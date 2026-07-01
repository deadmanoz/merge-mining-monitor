import { applyTreeHighlight } from "./controls.js";
import { $, BLOCK_H, BLOCK_W, chainColor, chainDisplayName, EDGE_KINDS, EDGE_LEGEND, esc, KINDS, ORPHAN_LEGEND, state } from "./frontend-state.js";
import { DEFAULT_TREE_LAYOUT, layoutTreeNodes } from "./tree-layout.js";
import { nodeFillVar, nodeLabel } from "./windowing.js";


// On-canvas legend overlay. Two groups: block kinds carry meaning through the
// node fill color (same var(--kind) the blocks draw with), and edge rows carry
// user-facing categories. The wire values remain raw edge kinds; the legend
// deliberately collapses stale_entry/stale into one readable stale-branch row.
function renderTreeLegend(legend) {
  const body = $("#tree-legend-body");
  if (!body) return;
  const nodeKinds = legend?.kinds || KINDS;
  const edgeItems = edgeLegendItems(legend?.edge_kinds || EDGE_KINDS);
  const nodeItems = blockLegendRows(nodeKinds)
    .map((row) => `<span class="tree-legend-item"><span class="tree-legend-node-swatch ${esc(row.swatchClass)}"></span>${esc(row.label)}</span>`)
    .join("");
  const edgeRows = edgeItems
    .map((item) => `<span class="tree-legend-item">${edgeSwatch(item.swatch)}${esc(item.label)}</span>`)
    .join("");
  body.innerHTML =
    `<div class="tree-legend-group-title">Blocks</div>${nodeItems}` +
    `<div class="tree-legend-group-title">Edges</div>${edgeRows}`;
}

// Block-kind legend rows: the single "unknown" kind expands into the visible
// strict/weak orphan signal; every other visible kind is one row keyed by its
// structural fill-<kind> swatch.
function blockLegendRows(nodeKinds) {
  const rows = [];
  for (const kind of nodeKinds) {
    if (kind === "unknown") {
      rows.push(...ORPHAN_LEGEND);
    } else if (kind === "near") {
      continue;
    } else {
      rows.push({ label: kind, swatchClass: `fill-${kind}` });
    }
  }
  return rows;
}

function edgeLegendItems(kinds) {
  const available = new Set(kinds);
  const handled = new Set();
  const items = EDGE_LEGEND
    .map((item) => {
      const itemKinds = item.kinds.filter((kind) => available.has(kind));
      itemKinds.forEach((kind) => handled.add(kind));
      return itemKinds.length ? { ...item, kinds: itemKinds } : null;
    })
    .filter(Boolean);
  for (const kind of kinds) {
    if (!handled.has(kind)) {
      items.push({ label: kind.replaceAll("_", " "), kinds: [kind], swatch: kind });
    }
  }
  return items;
}

function edgeSwatch(kind) {
  return `<svg class="tree-legend-edge-swatch" viewBox="0 0 22 8" aria-hidden="true"><line class="tree-edge edge-${esc(kind)}" x1="1" y1="4" x2="21" y2="4" /></svg>`;
}

// Restore the persisted collapse state, populate the legend with the static
// fallback (so it has content even if the first tree fetch errors), and wire the
// collapse toggle. The tree payload's own legend refreshes the body on load.
function wireTreeLegend() {
  const legend = $("#tree-legend");
  const toggle = $("#tree-legend-toggle");
  if (!legend || !toggle) return;
  const setCollapsed = (collapsed) => {
    legend.dataset.collapsed = collapsed ? "true" : "false";
    toggle.setAttribute("aria-expanded", collapsed ? "false" : "true");
  };
  setCollapsed(localStorage.getItem("mmm-tree-legend") === "collapsed");
  renderTreeLegend();
  toggle.addEventListener("click", () => {
    const next = legend.dataset.collapsed !== "true";
    setCollapsed(next);
    localStorage.setItem("mmm-tree-legend", next ? "collapsed" : "expanded");
  });
}

function renderTree(payload, svgElement, options = {}) {
  const svg = d3.select(svgElement);
  // Per-SVG state: the laid-out nodes (so selection can draw the chain overlay
  // without a relayout) and the persisted pan/zoom transform (so re-renders
  // never snap the camera). Bound to the element so the fixture harness's
  // multiple SVGs do not share a transform.
  const elState = svgElement.__mmmTree || (svgElement.__mmmTree = { byHash: new Map(), transform: null });
  svg.selectAll("*").remove();
  const width = Math.max(520, svgElement.clientWidth || 780);
  const height = Math.max(340, svgElement.clientHeight || 420);
  svg.attr("viewBox", `0 0 ${width} ${height}`);

  const { nodes, edges, nodesByHash } = layoutTreeNodes(payload, { width, height }, DEFAULT_TREE_LAYOUT);
  if (!nodes.length) {
    svg.append("text")
      .attr("x", width / 2)
      .attr("y", height / 2)
      .attr("text-anchor", "middle")
      .attr("fill", "currentColor")
      .text("No tree nodes");
    elState.byHash = new Map();
    return;
  }

  const zoomLayer = svg.append("g").attr("class", "tree-zoom-layer");
  const edgeLayer = zoomLayer.append("g").attr("class", "tree-edge-layer");
  const nodeLayer = zoomLayer.append("g").attr("class", "tree-node-layer");
  const overlayLayer = zoomLayer.append("g").attr("class", "tree-overlay-layer");

  const visibleEdges = edges.filter((edge) => nodesByHash.has(edge.from_hash) && nodesByHash.has(edge.to_hash));

  edgeLayer.selectAll("path")
    .data(visibleEdges)
    .join("path")
    .attr("class", (edge) => `tree-edge edge-${edge.edge_kind || "hidden"}`)
    .attr("d", (edge) => edgePath(nodesByHash.get(edge.from_hash), nodesByHash.get(edge.to_hash)));

  drawHiddenEdgeLabels(edgeLayer, visibleEdges, nodesByHash);

  const nodeGroups = nodeLayer.selectAll("g")
    .data(nodes, (node) => node.hash)
    .join("g")
    .attr("class", "tree-node")
    .attr("data-selected", (node) => node.hash === options.selectedHash)
    .attr("tabindex", "0")
    .attr("role", "button")
    .attr("aria-label", (node) => `${node.kind} ${node.height ?? "unheighted"} ${node.hash}`)
    .attr("transform", (node) => `translate(${node.x},${node.y})`)
    .on("click", (event, node) => {
      // Stop the gesture reaching the zoom behavior so a select never pans or
      // zooms the camera.
      event.stopPropagation();
      options.onSelect?.(node.hash);
    })
    .on("keydown", (event, node) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        options.onSelect?.(node.hash);
      }
    });

  nodeGroups.each(function drawNode(node) {
    drawTreeBlock(d3.select(this), node);
  });

  // Cache positioned nodes so selection can draw the chain overlay in place, and
  // draw the current selection's overlay now (it survives a filter re-render).
  elState.byHash = new Map(nodes.map((node) => [node.hash, node]));
  drawSelectionOverlay(overlayLayer, options.selectedHash, elState.byHash);
  applyTreeHighlight(svgElement, options.highlightSources, options.highlightKinds, options.highlightClassifications, options.selectedHash);

  // clickDistance(4) matches the background-click movement threshold below. d3-zoom
  // defaults clickDistance to 0, so ANY pointer movement between mousedown and
  // mouseup classifies a node click as a pan and arms a capture-phase click
  // suppressor that swallows the node's own click (no selection, drawer never
  // opens). A 4px tolerance lets a small trackpad micro-drag still register as a
  // click; real pans (>4px) still suppress the trailing click so a pan never selects.
  const zoom = d3.zoom().scaleExtent([0.35, 3]).clickDistance(4).on("zoom", (event) => {
    zoomLayer.attr("transform", event.transform);
    elState.transform = event.transform;
  });
  svg.call(zoom);
  // Cache the zoom behavior so a post-render one-shot center (centerCameraOnNode)
  // can move the camera without a relayout.
  elState.zoom = zoom;
  // Disable double-click-to-zoom: double-clicking a block should select, never
  // move the camera.
  svg.on("dblclick.zoom", null);
  // Clicking empty canvas deselects. Node clicks stopPropagation, so this only
  // sees background clicks; the movement threshold ignores the click that ends a
  // pan-drag so panning never clears the selection.
  let bgPointerDown = null;
  svg.on("pointerdown.bg", (event) => {
    bgPointerDown = [event.clientX, event.clientY];
    // Mark a tree pointer interaction active so the auto-refresh timer defers its
    // re-render until release: a renderTree() between this press and the resulting
    // click would remove the pressed node's <g> (and its per-node click handler),
    // silently dropping the selection. Only the primary button arms it: a
    // right-click's pointerup can be swallowed by the context menu, which would
    // otherwise strand the flag true and block auto-refresh indefinitely.
    if (event.button === 0) state.treePointerActive = true;
  });
  const endTreePointer = () => { state.treePointerActive = false; };
  svg.on("pointerup.bg", endTreePointer);
  svg.on("pointercancel.bg", endTreePointer);
  svg.on("click.bg", (event) => {
    if (!bgPointerDown) return;
    const moved = Math.hypot(event.clientX - bgPointerDown[0], event.clientY - bgPointerDown[1]);
    bgPointerDown = null;
    if (moved <= 4) options.onBackgroundClick?.();
  });
  // Preserve the user's pan/zoom across re-renders (filter changes, data
  // refreshes). Only anchor on the tip when there is no stored transform; the
  // "Live tip" and "Refresh" actions clear it to force a re-anchor.
  if (elState.transform) {
    svg.call(zoom.transform, elState.transform);
  } else {
    anchorTreeAtTip(svg, zoom, nodes, width, height);
  }
}

// Per-chain merge-mining evidence for one Bitcoin header, child-chain prefix
// stripped, used both for the high-level count badge and the click-expand card.
function childChainList(node) {
  return (node.child_chain_evidence || [])
    .map((item) => ({
      chain: String(item.child_chain || item.source || "").replace(/^auxpow:/, ""),
      count: item.event_count || 0,
      min: item.child_height_min,
      max: item.child_height_max,
    }))
    .filter((item) => item.chain);
}

function distinctChainCount(chains) {
  return new Set(chains.map((item) => item.chain)).size;
}

function truncateLabel(text, max) {
  const value = String(text);
  return value.length > max ? `${value.slice(0, max - 1)}…` : value;
}

function formatInt(value) {
  return Number(value).toLocaleString("en-US");
}

function treeNodeTitle(node, chains) {
  const base = `${node.kind} ${node.height ?? "unheighted"} ${node.hash}`;
  if (!chains.length) return base;
  const label = chains.map((item) => `${chainDisplayName(item.chain)}${item.count ? ` x${item.count}` : ""}`).join(", ");
  return `${base} | ${label}`;
}

// One uniform block: kind carried by fill color plus a kind label, the height
// inside, the Bitcoin miner below, and corner count badges for distinct child chains and
// distinct sources (fork.observer-style).
function drawTreeBlock(group, node) {
  const chains = childChainList(node);
  const chainCount = distinctChainCount(chains);

  group.append("title").text(treeNodeTitle(node, chains));

  group.append("rect")
    .attr("class", "tree-block")
    .attr("x", -BLOCK_W / 2)
    .attr("y", -BLOCK_H / 2)
    .attr("width", BLOCK_W)
    .attr("height", BLOCK_H)
    .attr("rx", 8)
    .attr("fill", nodeFillVar(node));

  // Height and miner name both sit inside the block; kind is conveyed by the
  // fill color, so there is no separate kind label. An unresolved miner reads
  // "unknown miner" rather than "Unknown" so it cannot be mistaken for the
  // "unknown" parent kind. A fork-placed orphan (null btc_height) shows its
  // derived placement height (prefixed "~" when approximate); see nodeLabel.
  group.append("text")
    .attr("class", "tree-block-height")
    .attr("x", 0)
    .attr("y", -10)
    .attr("text-anchor", "middle")
    .attr("dominant-baseline", "central")
    .text(nodeLabel(node));

  // Label with the best-available miner: display_miner_pool equals the strict
  // bitcoin_miner_pool for coinbase-resolved blocks and carries the
  // child-inferred pool for RSK-only stale blocks (basis "child_inferred"). The
  // strict bitcoin_miner_pool is the defensive fallback for older payloads.
  const minerPool = node.display_miner_pool ?? node.bitcoin_miner_pool;
  const minerLabel = minerPool?.known && minerPool.name ? minerPool.name : "unknown miner";
  group.append("text")
    .attr("class", "tree-block-pool")
    .attr("x", 0)
    .attr("y", 16)
    .attr("text-anchor", "middle")
    .attr("dominant-baseline", "central")
    .text(truncateLabel(minerLabel, 15));

  if (chainCount) {
    drawCornerBadge(group, BLOCK_W / 2 - 9, -BLOCK_H / 2 - 9, chainCount, "tree-badge-chains");
  }
}

function drawCornerBadge(group, cx, cy, value, klass) {
  const badge = group.append("g")
    .attr("class", `tree-badge ${klass}`)
    .attr("transform", `translate(${cx},${cy})`);
  badge.append("rect")
    .attr("class", "tree-badge-bg")
    .attr("x", -9)
    .attr("y", -9)
    .attr("width", 18)
    .attr("height", 18)
    .attr("rx", 4);
  badge.append("text")
    .attr("class", "tree-badge-text")
    .attr("x", 0)
    .attr("y", 0)
    .attr("text-anchor", "middle")
    .attr("dominant-baseline", "central")
    .text(value);
}

// Draw the per-chain breakdown for the selected block as an overlay anchored to
// the block. It lives in the pan/zoom layer (so it tracks the block) but is
// drawn last and never reflows neighbors or moves the camera.
function drawSelectionOverlay(overlayLayer, selectedHash, byHash) {
  overlayLayer.selectAll("*").remove();
  if (!selectedHash) return;
  const node = byHash.get(selectedHash);
  if (!node) return;
  const chains = childChainList(node);
  if (!chains.length) return;

  const title = `merge-mined chains (${distinctChainCount(chains)})`;
  const rows = chains.map((item) => {
    // Child-chain block height (range when it spans more than one), comma
    // formatted, no unit prefix. The event count is only shown when more than
    // one child block commits to this header (e.g. an RSK uncle plus canonical).
    const range = item.min == null
      ? ""
      : (item.min === item.max ? formatInt(item.min) : `${formatInt(item.min)} to ${formatInt(item.max)}`);
    const meta = item.count > 1 ? `${range ? `${range}  ` : ""}×${item.count}` : range;
    // Each row carries its chain's stable swatch color so the same chain reads
    // the same everywhere a child-chain layer is added later.
    return { name: chainDisplayName(item.chain), meta, color: chainColor(item.chain) };
  });

  const rowH = 16;
  const padX = 11;
  const gap = 22;
  const swatchPad = 14; // room for the per-chain color swatch left of the name
  // Size the card to its content so the chain name and the right-aligned
  // height/count never collide, whatever the value widths are.
  let contentW = title.length * 6.3;
  rows.forEach((row) => {
    contentW = Math.max(contentW, swatchPad + row.name.length * 7 + gap + row.meta.length * 6.3);
  });
  const cardW = Math.min(360, Math.max(150, Math.ceil(contentW) + 2 * padX));
  const cardH = 24 + rows.length * rowH;

  const card = overlayLayer.append("g")
    .attr("class", "tree-chain-card")
    .attr("transform", `translate(${node.x - cardW / 2},${node.y + BLOCK_H / 2 + 36})`);
  card.append("rect")
    .attr("class", "tree-chain-card-bg")
    .attr("x", 0)
    .attr("y", 0)
    .attr("width", cardW)
    .attr("height", cardH)
    .attr("rx", 6);
  card.append("text")
    .attr("class", "tree-chain-card-title")
    .attr("x", padX)
    .attr("y", 15)
    .text(title);
  rows.forEach((row, index) => {
    const g = card.append("g").attr("transform", `translate(${padX},${24 + index * rowH + 12})`);
    g.append("circle")
      .attr("class", "tree-chain-card-swatch")
      .attr("cx", 4)
      .attr("cy", -3)
      .attr("r", 4)
      .attr("fill", row.color);
    g.append("text").attr("class", "tree-chain-card-name").attr("x", swatchPad).attr("y", 0).text(row.name);
    g.append("text")
      .attr("class", "tree-chain-card-meta")
      .attr("x", cardW - 2 * padX)
      .attr("y", 0)
      .attr("text-anchor", "end")
      .text(row.meta);
  });
}

function edgePath(source, target) {
  const midX = (source.x + target.x) / 2;
  return `M${source.x},${source.y} C${midX},${source.y} ${midX},${target.y} ${target.x},${target.y}`;
}

// A `hidden` edge collapses an omitted run of canonical blocks into one dashed
// link. Label each with the count it hides (carried in the payload as
// hidden_count) so the size of the gap is legible without expanding the window.
// The pill sits at the edge midpoint, which for our symmetric cubic curve is
// just the midpoint of the two endpoints.
function drawHiddenEdgeLabels(edgeLayer, edges, nodesByHash) {
  const labeled = edges.filter((edge) => edge.edge_kind === "hidden" && edge.hidden_count > 0);
  edgeLayer.selectAll("g.tree-edge-label")
    .data(labeled)
    .join("g")
    .attr("class", "tree-edge-label")
    .attr("transform", (edge) => {
      const source = nodesByHash.get(edge.from_hash);
      const target = nodesByHash.get(edge.to_hash);
      return `translate(${(source.x + target.x) / 2},${(source.y + target.y) / 2})`;
    })
    .each(function drawLabel(edge) {
      const g = d3.select(this);
      const text = formatInt(edge.hidden_count);
      const width = Math.max(20, text.length * 7 + 12);
      g.append("title").text(`${text} hidden block${edge.hidden_count === 1 ? "" : "s"}`);
      g.append("rect")
        .attr("class", "tree-edge-label-bg")
        .attr("x", -width / 2)
        .attr("y", -8)
        .attr("width", width)
        .attr("height", 16)
        .attr("rx", 8);
      g.append("text")
        .attr("class", "tree-edge-label-text")
        .attr("x", 0)
        .attr("y", 0)
        .attr("text-anchor", "middle")
        .attr("dominant-baseline", "central")
        .text(text);
    });
}

// Anchor the default view on the chain tip at full block size. The tip (rightmost,
// highest block) sits dead-center at the same (width/2, height/2) point
// centerCameraOnNode uses, so Live tip, the initial load, and the
// anchorCameraOnTip fallback all center the tip the way a node click / navigator
// jump centers its target; nothing is newer than the tip, so the right half of the
// canvas is intentionally empty and older blocks extend left to be panned to.
function anchorTreeAtTip(svg, zoom, nodes, width, height) {
  const scale = 1;
  const maxX = Math.max(...nodes.map((node) => node.x));
  const tipCandidates = nodes.filter((node) => node.x === maxX);
  const anchor = tipCandidates.reduce((best, node) => (node.y <= best.y ? node : best));
  const targetX = width / 2;
  const targetY = height / 2;
  const tx = targetX - anchor.x * scale;
  const ty = targetY - anchor.y * scale;
  svg.call(zoom.transform, d3.zoomIdentity.translate(tx, ty).scale(scale));
}

// Re-anchor the camera on the layout tip from the cached layout, no relayout.
// Fallback for centerCameraOnNode when a requested center target is absent from
// the rendered window, so the camera lands on the tip (as a fresh render would)
// instead of stalling on a stale view. Returns false when there is no cached
// layout to anchor on.
function anchorCameraOnTip(svgElement) {
  const elState = svgElement?.__mmmTree;
  if (!elState?.zoom || !elState.byHash || elState.byHash.size === 0) return false;
  const nodes = [...elState.byHash.values()];
  const width = Math.max(520, svgElement.clientWidth || 780);
  const height = Math.max(340, svgElement.clientHeight || 420);
  anchorTreeAtTip(d3.select(svgElement), elState.zoom, nodes, width, height);
  return true;
}


export {
  renderTreeLegend,
  wireTreeLegend,
  renderTree,
  drawSelectionOverlay,
  anchorCameraOnTip,
};
