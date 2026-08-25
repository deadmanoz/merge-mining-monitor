import { blockExplorer } from "./explorer-links.js?v=0.7.0";
import { fmtDelta } from "./delta-scales.js?v=0.7.0";
import { $, chainDisplayName, CLASSIFICATION_META, esc, formatEpoch, formatScalar, formatSourceList, formatSourceRef, state } from "./frontend-state.js?v=0.7.0";


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
  child_time: {
    name: "Child Time",
    // Provenance-neutral: this topic is shown for every event, including the
    // RSK, Hathor and historical rows whose stamp was never checked against the
    // commitment, so the kicker must not assert that it was.
    meta: "The auxiliary block's own claimed timestamp, and its offset from Bitcoin",
    body: [
      "The auxiliary block's own timestamp, not a monitor capture time and not when the block reached its network. Whoever builds the child template writes it; the monitor observes no build and records only the claim. Which field it is depends on the chain: the header nTime for the Namecoin family, the block timestamp for RSK, the block transaction timestamp for Hathor.",
      "It usually sits behind the Bitcoin header time, and that is ordinary. The child data is committed into the Bitcoin coinbase before the Bitcoin work exists, so the stamp is sealed and cannot be refreshed later; one template is reused across many Bitcoin jobs, so it is stamped earlier than the parent that finally carries it.",
      "How much it proves varies. The Child Header row says whether bytes are stored to re-derive the stamp from, which is narrower than whether the Bitcoin block committed to it, and narrower again than a check having run: the header and the timestamp fill independently, so an event assembled from separate observations may never have had the two compared. A positive offset is an ordering disagreement worth investigating, never proof that either clock is wrong.",
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

// The Bitcoin consensus rules a catalogued error block is proven to break, keyed
// by the research catalogue's primary `rejection_reason` token. Each entry names
// the rule in prose and says what the block did wrong, so the drawer shows more
// than a raw snake_case token.
//
// The vocabulary is fixed and small, but deliberately NOT exhaustive here: only
// eight of these tokens have a committed catalogue row today, and the research
// classifier can emit tokens this map has not seen yet. An unmapped token falls
// back to its raw value with no help control (see `consensusRuleHelpFor` and
// `renderParentBlock`) rather than opening an empty or mislabelled dialog.
const CONSENSUS_RULE_HELP = {
  bip34_v2_coinbase_height_mismatch: {
    name: "BIP34 coinbase height mismatch (version 2+)",
    meta: "Coinbase scriptSig does not begin with this block's height",
    body: [
      "BIP34 requires a version 2 or newer block to begin its coinbase scriptSig with its own height, serialized exactly. This block's coinbase carries a different height, so it is invalid regardless of its proof of work.",
      "This is BIP34's first enforcement stage, active from height 224,413, where the rule bound only blocks that opted in to version 2.",
    ],
  },
  bip34_coinbase_height_mismatch: {
    name: "BIP34 coinbase height mismatch",
    meta: "Coinbase height prefix mandatory and wrong here",
    body: [
      "From height 227,931 BIP34's coinbase-height prefix is mandatory for every valid block, and version 1 blocks are rejected outright. This block's coinbase scriptSig does not carry its own serialized height.",
      "Bitcoin Core buries this deployment, so the monitor's catalogue applies the rule explicitly rather than relying on a node to replay it for an off-chain branch.",
    ],
  },
  bip34_v2_coinbase_height_missing: {
    name: "BIP34 coinbase height missing (version 2+)",
    meta: "Version 2+ block with no coinbase height prefix at all",
    body: [
      "A version 2 or newer block from BIP34's first enforcement stage whose coinbase scriptSig carries no serialized height prefix at all, as distinct from carrying the wrong one.",
    ],
  },
  bip34_coinbase_height_missing: {
    name: "BIP34 coinbase height missing",
    meta: "No coinbase height prefix where one is mandatory",
    body: [
      "From height 227,931 every valid block must carry its serialized height at the start of the coinbase scriptSig. This block carries none at all, as distinct from carrying the wrong one.",
    ],
  },
  bip34_block_version_below_2: {
    name: "Block version below 2 (BIP34)",
    meta: "Version 1 block after BIP34 made version 2 the minimum",
    body: [
      "BIP34 set a minimum block version of 2 from height 227,931. This header declares a lower version.",
      "The catalogue applies a hard height cutover at the observed activation height. Bitcoin Core's real rule was a rolling 750-of-1000 lock-in then 950-of-1000 enforcement, so treat a minimum-version verdict as a canonical-context judgement rather than a universal one.",
    ],
  },
  bip66_block_version_below_3: {
    name: "Block version below 3 (BIP66)",
    meta: "Version predates BIP66's strict DER minimum",
    body: [
      "BIP66 set a minimum block version of 3 from height 363,725. This header declares a lower version.",
      "As with the other minimum-version rules, the catalogue uses a hard height cutover at the observed activation height rather than replaying Bitcoin Core's rolling supermajority threshold.",
    ],
  },
  bip65_block_version_below_4: {
    name: "Block version below 4 (BIP65)",
    meta: "Version predates BIP65's CHECKLOCKTIMEVERIFY minimum",
    body: [
      "BIP65 set a minimum block version of 4 from height 388,381. This header declares a lower version.",
      "As with the other minimum-version rules, the catalogue uses a hard height cutover at the observed activation height rather than replaying Bitcoin Core's rolling supermajority threshold.",
    ],
  },
  coinbase_scriptsig_length_above_100: {
    name: "Coinbase scriptSig too long",
    meta: "Serialized length outside the 2-100 byte bound",
    body: [
      "A coinbase scriptSig must serialize to between 2 and 100 bytes. This block's exceeds 100.",
      "The token name is historical; the gate enforces the full two-sided bound.",
    ],
  },
  coinbase_scriptsig_length_below_2: {
    name: "Coinbase scriptSig too short",
    meta: "Serialized length below the 2-byte minimum",
    body: [
      "A coinbase scriptSig must serialize to between 2 and 100 bytes. This block's is shorter than 2.",
    ],
  },
  median_time_past_violation: {
    name: "Block time not after median-time-past",
    meta: "Timestamp violates the median-time-past rule",
    body: [
      "A block's timestamp must be strictly greater than the median-time-past of its parent, the median of the previous eleven block times. This block's is not.",
      "The parent's median-time-past is committed canonical-chain context, so this rule re-derives offline with no live node.",
    ],
  },
  time_below_mtp: {
    name: "Block time at or below median-time-past",
    meta: "Timestamp at or under the canonical parent's median-time-past",
    body: [
      "This block's nTime is at or below the median-time-past of its canonical parent, which the median-time-past rule forbids.",
      "The parent's median-time-past is committed canonical-chain context, so this rule re-derives offline with no live node.",
    ],
  },
  nbits_retarget_not_applied: {
    name: "Retarget not applied",
    meta: "Carries the previous epoch's nBits at a retarget boundary",
    body: [
      "At a difficulty retarget boundary (a height divisible by 2016) this block still carries the previous epoch's nBits instead of the newly retargeted value, while its header hash nonetheless meets Bitcoin's real target at that height.",
      "This is distinct from an ordinary nBits mismatch away from a boundary, which usually means the header's difficulty context is another chain's rather than Bitcoin's. Such a header is not an error block at all.",
    ],
  },
  time_beyond_future_limit: {
    name: "Block time beyond the future limit",
    meta: "Timestamp more than two hours ahead of network-adjusted time",
    body: [
      "Bitcoin rejects a block whose timestamp is more than two hours beyond network-adjusted time.",
      "No catalogued block carries this token. Network-adjusted time cannot be reconstructed from committed offline evidence, so the rule is not mechanically re-checkable and the catalogue's offline validator rejects any row claiming it.",
    ],
  },
};

// Help for one catalogued consensus rule, or null when the token is outside the
// mapped vocabulary. Null is meaningful: the caller renders the raw token with no
// help control rather than an empty dialog.
//
// The own-property check matters: the token is server data, and a plain object's
// bracket lookup also resolves inherited members, so tokens like `constructor`,
// `toString` or `__proto__` would otherwise read as mapped rules and emit a help
// control whose dialog then throws on the missing `body`.
function consensusRuleHelpFor(token) {
  if (!token || !Object.prototype.hasOwnProperty.call(CONSENSUS_RULE_HELP, token)) return null;
  return CONSENSUS_RULE_HELP[token];
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

// The consensus-rule twin of auxpowInfoButton, opening #consensus-rule-dialog.
// Only ever emitted for a token consensusRuleHelpFor maps, so the dialog it
// opens always has content.
function consensusRuleInfoButton(token, help) {
  const label = `About ${esc(help.name)}`;
  return `<button class="icon-button auxpow-info-button" type="button" data-consensus-rule-info="${esc(token)}" aria-label="${label}" title="${label}">
    <svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" /></svg>
  </button>`;
}

// The "Consensus rejection" row value: the humanised rule name plus its help
// control when the token is mapped, otherwise the raw token alone. A catalogue
// row always carries a token, so the empty case is only reachable through a
// malformed payload.
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

// Pure block-detail renderer: builds the drawer HTML from a /block/:hash payload
// with no `state` access, so the dev fixture harness can render committed
// fixtures directly. `renderDrawer` is the thin `state`-reading caller.
function renderBlockDetailPayload(payload) {
  const block = payload.block;
  return [
    detailSection("Parent block (Bitcoin)", renderParentBlock(block)),
    payload.commitment ? detailSectionHelp("Merge-mining commitment", "commitment", renderCommitment(payload.commitment)) : "",
    detailSection("Sources & capture", renderSourcesAndCapture(block.source_summary)),
    detailSection("Auxiliary blocks", renderEvents(payload.event_details || [], block.header?.time)),
    payload.competition ? detailSection("Competition", renderCompetition(payload.competition)) : "",
    // An error block has no competition section because it never competed. Say
    // so rather than leaving the reader to infer meaning from a missing panel
    // that a stale block at the same height would have.
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
  // btc_orphan_class is a refinement of kind='unknown'; show it only there
  // (canonical/stale always have a null orphan class). Reuse the navigator's
  // label map; a null class is the pending, never-Core-checked case.
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

// The auxiliary stamp beside its offset from the Bitcoin header this event is
// committed to. Both are miner-set claims, and not necessarily by one operator:
// a Bitcoin pool can proxy child-chain operation elsewhere, which is why the two
// attributions are tracked apart. The offset is normally negative because the
// child stamp was settled when the template was built, while the parent one is
// set later, at job creation or rolled past it; neither is a clock reading the
// service records. The child_time help topic carries
// the mechanism and the caveats, including that a stored child header means the
// stamp is re-derivable rather than that a parent committed to it. That
// distinction is left to the help text and the Child Header row rather than
// gating the offset: a per-event authentication flag would be a new API field,
// which this change does not add.
// Rendered only when the subtraction is exact, so a pair JavaScript cannot
// subtract losslessly shows the time alone rather than a rounded offset dressed
// up as measured.
function childTimeCell(childTime, parentHeaderTime) {
  const stamp = unavailableEvidence(childTime, formatEpoch);
  if (!Number.isSafeInteger(childTime) || !Number.isSafeInteger(parentHeaderTime)) return stamp;
  const offset = childTime - parentHeaderTime;
  // Two safe operands can still difference into the imprecise range: child_block_time
  // is an unbounded i64 by contract, so a safe-but-extreme stamp minus a u32 parent
  // time lands past MAX_SAFE_INTEGER and the "exact seconds" title would be a lie.
  if (!Number.isSafeInteger(offset)) return stamp;
  // fmtDelta compacts to the largest fitting unit, which is what the distribution
  // view reads in, but it rounds two offsets a few seconds apart to the same
  // minute (-600 and -607 are both "-10m"). Comparing auxiliaries on one block is
  // exactly where that distinction matters, so the exact second count is rendered
  // VISIBLY rather than parked in a title: a title on a non-focusable span is
  // unreachable by keyboard and touch, and hiding the figure from sighted readers
  // to expose it only to assistive technology just moves the gap. It is appended
  // only where the compact form is lossy, so "+7s" is not padded to "+7s (+7s)".
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
      // The real auxiliary block time, not a monitor capture timestamp. The help
      // topic covers why it normally trails the Bitcoin header time.
      ["Child Time", `${childTimeCell(event.child_block_time, parentHeaderTime)} ${auxpowInfoButton("child_time")}`],
      ["Child nBits", unavailableEvidence(event.child_nbits)],
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

/// A derivable delta doubles as a cross-link into the distribution view, which
/// shows where this competition sits among all of them. An absent or
/// unrepresentable one stays plain text: it has no position on the symlog strip,
/// no bin and no outlier row, so the link would land on a view that cannot place
/// it. The selection is already in `state.selectedHash`, so the target view
/// needs no argument beyond the view id.
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
