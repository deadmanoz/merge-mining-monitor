import { blockExplorer } from "./explorer-links.js?v=0.9.3";
import { fmtDelta } from "./delta-scales.js?v=0.9.3";
import { $, chainDisplayName, CLASSIFICATION_META, esc, formatEpoch, formatScalar, formatSourceList, formatSourceRef, state } from "./frontend-state.js?v=0.9.3";


// Contextual help topics for AuxPoW fields in the block-detail drawer.
const AUXPOW_HELP = {
  inferred_miner: {
    name: "Inferred miner",
    meta: "Best-available miner when the Bitcoin coinbase miner is unknown",
    body: [
      "This Bitcoin block's own coinbase miner could not be identified, most often on an RSK-only stale block whose RSKIP-92 proof omits the parent coinbase.",
      "Because the same proof of work also secured a child-chain block, the monitor maps that child block's miner or payout identity to a pool as a best-available hint while keeping \"Bitcoin miner\" as Unknown.",
    ],
  },
  commitment: {
    name: "Merge-mining commitment",
    meta: "How this Bitcoin block commits to its auxiliary chains",
    body: [
      "A merge-mined Bitcoin block commits to one or more auxiliary chains through its coinbase, so the same proof-of-work can secure all of them. How the commitment is encoded depends on the chain family (see Format).",
      "Namecoin-family chains (Namecoin, Syscoin, Fractal, Elastos, Terracoin) use a 44-byte marker in the coinbase scriptSig: the 0xfabe6d6d magic, a 32-byte aux_merkle_root, and merkle_size / merkle_nonce; this panel shows those decoded fields. RSK discards the coinbase under RSKIP-92, so its commitment is opaque with no recoverable marker; Hathor uses the RFC 0006 \"Hath\" split-header form instead of fabe6d6d.",
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
  child_time: {
    name: "Child Time",
    meta: "The auxiliary block's own claimed timestamp, and its offset from Bitcoin",
    body: [
      "The auxiliary block's timestamp (header nTime, RSK block timestamp, or Hathor tx timestamp): the monitor observes no build and records only the claim. Because the child template is sealed into the Bitcoin coinbase before hashing and reused across jobs, it usually trails the parent header time.",
      "A stored Child Header is narrower than whether the Bitcoin block committed to it, and separate observations may not cross-check the two. A positive offset marks an ordering disagreement, not a broken clock.",
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

// Consensus rules for catalogued error blocks, keyed by `rejection_reason`.
const CONSENSUS_RULE_HELP = {
  consensus_invalid_parent: {
    name: "Invalid predecessor",
    meta: "Extends a Bitcoin consensus-invalid branch",
    body: [
      "This block extends a predecessor that the reviewed Research catalogue establishes as consensus-invalid, so its authenticated predecessor path cannot form a valid Bitcoin branch despite valid proof of work.",
    ],
  },
  "bad-blk-sigops": {
    name: "Too many sigops (bad-blk-sigops)",
    meta: "Signature-operation cost exceeds Bitcoin's 80,000 limit",
    body: [
      "This block's total signature-operation cost exceeds Bitcoin's 80,000 consensus cap, as established from complete block and previous-output evidence in the Research catalogue.",
    ],
  },
  "block-script-verify-flag-failed": {
    name: "Script verification failed (block-script-verify-flag-failed)",
    meta: "A transaction fails Bitcoin consensus script verification",
    body: [
      "Transactions in this block spend previous outputs with invalid P2SH redeem scripts, established from complete transaction and previous-output evidence.",
    ],
  },
  "bad-cb-amount": {
    name: "Coinbase overpayment (bad-cb-amount)",
    meta: "Coinbase outputs exceed the permitted subsidy plus transaction fees",
    body: [
      "A Bitcoin block may pay its miner no more than the block subsidy plus its transaction fees; reviewed full-block evidence shows this coinbase overpays.",
    ],
  },
  "bad-txns-inputs-missingorspent": {
    name: "Missing or spent input (bad-txns-inputs-missingorspent)",
    meta: "A transaction spends an unavailable previous output",
    body: [
      "Reviewed full-block evidence shows a transaction spending a missing or already-spent input against the prior chain state.",
    ],
  },
  bip34_v2_coinbase_height_mismatch: {
    name: "BIP34 coinbase height mismatch (version 2+)",
    meta: "Coinbase scriptSig does not begin with this block's height",
    body: [
      "During BIP34's v2 opt-in stage (from height 224,413), a version 2+ block had to begin its coinbase scriptSig with its serialized height; this block carries a different height.",
    ],
  },
  bip34_coinbase_height_mismatch: {
    name: "BIP34 coinbase height mismatch",
    meta: "Coinbase height prefix mandatory and wrong here",
    body: [
      "From height 227,931 every valid block must begin its coinbase scriptSig with its own serialized height; this block carries a different height.",
    ],
  },
  bip34_v2_coinbase_height_missing: {
    name: "BIP34 coinbase height missing (version 2+)",
    meta: "Version 2+ block with no coinbase height prefix at all",
    body: [
      "A version 2+ block from BIP34's first enforcement stage whose coinbase scriptSig omits the required serialized height prefix.",
    ],
  },
  bip34_coinbase_height_missing: {
    name: "BIP34 coinbase height missing",
    meta: "No coinbase height prefix where one is mandatory",
    body: [
      "From height 227,931 every valid block must begin its coinbase scriptSig with its serialized height; this block omits the prefix entirely.",
    ],
  },
  bip34_block_version_below_2: {
    name: "Block version below 2 (BIP34)",
    meta: "Version 1 block after BIP34 made version 2 the minimum",
    body: [
      "BIP34 required block version 2 or higher from height 227,931; this header declares version 1.",
    ],
  },
  bip66_block_version_below_3: {
    name: "Block version below 3 (BIP66)",
    meta: "Version predates BIP66's strict DER minimum",
    body: [
      "BIP66 required block version 3 or higher from height 363,725; this header declares a lower version.",
    ],
  },
  bip65_block_version_below_4: {
    name: "Block version below 4 (BIP65)",
    meta: "Version predates BIP65's CHECKLOCKTIMEVERIFY minimum",
    body: [
      "BIP65 required block version 4 or higher from height 388,381; this header declares a lower version.",
    ],
  },
  coinbase_scriptsig_length_above_100: {
    name: "Coinbase scriptSig too long",
    meta: "Serialized length outside the 2-100 byte bound",
    body: [
      "A coinbase scriptSig must serialize to 2–100 bytes; this block's exceeds 100 bytes.",
    ],
  },
  coinbase_scriptsig_length_below_2: {
    name: "Coinbase scriptSig too short",
    meta: "Serialized length below the 2-byte minimum",
    body: [
      "A coinbase scriptSig must serialize to 2–100 bytes; this block's is shorter than 2 bytes.",
    ],
  },
  median_time_past_violation: {
    name: "Block time not after median-time-past",
    meta: "Timestamp violates the median-time-past rule",
    body: [
      "A block's timestamp must be strictly greater than its parent's median-time-past (the median of the preceding 11 blocks).",
    ],
  },
  time_below_mtp: {
    name: "Block time at or below median-time-past",
    meta: "Timestamp at or under the canonical parent's median-time-past",
    body: [
      "This block's nTime is at or below its canonical parent's median-time-past.",
    ],
  },
  nbits_retarget_not_applied: {
    name: "Retarget not applied",
    meta: "Carries the previous epoch's nBits at a retarget boundary",
    body: [
      "At a 2016-block difficulty retarget boundary, this header carries the previous epoch's nBits instead of the retargeted value while still meeting Bitcoin's target.",
    ],
  },
};

function consensusRuleHelpFor(token) {
  if (!token || !Object.prototype.hasOwnProperty.call(CONSENSUS_RULE_HELP, token)) return null;
  return CONSENSUS_RULE_HELP[token];
}

function auxpowInfoButton(topic) {
  const help = auxpowHelpFor(topic);
  const label = `About ${esc(help.name)}`;
  return `<button class="icon-button auxpow-info-button" type="button" data-auxpow-info="${esc(topic)}" aria-label="${label}" title="${label}">
    <svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" /></svg>
  </button>`;
}

function detailSectionHelp(title, topic, body) {
  return `<section class="detail-section"><h3>${esc(title)} ${auxpowInfoButton(topic)}</h3>${body}</section>`;
}

function consensusRuleInfoButton(token, help) {
  const label = `About ${esc(help.name)}`;
  return `<button class="icon-button auxpow-info-button" type="button" data-consensus-rule-info="${esc(token)}" aria-label="${label}" title="${label}">
    <svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" /></svg>
  </button>`;
}

function consensusRejectionValue(token) {
  if (!token) return esc("catalogued consensus violation");
  const help = consensusRuleHelpFor(token);
  if (!help) return esc(token);
  return `${esc(help.name)} ${consensusRuleInfoButton(token, help)}`;
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

function renderBlockDetailPayload(payload) {
  const block = payload.block;
  return [
    detailSection("Parent block (Bitcoin)", renderParentBlock(block)),
    payload.commitment ? detailSectionHelp("Merge-mining commitment", "commitment", renderCommitment(payload.commitment)) : "",
    detailSection("Sources & capture", renderSourcesAndCapture(block.source_summary)),
    detailSection("Auxiliary blocks", renderEvents(payload.event_details || [], block.header?.time)),
    payload.competition ? detailSection("Competition", renderCompetition(payload.competition)) : "",
    !payload.competition && block.kind === "error_block"
      ? detailSection("Competition", `<div class="empty">This block never raced. It carries full Bitcoin proof of work but breaks a consensus rule, so it was never a valid contender for its height and has no canonical competitor.</div>`)
      : "",
    payload.stale_branch ? detailSection("Stale Branch", renderStaleBranch(payload.stale_branch, block.hash)) : "",
  ].join("");
}

function renderParentBlock(block) {
  const rows = [
    ["Hash", explorerCopyValue(block.hash, "bitcoin", { hash: block.hash })],
    ["Height", formatScalar(block.height)],
    ["Kind", kindBadge(block.kind)],
  ];
  if (block.kind === "unknown") {
    const meta = CLASSIFICATION_META[block.btc_orphan_class || "pending"];
    rows.push(["Orphan class", esc(meta ? meta.name : (block.btc_orphan_class || "Pending"))]);
  }
  if (block.kind === "error_block") {
    rows.push(["Consensus rejection", consensusRejectionValue(block.error_block_reason)]);
  }
  if (block.coinbase_tag) {
    rows.push(["Coinbase tag", esc(block.coinbase_tag)]);
  }
  rows.push(["Bitcoin miner", poolName(block.bitcoin_miner_pool)]);
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

// Formats child block time alongside its exact offset from the Bitcoin header time.
function childTimeCell(childTime, parentHeaderTime) {
  const stamp = unavailableEvidence(childTime, formatEpoch);
  if (!Number.isSafeInteger(childTime) || !Number.isSafeInteger(parentHeaderTime)) return stamp;
  const offset = childTime - parentHeaderTime;
  if (!Number.isSafeInteger(offset)) return stamp;
  const sign = offset > 0 ? "+" : "";
  const compact = fmtDelta(offset);
  const exactShort = `${sign}${offset}s`;
  const visible = compact === exactShort ? compact : `${compact} (${exactShort})`;
  const title = `${exactShort} from the Bitcoin header time`;
  return `${stamp} <span class="child-time-offset" title="${esc(title)}">${esc(visible)} vs Bitcoin</span>`;
}

function unavailableEvidence(value, render = copyValue) {
  if (value === null || value === undefined) {
    return `<span class="null-value">unavailable</span>`;
  }
  return render(value);
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

function renderSourcesAndCapture(summary = {}) {
  const rows = [
    ["Sources", formatSourceList(summary.sources || [])],
    ["Distinct", formatScalar(summary.distinct_sources)],
    ["AuxPoW Chains", formatScalar(summary.auxpow_chain_count)],
  ];
  return kvRows(rows);
}

function renderEvents(events, parentHeaderTime) {
  if (!events.length) return `<div class="empty">No auxiliary blocks</div>`;
  return events.map((event) => {
    const knownChildPool = event.child_miner_pool?.known ? event.child_miner_pool : null;
    const poolSuffix = knownChildPool ? ` · ${esc(knownChildPool.name)}` : "";
    const slotSuffix = event.slot_index != null ? ` · slot ${esc(event.slot_index)}` : "";
    const summary = `${esc(chainDisplayName(event.child_chain))} · ${esc(event.child_height ?? "height unavailable")}${slotSuffix}${poolSuffix}`;
    const rows = [
      ["Source", formatSourceRef(event.source)],
      ["Child Hash", unavailableEvidence(event.child_block_hash, (hash) => explorerCopyValue(hash, event.child_chain, {
        hash,
        height: event.child_height,
      }))],
      ["Child Header", unavailableEvidence(event.child_header_hex)],
      ["Child Time", `${childTimeCell(event.child_block_time, parentHeaderTime)} ${auxpowInfoButton("child_time")}`],
      ["Child nBits", unavailableEvidence(event.child_nbits)],
      ["PoW (parent_target / aux_target)", `${formatScalar(event.pow_validates_btc_target)} / ${formatScalar(event.pow_validates_child_target)} ${auxpowInfoButton("targets")}`],
    ];
    if (event.slot_index != null) rows.push(["Slot index", `${formatScalar(event.slot_index)} ${auxpowInfoButton("slot_index")}`]);
    if (event.chain_id != null) rows.push(["Chain id", `${formatScalar(event.chain_id)} ${auxpowInfoButton("chain_id")}`]);
    if (knownChildPool) rows.push(["Child miner", poolName(knownChildPool)]);
    const rsk = event.rsk ? renderRsk(event.rsk) : "";
    const auxProof = event.aux_proof
      ? renderAuxProof(event.aux_proof)
      : renderUndecodedProof(event.aux_merkle_proof_hex);
    return `<details class="event-block"><summary>${summary}</summary>${kvRows(rows)}${rsk}${auxProof}</details>`;
  }).join("");
}

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

function renderUndecodedProof(hex) {
  if (!hex) return "";
  const bytes = Math.floor(hex.length / 2);
  return `<details class="collapse"><summary>Proof bytes (undecoded · ${esc(bytes)} bytes)</summary>${kvRows([
    ["aux_merkle_proof", copyOnly(hex)],
  ])}</details>`;
}

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

// Cross-links a finite delta to the distribution view; null stays plain text.
function formatHeaderTimeDelta(value) {
  if (value === null || value === undefined) return formatScalar(value);
  const seconds = Number(value);
  if (!Number.isFinite(seconds)) return formatScalar(value);
  const prose = seconds === 0
    ? "winning and stale block header timestamps are equal to the second"
    : `winning block header timestamp is ${Math.abs(seconds)}s `
      + `${seconds > 0 ? "after" : "before"} the stale block header timestamp`;
  return `<button class="kv-link" type="button" data-action="delta"`
    + ` title="Show this competition in the distribution">${prose}</button>`;
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
  consensusRuleHelpFor,
  renderDrawer,
  kvRows,
  errorSummary,
};
