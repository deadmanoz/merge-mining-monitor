// Pure layout for the Bitcoin header tree. D3 rendering stays in app.js; this
// module only assigns stable x/y coordinates so the geometry is testable without
// a browser DOM.

export const DEFAULT_TREE_LAYOUT = {
  minWidth: 520,
  minHeight: 340,
  fallbackWidth: 780,
  fallbackHeight: 420,
  margin: { left: 80, right: 110, top: 64, bottom: 64 },
  blockPitchX: 152,
  blockPitchY: 132,
};

export function layoutTreeNodes(payload, size = {}, config = {}) {
  const layout = { ...DEFAULT_TREE_LAYOUT, ...config };
  layout.margin = { ...DEFAULT_TREE_LAYOUT.margin, ...(config.margin || {}) };

  const width = Math.max(layout.minWidth, size.width || layout.fallbackWidth);
  const height = Math.max(layout.minHeight, size.height || layout.fallbackHeight);
  const nodes = (payload?.nodes || []).map((node) => ({ ...node }));
  const edges = payload?.edges || [];
  if (!nodes.length) {
    return { nodes, edges, width, height, nodesByHash: new Map() };
  }

  const nodesByHash = new Map(nodes.map((node) => [node.hash, node]));
  // A fork-placed anchor orphan carries a null `btc_height` but a derived
  // `placement_height`, so it ranks at its placement column (beside the
  // same-height canonical) and dangles into a fork slot below the spine, rather
  // than falling into the flat null lane. The edge to its canonical predecessor
  // joins it to the spine component, so it shares that lane's y and the existing
  // `fanOutTreeSlots` stacks it under the same-height canonical.
  const rankHeightOf = (node) => {
    const value = node.height ?? node.placement_height;
    return value === null || value === undefined ? null : Number(value);
  };
  const occupiedHeights = Array.from(
    new Set(nodes.map(rankHeightOf).filter((value) => value !== null)),
  ).sort((a, b) => a - b);
  const heightRank = new Map(occupiedHeights.map((heightValue, index) => [heightValue, index]));
  const componentByHash = assignComponents(nodes, edges);
  const components = Array.from(new Set(nodes.map((node) => componentByHash.get(node.hash) || 0))).sort((a, b) => a - b);
  const laneByComponent = new Map(components.map((component, index) => [component, index]));
  const nullLane = Math.max(components.length, 1);

  const rankCount = Math.max(occupiedHeights.length, 1);
  const laneCount = Math.max(components.length + 1, 2);
  const xStep = Math.max(layout.blockPitchX, (width - layout.margin.left - layout.margin.right) / Math.max(1, rankCount - 1));
  const yStep = Math.max(layout.blockPitchY, (height - layout.margin.top - layout.margin.bottom) / Math.max(1, laneCount));

  // Heighted (and fork-placed) nodes rank by height. Truly height-less nodes (a
  // few direct near/unknown nodes, or the flat-strip fallback) spread
  // left-to-right by payload order into their own lane, rather than piling at a
  // single rank/lane, so such a view lays out as a navigable time strip instead
  // of one stacked column. The backend returns those in (btc_header_time,
  // btc_header_hash) order, so payload order is time order.
  const heightedRanks = occupiedHeights.length;
  let nullIndex = 0;
  nodes.forEach((node) => {
    const rankHeight = rankHeightOf(node);
    if (rankHeight === null) {
      node.rank = heightedRanks + nullIndex;
      node.x = layout.margin.left + heightedRanks * xStep + nullIndex * layout.blockPitchX;
      node.y = layout.margin.top + nullLane * yStep;
      nullIndex += 1;
    } else {
      const rank = heightRank.get(rankHeight) ?? 0;
      const componentLane = laneByComponent.get(componentByHash.get(node.hash) || 0) || 0;
      node.rank = rank;
      node.x = layout.margin.left + rank * xStep;
      node.y = layout.margin.top + componentLane * yStep;
    }
  });

  fanOutTreeSlots(nodes, payload?.branches || [], nodesByHash, yStep);

  return { nodes, edges, width, height, nodesByHash };
}

function fanOutTreeSlots(nodes, branches, nodesByHash, yStep) {
  const branchUnits = staleBranchUnits(branches, nodesByHash);
  const branchRootByMember = new Map();
  branchUnits.forEach((unit, rootHash) => {
    unit.members.forEach((member) => branchRootByMember.set(member.hash, rootHash));
  });

  const slotGroups = new Map();
  nodes.forEach((node) => {
    const rootHash = branchRootByMember.get(node.hash);
    if (rootHash && rootHash !== node.hash) return;

    const unit = branchUnits.get(node.hash) || { root: node, members: [node] };
    const key = `${unit.root.rank}:${Math.round(unit.root.y)}`;
    const group = slotGroups.get(key);
    if (group) group.push(unit);
    else slotGroups.set(key, [unit]);
  });

  const spread = yStep * 0.82;
  for (const group of slotGroups.values()) {
    let slotOffset = 0;
    group.slice().sort(compareTreeSlotUnits).forEach((unit) => {
      const rowY = unit.root.y + slotOffset * spread;
      unit.members.forEach((node) => {
        node.y = rowY;
      });
      // Fork lanes: a forked branch (e.g. two orphans off one parent) places more
      // than one member at the same rank/x; they must lane apart. Lanes are
      // ANCESTRY-aware so an arm of a forked component stays on ONE row: a node's
      // first child (by hash) inherits its lane, and each additional child (a fork
      // sibling) opens a new lane that ITS descendants inherit. So a continuing fork
      // (root -> {a, b}, b -> b2) keeps b2 on b's lane rather than collapsing back to
      // the root row and drawing a backtracking edge. A linear branch uses lane 0
      // only, so this is a no-op and the existing stale-branch layout is unchanged.
      const byHash = new Map(unit.members.map((node) => [node.hash, node]));
      const childrenOf = new Map();
      unit.members.forEach((node) => {
        if (byHash.has(node.prev_hash)) {
          const kids = childrenOf.get(node.prev_hash);
          if (kids) kids.push(node);
          else childrenOf.set(node.prev_hash, [node]);
        }
      });
      const laneOf = new Map();
      let nextLane = 0;
      const assignLane = (node, lane) => {
        laneOf.set(node.hash, lane);
        (childrenOf.get(node.hash) || [])
          .slice()
          .sort((a, b) => String(a.hash).localeCompare(String(b.hash)))
          .forEach((kid, index) => {
            assignLane(kid, index === 0 ? lane : nextLane++);
          });
      };
      unit.members
        .filter((node) => !byHash.has(node.prev_hash))
        .sort((a, b) => String(a.hash).localeCompare(String(b.hash)))
        .forEach((root) => assignLane(root, nextLane++));
      let lanes = 1;
      unit.members.forEach((node) => {
        const lane = laneOf.get(node.hash) || 0;
        lanes = Math.max(lanes, lane + 1);
        node.y = rowY + lane * spread;
      });
      slotOffset += lanes;
    });
  }
}

function staleBranchUnits(branches, nodesByHash) {
  const units = new Map();
  (branches || []).forEach((branch) => {
    const members = (branch.member_hashes || [])
      .map((hash) => nodesByHash.get(hash))
      .filter(Boolean);
    if (!members.length) return;

    units.set(members[0].hash, {
      root: members[0],
      members,
    });
  });
  return units;
}

function assignComponents(nodes, edges) {
  const adjacency = new Map(nodes.map((node) => [node.hash, new Set()]));
  edges.forEach((edge) => {
    if (adjacency.has(edge.from_hash) && adjacency.has(edge.to_hash)) {
      adjacency.get(edge.from_hash).add(edge.to_hash);
      adjacency.get(edge.to_hash).add(edge.from_hash);
    }
  });
  const component = new Map();
  let id = 0;
  for (const node of nodes) {
    if (component.has(node.hash)) continue;
    const stack = [node.hash];
    while (stack.length) {
      const hash = stack.pop();
      if (component.has(hash)) continue;
      component.set(hash, id);
      adjacency.get(hash)?.forEach((next) => {
        if (!component.has(next)) stack.push(next);
      });
    }
    id += 1;
  }
  return component;
}

function compareTreeSlotUnits(a, b) {
  const priority = treeSlotKindPriority(a.root.kind) - treeSlotKindPriority(b.root.kind);
  if (priority !== 0) return priority;
  if (a.members.length !== b.members.length) return b.members.length - a.members.length;
  return String(a.root.hash || "").localeCompare(String(b.root.hash || ""));
}

function treeSlotKindPriority(kind) {
  if (kind === "canonical") return 0;
  if (kind === "stale") return 2;
  return 1;
}
