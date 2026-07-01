import { blockExplorer } from "./explorer-links.js";
import { $, chainDisplayName, CLASSIFICATION_META, esc, formatEpoch, formatScalar, formatSourceList, formatSourceRef, state } from "./frontend-state.js";


// Contextual help for the esoteric AuxPoW and merge-mining concepts in the
// block-detail drawer, sourced from the merge-mining/AuxPoW explainer. Each `(i)`
// button in the drawer carries data-auxpow-info=<topic> and opens #auxpow-dialog
// with this content.
const AUXPOW_HELP = {
  inferred_miner: {
    name: "Inferred miner",
    meta: "Best-available miner when the Bitcoin coinbase miner is unknown",
    body: [
      "This Bitcoin block's own coinbase miner could not be identified, so \"Bitcoin miner\" stays Unknown. That happens when there is no recoverable Bitcoin coinbase to read a pool identity from, most often an RSK-only stale block whose compressed AuxPoW proof discards the parent coinbase under RSKIP-92.",
      "It was still merge-mined: the same proof-of-work that found this Bitcoin block also secured a child-chain block, whose own miner or payout identity we map to a pool and show here. That identity is usually, but not always, the same operator as the Bitcoin coinbase miner, so treat it as a best-available hint, not a coinbase fact; the strict \"Bitcoin miner\" stays Unknown.",
    ],
  },
  commitment: {
    name: "Merge-mining commitment",
    meta: "How this Bitcoin block commits to its auxiliary chains",
    body: [
      "A merge-mined Bitcoin block commits to one or more auxiliary chains through its coinbase, so the same proof-of-work can secure all of them. How the commitment is encoded depends on the chain family (see Format).",
      "Namecoin-family chains (Namecoin, Syscoin, Fractal, Elastos) use a 44-byte marker in the coinbase scriptSig: the 0xfabe6d6d magic, a 32-byte aux_merkle_root, and merkle_size / merkle_nonce; this panel shows those decoded fields. RSK discards the coinbase under RSKIP-92, so its commitment is opaque with no recoverable marker; Hathor uses the RFC 0006 \"Hath\" split-header form instead of fabe6d6d.",
    ],
  },
  aux_merkle_root: {
    name: "aux_merkle_root",
    meta: "Root of the merkle slot tree",
    body: [
      "The root of a fixed-size merkle tree whose leaves are auxiliary block hashes. Each merge-mined chain occupies one slot, and the single root commits to all of them at once, so one Bitcoin block can reward several auxiliary chains.",
      "Shown in display (reversed) byte order, like every other hash here.",
    ],
  },
  merkle_size_nonce: {
    name: "merkle_size & merkle_nonce",
    meta: "Slot-tree size and placement nonce",
    body: [
      "merkle_size is the number of slots in the aux merkle tree (always a power of two). merkle_nonce is a miner-chosen value meant to help avoid slot collisions between chains.",
      "A chain's slot is derived from its chain_id, merkle_nonce, and merkle_size by a fixed LCG; merkle_size = 1 collapses the tree to a single leaf.",
    ],
  },
  slot_index: {
    name: "Slot index",
    meta: "This chain's leaf in the parent's slot tree",
    body: [
      "The position (nChainIndex) this auxiliary chain occupies in the parent block's merkle slot tree. A verifier independently derives the expected slot from chain_id + merkle_nonce + merkle_size and rejects the proof if it disagrees, so a miner cannot silently put two chains at the same leaf.",
    ],
  },
  chain_id: {
    name: "Chain id",
    meta: "The chain's AuxPoW identifier",
    body: [
      "Each Bitcoin-merge-mined chain has a fixed AuxPoW chain id (Namecoin = 1). Combined with the marker's merkle_nonce and merkle_size it determines the chain's slot. It is a reference label; the slot index decoded from the proof determines verification.",
    ],
  },
  targets: {
    name: "parent_target vs aux_target",
    meta: "The two proof-of-work thresholds",
    body: [
      "parent_target is Bitcoin's own proof-of-work threshold (its nBits). aux_target is the auxiliary chain's threshold, set independently and almost always easier.",
      "Clearing parent_target means the embedded header is a valid Bitcoin proof-of-work, but not which Bitcoin chain it is on: classifying it as canonical, stale, or a Core-gated orphan needs Bitcoin-chain evidence (see Kind and Orphan class). A header clearing only aux_target never met Bitcoin's target at all. The stale and orphan parents are what make these records a side-channel into Bitcoin's history.",
    ],
  },
  auxpow_proof: {
    name: "AuxPoW proof",
    meta: "The two merkle proofs that link the chains",
    body: [
      "The AuxPoW record carries two compact merkle proofs, each folding a known start hash up to an expected root: coinbase_branch and blockchain_branch. Together they prove this auxiliary block inherited the parent's proof-of-work.",
      "hash_block is the redundant CAuxPow::hashBlock; the verifier ignores it and it is conventionally all-zero.",
    ],
  },
  coinbase_branch: {
    name: "coinbase_branch",
    meta: "Coinbase txid up to the parent tx merkle root",
    body: [
      "A merkle path from the parent coinbase transaction's txid up to the transaction merkle root inside the parent block header. It proves the parent's proof-of-work was performed over a transaction tree containing the coinbase that carries the AuxPoW marker.",
      "The side mask is all-zero because the coinbase is always leaf 0 of the parent transaction tree.",
    ],
  },
  blockchain_branch: {
    name: "blockchain_branch",
    meta: "Aux block hash up to the aux_merkle_root",
    body: [
      "A merkle path from this auxiliary block's hash up to the aux_merkle_root in the parent coinbase marker. It proves the marker commits to this auxiliary block (alongside any other chains sharing the slot tree).",
      "Its side mask is this chain's slot index, so the number of siblings is log2(merkle_size).",
    ],
  },
};

function auxpowHelpFor(topic) {
  return AUXPOW_HELP[topic] || { name: "AuxPoW", meta: "", body: [] };
}

// A small `(i)` button that opens the AuxPoW help dialog for one topic. Models
// the source-info-button; wired by a document-level delegation like the copy
// buttons, so it survives drawer re-renders.
function auxpowInfoButton(topic) {
  const help = auxpowHelpFor(topic);
  const label = `About ${esc(help.name)}`;
  return `<button class="icon-button auxpow-info-button" type="button" data-auxpow-info="${esc(topic)}" aria-label="${label}" title="${label}">
    <svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" /></svg>
  </button>`;
}

// A detail section whose heading carries an AuxPoW help button.
function detailSectionHelp(title, topic, body) {
  return `<section class="detail-section"><h3>${esc(title)} ${auxpowInfoButton(topic)}</h3>${body}</section>`;
}

function renderDrawer() {
  const container = $("#drawer");
  const error = state.errors.block;
  if (!state.selectedHash) {
    container.innerHTML = `<div class="empty">No block selected</div>`;
    return;
  }
  if (error) {
    container.innerHTML = errorHtml(error, "Block");
    return;
  }
  const payload = state.selectedBlock;
  if (!payload) {
    container.innerHTML = `<div class="loading">Loading block</div>`;
    return;
  }
  container.innerHTML = renderBlockDetailPayload(payload);
}

// Pure block-detail renderer: builds the drawer HTML from a /block/:hash payload
// with no `state` access, so the dev fixture harness can render committed
// fixtures directly. `renderDrawer` is the thin `state`-reading caller.
function renderBlockDetailPayload(payload) {
  const block = payload.block;
  return [
    detailSection("Parent block (Bitcoin)", renderParentBlock(block)),
    payload.commitment ? detailSectionHelp("Merge-mining commitment", "commitment", renderCommitment(payload.commitment)) : "",
    detailSection("Sources & capture", renderSourcesAndCapture(block.source_summary)),
    detailSection("Auxiliary blocks", renderEvents(payload.event_details || [])),
    payload.competition ? detailSection("Competition", renderCompetition(payload.competition)) : "",
    payload.stale_branch ? detailSection("Stale Branch", renderStaleBranch(payload.stale_branch, block.hash)) : "",
  ].join("");
}

function renderParentBlock(block) {
  const rows = [
    ["Hash", explorerCopyValue(block.hash, "bitcoin", { hash: block.hash })],
    ["Height", formatScalar(block.height)],
    ["Kind", kindBadge(block.kind)],
  ];
  // btc_orphan_class is a refinement of kind='unknown'; show it only there
  // (canonical/stale always have a null orphan class). Reuse the navigator's
  // label map; a null class is the pending, never-Core-checked case.
  if (block.kind === "unknown") {
    const meta = CLASSIFICATION_META[block.btc_orphan_class || "pending"];
    rows.push(["Orphan class", esc(meta ? meta.name : (block.btc_orphan_class || "Pending"))]);
  }
  if (block.coinbase_tag) {
    rows.push(["Coinbase tag", esc(block.coinbase_tag)]);
  }
  rows.push(["Bitcoin miner", poolName(block.bitcoin_miner_pool)]);
  // For an RSK-only stale block the Bitcoin coinbase miner is unknown; show
  // the chain-agnostic child-inferred miner without overstating it as coinbase.
  // The strict row above stays Unknown; the (i) button explains the situation and
  // per-event "Child miner" rows disclose the specific child-chain provenance.
  if (block.display_miner_basis === "child_inferred") {
    rows.push(["Inferred miner", `${poolName(block.display_miner_pool)} ${auxpowInfoButton("inferred_miner")}`]);
  }
  rows.push(["Previous", explorerCopyValue(block.header?.prev_hash, "bitcoin", {
    hash: block.header?.prev_hash,
  })]);
  rows.push(["Time", formatEpoch(block.header?.time)]);
  return kvRows(rows) + `<details class="collapse"><summary>Raw header</summary>${kvRows([
    ["Merkle Root", copyValue(block.header?.merkle_root)],
    ["parent_target (nBits)", formatScalar(block.header?.bits)],
    ["Nonce", formatScalar(block.header?.nonce)],
  ])}</details>`;
}

// The AuxPoW marker shared by every child chain committed to this Bitcoin
// parent: the decoded aux_merkle_root/merkle_size/merkle_nonce for Namecoin-family
// parents, or a format-only entry with a null marker for RSK (rsk-opaque) and
// Hathor (hathor-rfc0006). The raw coinbase tag is rendered in the parent block
// section from the server-projected block field.
function renderCommitment(commitment) {
  const rows = [["Format", formatScalar(commitment.format)]];
  if (commitment.parent_coinbase_txid) {
    rows.push(["Parent coinbase txid", copyValue(commitment.parent_coinbase_txid)]);
  }
  const marker = commitment.marker;
  if (marker) {
    rows.push(["aux_merkle_root", `${copyValue(marker.aux_merkle_root)} ${auxpowInfoButton("aux_merkle_root")}`]);
    rows.push(["merkle_size", `${formatScalar(marker.merkle_size)} ${auxpowInfoButton("merkle_size_nonce")}`]);
    rows.push(["merkle_nonce", formatScalar(marker.merkle_nonce)]);
  } else {
    rows.push(["Marker", `<span class="false-value">none decoded</span>`]);
  }
  return kvRows(rows);
}

function detailSection(title, body) {
  return `<section class="detail-section"><h3>${esc(title)}</h3>${body}</section>`;
}

function kvRows(rows) {
  return `<dl class="kv">${rows.map(([key, value]) => `<dt>${esc(key)}</dt><dd>${value}</dd>`).join("")}</dl>`;
}

// The shared copy button + its (formerly duplicated) icon SVG. The full value
// rides on data-copy; boot's delegated click handler copies it to the clipboard.
function copyButton(value) {
  const text = esc(value);
  return `<button type="button" class="copy-button" data-copy="${text}" aria-label="Copy value" title="Copy value">
    <svg class="copy-button-icon copy-button-icon-copy" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect width="14" height="14" x="8" y="8" rx="2" ry="2"></rect>
      <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"></path>
    </svg>
    <svg class="copy-button-icon copy-button-icon-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M20 6 9 17l-5-5"></path>
    </svg>
  </button>`;
}

function copyValue(value) {
  if (value === null || value === undefined) return formatScalar(value);
  return `<code>${esc(value)}</code> ${copyButton(value)}`;
}

// A copy button with no inline value, for large blobs we deliberately do not
// dump into the page (avoids a wall of hex).
function copyOnly(value) {
  if (value === null || value === undefined) return formatScalar(value);
  return copyButton(value);
}

function explorerCopyValue(value, chain, block = {}) {
  if (value === null || value === undefined) return formatScalar(value);
  const explorer = blockExplorer(chain, block);
  if (!explorer) return copyValue(value);
  return `${copyValue(value)} ${explorerLink(explorer, chain)}`;
}

function explorerLink(explorer, chain) {
  const safeChain = esc(chain);
  const safeName = esc(explorer.name);
  const label = `Open ${safeChain} block in ${safeName}`;
  return `<a class="explorer-link" href="${esc(explorer.url)}" target="_blank" rel="noopener noreferrer" title="${label}" aria-label="${label}">explorer</a>`;
}

function kindBadge(kind) {
  return `<span class="state-pill kind-${esc(kind)}">${esc(kind)}</span>`;
}

function poolName(pool) {
  if (!pool) return formatScalar(null);
  const cls = pool.known ? "true-value" : "false-value";
  return `<span class="${cls}">${esc(pool.name || "Unknown")}</span>`;
}

// Provenance section: which monitor sources captured this Bitcoin parent.
function renderSourcesAndCapture(summary = {}) {
  const rows = [
    ["Sources", formatSourceList(summary.sources || [])],
    ["Distinct", formatScalar(summary.distinct_sources)],
    ["AuxPoW Chains", formatScalar(summary.auxpow_chain_count)],
  ];
  return kvRows(rows);
}

// Each event collapses to a one-line summary (chain, child height, and the
// child miner when child-side attribution resolves); expanding reveals the rest.
// Dropped fields that were redundant with the Header (Parent Kind, Parent Hash,
// Parent Bitcoin miner) or internal (the DB ID). Child miner only appears when
// resolved.
function renderEvents(events) {
  if (!events.length) return `<div class="empty">No auxiliary blocks</div>`;
  return events.map((event) => {
    const knownChildPool = event.child_miner_pool?.known ? event.child_miner_pool : null;
    const poolSuffix = knownChildPool ? ` · ${esc(knownChildPool.name)}` : "";
    const slotSuffix = event.slot_index != null ? ` · slot ${esc(event.slot_index)}` : "";
    const summary = `${esc(chainDisplayName(event.child_chain))} · ${esc(event.child_height ?? "unheighted")}${slotSuffix}${poolSuffix}`;
    const rows = [
      ["Source", formatSourceRef(event.source)],
      ["Child Hash", explorerCopyValue(event.child_block_hash, event.child_chain, {
        hash: event.child_block_hash,
        height: event.child_height,
      })],
      // The real auxiliary block time, not a monitor capture timestamp.
      ["Child Time", formatEpoch(event.child_block_time)],
      ["PoW (parent_target / aux_target)", `${formatScalar(event.pow_validates_btc_target)} / ${formatScalar(event.pow_validates_child_target)} ${auxpowInfoButton("targets")}`],
    ];
    if (event.slot_index != null) rows.push(["Slot index", `${formatScalar(event.slot_index)} ${auxpowInfoButton("slot_index")}`]);
    if (event.chain_id != null) rows.push(["Chain id", `${formatScalar(event.chain_id)} ${auxpowInfoButton("chain_id")}`]);
    if (knownChildPool) rows.push(["Child miner", poolName(knownChildPool)]);
    const rsk = event.rsk ? renderRsk(event.rsk) : "";
    // The decoded AuxPoW proof (two merkle branches), or a compact fallback when
    // the stored blob is present but did not decode (corrupt / parent-mismatched),
    // so the bytes never silently vanish from the UI.
    const auxProof = event.aux_proof
      ? renderAuxProof(event.aux_proof)
      : renderUndecodedProof(event.aux_merkle_proof_hex);
    return `<details class="event-block"><summary>${summary}</summary>${kvRows(rows)}${rsk}${auxProof}</details>`;
  }).join("");
}

// The decoded CAuxPow merkle proofs for one auxiliary block: the redundant
// hash_block (CAuxPow::hashBlock, conventionally zero) plus the coinbase_branch
// (coinbase txid -> parent tx merkle root) and blockchain_branch (aux block hash
// -> aux_merkle_root). Each sibling is an individual copyable hash, not a wall of
// hex. Absent for RSK / Hathor and for rows whose stored blob does not decode.
function renderAuxProof(proof) {
  if (!proof) return "";
  const branch = (label, topic, b) => {
    const count = b.siblings.length;
    const head = `<div class="event-subhead">${esc(label)} · index ${esc(b.index)} · ${count} sibling${count === 1 ? "" : "s"} ${auxpowInfoButton(topic)}</div>`;
    const body = count
      ? kvRows(b.siblings.map((hash, i) => [`sibling ${i}`, copyValue(hash)]))
      : `<div class="empty">no siblings (single-leaf tree)</div>`;
    return head + body;
  };
  const inner =
    kvRows([["hash_block (redundant · usually zero)", copyValue(proof.hash_block)]]) +
    branch("coinbase_branch", "coinbase_branch", proof.coinbase_branch) +
    branch("blockchain_branch", "blockchain_branch", proof.blockchain_branch);
  return `<details class="collapse"><summary>AuxPoW proof ${auxpowInfoButton("auxpow_proof")}</summary>${inner}</details>`;
}

// Fallback when the stored CAuxPow blob did not decode: keep it reachable as a
// byte count plus a copy button, never an inline hex wall.
function renderUndecodedProof(hex) {
  if (!hex) return "";
  const bytes = Math.floor(hex.length / 2);
  return `<details class="collapse"><summary>Proof bytes (undecoded · ${esc(bytes)} bytes)</summary>${kvRows([
    ["aux_merkle_proof", copyOnly(hex)],
  ])}</details>`;
}

// RSK-specific extras shown inside an expanded RSK event. The RSK block hash and
// height are dropped here because they are already the event's Child Hash and
// summary height; uncle position only shows for uncles, and the null-prone
// miner identity / opaque proof rows only appear when present.
function renderRsk(rsk) {
  const rows = [["Uncle", formatScalar(rsk.is_uncle)]];
  if (rsk.is_uncle) {
    rows.push(["Uncle Index", formatScalar(rsk.uncle_index)]);
    rows.push(["Referencing Height", formatScalar(rsk.uncle_referencing_height)]);
  }
  rows.push(["Miner", copyValue(rsk.miner_address)]);
  if (rsk.pool_identity) {
    rows.push(["Pool Identity", formatScalar(`${rsk.pool_identity.namespace}:${rsk.pool_identity.identifier}`)]);
  }
  rows.push(["Proof Format", formatScalar(rsk.proof_format)]);
  if (rsk.merkle_proof_hex) rows.push(["Opaque Proof", copyValue(rsk.merkle_proof_hex)]);
  return `<div class="event-subhead">RSK</div>${kvRows(rows)}`;
}

function renderCompetition(competition) {
  return kvRows([
    ["BTC Height", formatScalar(competition.btc_height)],
    ["Stale", explorerCopyValue(competition.stale_hash, "bitcoin", {
      hash: competition.stale_hash,
    })],
    ["Winning Block", explorerCopyValue(competition.canonical_hash, "bitcoin", {
      hash: competition.canonical_hash,
    })],
    ["Stale Bitcoin miner", poolName(competition.stale_bitcoin_miner_pool)],
    ["Winning Bitcoin miner", poolName(competition.canonical_bitcoin_miner_pool)],
    ["Header Time Delta", formatHeaderTimeDelta(competition.header_time_delta_s)],
    ["Propagation Delta", formatScalar(competition.propagation_delta_s)],
  ]);
}

function formatHeaderTimeDelta(value) {
  if (value === null || value === undefined) return formatScalar(value);
  const seconds = Number(value);
  if (!Number.isFinite(seconds)) return formatScalar(value);
  if (seconds === 0) return "winning and stale block header timestamps are equal to the second";
  const direction = seconds > 0 ? "after" : "before";
  return `winning block header timestamp is ${Math.abs(seconds)}s ${direction} the stale block header timestamp`;
}

function renderStaleBranch(branch, selectedHash = null) {
  const memberCount = branch.member_hashes?.length ?? 0;
  const depth = Number.isFinite(Number(branch.depth)) ? Number(branch.depth) : memberCount;
  const rows = [
    ["Depth", formatScalar(formatBlockCount(depth))],
    ["Position", formatScalar(staleBranchPositionLabel(branch.position))],
  ];
  const heightSpan = staleBranchHeightSpan(branch);
  if (heightSpan) rows.push(["Height Span", formatScalar(heightSpan)]);
  if (depth > 1) {
    rows.push(["Root", staleBranchHashValue(branch.root_hash, selectedHash)]);
    rows.push(["Tip", staleBranchHashValue(branch.tip_hash, selectedHash)]);
    if (branch.parent_stale_hash && branch.parent_stale_hash !== branch.root_hash) {
      rows.push(["Previous Stale", staleBranchHashValue(branch.parent_stale_hash, selectedHash)]);
    }
    const childHashes = (branch.child_stale_hashes || []).filter((hash) => hash !== branch.tip_hash);
    if (childHashes.length === 1) {
      rows.push(["Next Stale", staleBranchHashValue(childHashes[0], selectedHash)]);
    } else if (childHashes.length > 1) {
      rows.push(["Next Stales", staleBranchHashList(childHashes, selectedHash)]);
    }
  }
  return kvRows(rows);
}

function formatBlockCount(count) {
  if (!count) return "unknown";
  return `${count} block${count === 1 ? "" : "s"}`;
}

function staleBranchPositionLabel(position) {
  if (position === "root_and_tip") return "one-block branch";
  if (position === "root") return "branch root";
  if (position === "interior") return "interior block";
  if (position === "tip") return "branch tip";
  return position || null;
}

function staleBranchHeightSpan(branch) {
  if (branch.btc_height_min == null || branch.btc_height_max == null) return null;
  if (branch.btc_height_min === branch.btc_height_max) return null;
  return `${branch.btc_height_min} - ${branch.btc_height_max}`;
}

function staleBranchHashValue(hash, selectedHash = null) {
  if (!hash) return formatScalar(null);
  if (hash === selectedHash) return formatScalar("selected block");
  return explorerCopyValue(hash, "bitcoin", { hash });
}

function staleBranchHashList(hashes, selectedHash = null) {
  if (!hashes?.length) return formatScalar([]);
  return hashes.map((hash) => staleBranchHashValue(hash, selectedHash)).join("<br>");
}

function errorHtml(error, label) {
  return `<div class="empty"><strong>${esc(label)} ${esc(error.code || "error")}</strong><span>${esc(error.message || "Request failed")}</span></div>`;
}

function errorSummary(error, label) {
  const action = error.details?.action ? ` (${error.details.action})` : "";
  return `${label} ${error.code || "error"}: ${error.message || "Request failed"}${action}`;
}


export {
  auxpowHelpFor,
  renderDrawer,
  kvRows,
  errorSummary,
};
