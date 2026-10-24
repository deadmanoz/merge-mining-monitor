// The block page's relationship signals: every pool the parent names, and whether they disagree.
import { chainDisplayName, copyValue, esc, poolName, protocolLabel } from "./frontend-state.js";

const RELATIONSHIP_SIGNALS_HELP = {
  name: "Relationship signals",
  meta: "Every pool this block's coinbase, child blocks and declarations name",
  body: [
    "A Bitcoin block can name a mining pool in several places: its coinbase tag and payout addresses, each merge-mined child block's reward address or coinbase tag, and the CORE and exSat accounts its coinbase declares. Each one is resolved to a pool through this project's registries.",
    "Mixed means these name two or more pools. The block was then probably built on another pool's jobs, for example a Binance Pool block whose RSK reward address is registered to AntPool.",
    "A row is marked as differing when it names a pool other than the reference: the block's Bitcoin miner, else the first pool a signal names. A signal is an association, not proof of who mined the block, built its template, or controls an address or account.",
  ],
};

const PROTOCOL_ACCOUNT_LABELS = {
  core_reward_address: `${protocolLabel("core")} reward address`,
  exsat_account: `${protocolLabel("exsat")} account`,
};

const SIGNAL_LABELS = {
  bitcoin_coinbase_tag: () => "Bitcoin coinbase tag",
  bitcoin_payout_address: () => "Bitcoin payout address",
  child_reward_address: (entry) => `${chainDisplayName(entry.source)} reward address`,
  child_coinbase_tag: (entry) => `${chainDisplayName(entry.source)} coinbase tag`,
  protocol_account: (entry) => PROTOCOL_ACCOUNT_LABELS[entry.namespace] || "Protocol account",
};

function poolList(pools) {
  const names = pools.map((pool) => pool.name);
  return names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names.at(-1)}` : names.join("");
}

// The reference is the first known pool, so it is known whenever an entry's pool is.
function differsFromReference(entry, reference) {
  return entry.pool.known && entry.pool.id !== reference.id;
}

function signalsSummary({ mixed, pools }) {
  if (mixed) return `<strong>Mixed signals:</strong> they name ${esc(pools.length)} pools, ${esc(poolList(pools))}.`;
  if (pools.length) return `Every signal names ${esc(poolList(pools))} or no known pool.`;
  return "No signal names a known pool.";
}

function renderRelationshipSignals(signals) {
  if (!signals.entries.length) return `<div class="empty">No coinbase tag, payout address, child identity or declared account in this block names a pool.</div>`;
  const rows = signals.entries.map((entry) => {
    const differs = differsFromReference(entry, signals.reference_pool);
    const label = SIGNAL_LABELS[entry.kind]?.(entry) || entry.kind;
    const pool = differs ? `${poolName(entry.pool)} <span class="state-pill signal-mixed-pill">differs</span>` : poolName(entry.pool);
    return `<tr${differs ? ' class="signal-differs"' : ""}><td>${esc(label)}</td><td>${copyValue(entry.value)}</td><td>${pool}</td></tr>`;
  });
  return `<div class="signal-summary">${signalsSummary(signals)}</div>`
    + `<table class="signal-table"><thead><tr><th>Signal</th><th>Value</th><th>Pool</th></tr></thead><tbody>${rows.join("")}</tbody></table>`;
}

// Beside "Bitcoin miner": a mixed block says so, and scrolls to its section, before the reader
// reaches it. A button rather than an in-page link, which would add a history entry.
function mixedSignalsChip(signals) {
  if (!signals?.mixed) return "";
  return ` <button type="button" class="state-pill signal-mixed-pill" data-scroll-topic="relationship_signals">Mixed signals · ${esc(signals.pools.length)} pools</button>`;
}

export {
  mixedSignalsChip,
  RELATIONSHIP_SIGNALS_HELP,
  renderRelationshipSignals,
};
