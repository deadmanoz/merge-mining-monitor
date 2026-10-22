import { applyTreeHighlight } from "./controls.js";
import { $, BLOCK_H, BLOCK_W, chainColor, chainDisplayName, EDGE_KINDS, EDGE_LEGEND, esc, KINDS, ORPHAN_LEGEND, protocolLabel, protocolStatusLabel, state } from "./frontend-state.js";
import { DEFAULT_TREE_LAYOUT, layoutTreeNodes } from "./tree-layout.js";

function nodeLabel(node) {
  if (node?.height != null) return String(node.height);
  if (node?.placement_height != null) {
    return node.placement_approx ? `~${node.placement_height}` : String(node.placement_height);
  }
  const hash = typeof node?.hash === "string" ? node.hash : "";
  return hash ? hash.slice(0, 12) : "?";
}

const ORPHAN_FILL_VAR = {
  strict_btc_orphan: "var(--orphan-strict)",
  weak_btc_orphan: "var(--orphan-weak)",
  excluded: "var(--orphan-excluded)",
};

function nodeFillVar(node) {
  const kind = node?.kind || "unknown";
  return kind === "unknown"
    ? ORPHAN_FILL_VAR[node?.btc_orphan_class] || "var(--orphan-pending)"
    : `var(--${kind})`;
}

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
  const badgeRows = [["chains", "merge-mined chains"], ["protocols", "protocol commitments"]]
    .map(([klass, label]) => `<span class="tree-legend-item">${badgeSwatch(klass)}${esc(label)}</span>`)
    .join("");
  body.innerHTML =
    `<div class="tree-legend-group-title">Blocks</div>${nodeItems}` +
    `<div class="tree-legend-group-title">Badges</div>${badgeRows}` +
    `<div class="tree-legend-group-title">Edges</div>${edgeRows}`;
}

function blockLegendRows(nodeKinds) {
  const rows = [];
  for (const kind of nodeKinds) {
    if (kind === "unknown") {
      rows.push(...ORPHAN_LEGEND);
    } else {
      rows.push({ label: kind.replaceAll("_", " "), swatchClass: `fill-${kind}` });
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

function badgeSwatch(klass) {
  return `<svg class="tree-legend-badge-swatch" viewBox="-9 -9 18 18" aria-hidden="true"><g class="tree-badge tree-badge-${klass}"><rect class="tree-badge-bg" x="-9" y="-9" width="18" height="18" rx="4" /><text class="tree-badge-text" text-anchor="middle" dominant-baseline="central">n</text></g></svg>`;
}

function edgeSwatch(kind) {
  return `<svg class="tree-legend-edge-swatch" viewBox="0 0 22 8" aria-hidden="true"><line class="tree-edge edge-${esc(kind)}" x1="1" y1="4" x2="21" y2="4" /></svg>`;
}

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

  elState.byHash = new Map(nodes.map((node) => [node.hash, node]));
  drawSelectionOverlay(overlayLayer, options.selectedHash, elState.byHash);
  applyTreeHighlight(svgElement, options.highlightSources, options.highlightKinds, options.highlightClassifications, options.selectedHash);

  // clickDistance(4) lets a small trackpad micro-drag still register as a node click.
  const zoom = d3.zoom().scaleExtent([0.35, 3]).clickDistance(4).on("zoom", (event) => {
    zoomLayer.attr("transform", event.transform);
    elState.transform = event.transform;
  });
  svg.call(zoom);
  elState.zoom = zoom;
  svg.on("dblclick.zoom", null);
  let bgPointerDown = null;
  svg.on("pointerdown.bg", (event) => {
    bgPointerDown = [event.clientX, event.clientY];
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
  if (elState.transform) {
    svg.call(zoom.transform, elState.transform);
  } else {
    anchorTreeAtTip(svg, zoom, nodes, width, height);
  }
}

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
  const parts = [`${node.kind} ${node.height ?? "unheighted"} ${node.hash}`];
  if (chains.length) parts.push(chains.map((item) => `${chainDisplayName(item.chain)}${item.count ? ` x${item.count}` : ""}`).join(", "));
  const protocols = node.protocol_commitments || [];
  if (protocols.length) parts.push(`protocol commitments: ${protocols.map((entry) => protocolLabel(entry.protocol)).join(", ")}`);
  return parts.join(" | ");
}

// A declaration's associated pool, else its declared account, else why it names none.
function protocolCardMeta(entry) {
  if (entry.decode_status !== "valid") return protocolStatusLabel(entry.decode_status);
  if (entry.declared_account_pool?.known) return entry.declared_account_pool.name;
  const account = entry.declared_account || "";
  return entry.protocol === "core" ? `${account.slice(0, 6)}…${account.slice(-4)}` : account;
}

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

  group.append("text")
    .attr("class", "tree-block-height")
    .attr("x", 0)
    .attr("y", -10)
    .attr("text-anchor", "middle")
    .attr("dominant-baseline", "central")
    .text(nodeLabel(node));

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
  const protocolCount = node.protocol_commitments?.length;
  if (protocolCount) {
    drawCornerBadge(group, -BLOCK_W / 2 + 9, -BLOCK_H / 2 - 9, protocolCount, "tree-badge-protocols");
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

function drawSelectionOverlay(overlayLayer, selectedHash, byHash) {
  overlayLayer.selectAll("*").remove();
  if (!selectedHash) return;
  const node = byHash.get(selectedHash);
  if (!node) return;
  const chains = childChainList(node);
  const protocols = node.protocol_commitments || [];
  if (!chains.length && !protocols.length) return;

  const sections = [];
  if (chains.length) {
    sections.push({
      title: `merge-mined chains (${distinctChainCount(chains)})`,
      rows: chains.map((item) => {
        const range = item.min == null
          ? ""
          : (item.min === item.max ? formatInt(item.min) : `${formatInt(item.min)} to ${formatInt(item.max)}`);
        const meta = item.count > 1 ? `${range ? `${range}  ` : ""}×${item.count}` : range;
        return { name: chainDisplayName(item.chain), meta, color: chainColor(item.chain) };
      }),
    });
  }
  if (protocols.length) {
    sections.push({
      title: `protocol commitments (${protocols.length})`,
      rows: protocols.map((entry) => ({
        name: protocolLabel(entry.protocol),
        meta: protocolCardMeta(entry),
        protocol: entry.protocol,
      })),
      note: "associated pool, not the block's miner",
    });
  }

  const rowH = 16;
  const padX = 11;
  const gap = 22;
  const swatchPad = 14;
  let contentW = 0;
  sections.forEach((section) => {
    contentW = Math.max(contentW, section.title.length * 6.3, (section.note || "").length * 6);
    section.rows.forEach((row) => {
      contentW = Math.max(contentW, swatchPad + row.name.length * 7 + gap + row.meta.length * 6.3);
    });
  });
  const cardW = Math.min(360, Math.max(150, Math.ceil(contentW) + 2 * padX));
  const sectionGap = 12;
  const noteH = 16;

  const card = overlayLayer.append("g")
    .attr("class", "tree-chain-card")
    .attr("transform", `translate(${node.x - cardW / 2},${node.y + BLOCK_H / 2 + 36})`);
  const background = card.append("rect")
    .attr("class", "tree-chain-card-bg")
    .attr("x", 0)
    .attr("y", 0)
    .attr("width", cardW)
    .attr("rx", 6);
  let top = 0;
  sections.forEach((section, sectionIndex) => {
    if (sectionIndex) {
      card.append("line")
        .attr("class", "tree-chain-card-divider")
        .attr("x1", padX)
        .attr("x2", cardW - padX)
        .attr("y1", top + sectionGap / 2)
        .attr("y2", top + sectionGap / 2);
      top += sectionGap;
    }
    card.append("text")
      .attr("class", "tree-chain-card-title")
      .attr("x", padX)
      .attr("y", top + 15)
      .text(section.title);
    section.rows.forEach((row, index) => {
      const g = card.append("g").attr("transform", `translate(${padX},${top + 24 + index * rowH + 12})`);
      if (row.protocol) {
        g.append("rect")
          .attr("class", `tree-chain-card-swatch tree-protocol-swatch-${row.protocol}`)
          .attr("x", 0)
          .attr("y", -7)
          .attr("width", 8)
          .attr("height", 8)
          .attr("rx", 2);
      } else {
        g.append("circle")
          .attr("class", "tree-chain-card-swatch")
          .attr("cx", 4)
          .attr("cy", -3)
          .attr("r", 4)
          .attr("fill", row.color);
      }
      g.append("text").attr("class", "tree-chain-card-name").attr("x", swatchPad).attr("y", 0).text(row.name);
      g.append("text")
        .attr("class", "tree-chain-card-meta")
        .attr("x", cardW - 2 * padX)
        .attr("y", 0)
        .attr("text-anchor", "end")
        .text(row.meta);
    });
    top += 24 + section.rows.length * rowH;
    if (section.note) {
      card.append("text")
        .attr("class", "tree-chain-card-note")
        .attr("x", padX)
        .attr("y", top + 10)
        .text(section.note);
      top += noteH;
    }
  });
  background.attr("height", top);
}

function edgePath(source, target) {
  const midX = (source.x + target.x) / 2;
  return `M${source.x},${source.y} C${midX},${source.y} ${midX},${target.y} ${target.x},${target.y}`;
}

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
