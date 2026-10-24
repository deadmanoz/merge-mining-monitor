// The block page's relationship signals: every pool the parent names, and whether they disagree.
import { chainDisplayName, copyValue, esc, kvRows, protocolLabel } from "./frontend-state.js";

const RELATIONSHIP_SIGNALS_HELP = {
  name: "Relationship signals",
  meta: "Every pool this block's coinbase, child blocks and declarations name",
  body: [
    "A Bitcoin block can name a mining pool in several places: its coinbase tag and payout addresses, each merge-mined child block's reward address or coinbase tag, and the CORE and exSat accounts its coinbase declares. Each one is resolved to a pool through this project's registries.",
    "Mixed means these name two or more pools. The block was then probably built on another pool's jobs, for example a Binance Pool block whose RSK reward address is registered to AntPool.",
    "The signals are grouped by pool, and a group is marked as differing when its pool is not the reference: the block's Bitcoin miner, else the first pool a signal names. A signal is an association, not proof of who mined the block, built its template, or controls an address or account.",
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

function signalsSummary({ mixed, pools }) {
  if (mixed) return `<strong>Mixed signals:</strong> they name ${esc(pools.length)} pools, ${esc(poolList(pools))}.`;
  if (pools.length) return `Every signal names ${esc(poolList(pools))} or no known pool.`;
  return "No signal names a known pool.";
}

// One group per pool: the reference, then the other known pools as `pools` orders them, then
// every signal naming no known pool.
function signalGroups({ entries, pools, reference_pool: reference }) {
  const groups = pools.map((pool) => ({ name: pool.name, differs: pool.id !== reference.id, entries: entries.filter((entry) => entry.pool.id === pool.id) }));
  groups.push({ name: "No known pool", differs: false, entries: entries.filter((entry) => !entry.pool.known) });
  return groups.filter((group) => group.entries.length);
}

function renderSignalGroup({ name, differs, entries }) {
  const count = `${entries.length} signal${entries.length === 1 ? "" : "s"}`;
  const pill = differs ? ' <span class="state-pill signal-mixed-pill">differs</span>' : "";
  const head = `<span class="signal-group-head"><strong>${esc(name)}</strong> <span class="signal-group-count">${esc(count)}</span>${pill}</span>`;
  const rows = entries.map((entry) => [SIGNAL_LABELS[entry.kind]?.(entry) || entry.kind, copyValue(entry.value)]);
  return `<details class="event-block${differs ? " signal-differs" : ""}"><summary>${head}</summary>${kvRows(rows)}</details>`;
}

function renderRelationshipSignals(signals) {
  if (!signals.entries.length) return `<div class="empty">No coinbase tag, payout address, child identity or declared account in this block names a pool.</div>`;
  return `<div class="signal-summary">${signalsSummary(signals)}</div>${signalGroups(signals).map(renderSignalGroup).join("")}`;
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
