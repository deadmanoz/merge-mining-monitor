// Symmetric log: linear inside +/-10s, log decades beyond, sign-preserving.
const SYMLOG_T = 10;

const symlog = (x) => {
  const a = Math.abs(x);
  return Math.sign(x) * (a <= SYMLOG_T ? a / SYMLOG_T : 1 + Math.log10(a / SYMLOG_T));
};

const symexp = (y) => {
  const a = Math.abs(y);
  return Math.sign(y) * (a <= 1 ? a * SYMLOG_T : SYMLOG_T * Math.pow(10, a - 1));
};

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

const UNITS = [
  [86400, "d"],
  [3600, "h"],
  [60, "m"],
  [1, "s"],
];

function fmtDelta(seconds, { signed = true } = {}) {
  if (seconds === 0) return "0s";
  const sign = seconds < 0 ? "-" : signed ? "+" : "";
  const a = Math.abs(seconds);
  const [size, suffix] = UNITS.find(([s]) => a >= s) ?? [1, "s"];
  const value = a / size;
  const text = size === 1
    ? String(Math.round(value))
    : value.toFixed(value < 10 ? 1 : 0).replace(/\.0$/, "");
  return `${sign}${text}${suffix}`;
}

function fmtAxis(seconds) {
  if (seconds === 0) return "0";
  const a = Math.abs(seconds);
  const sign = seconds < 0 ? "-" : "+";
  const [size, suffix] = a < 120 ? [1, "s"] : a < 7200 ? [60, "m"] : a < 172800 ? [3600, "h"] : [86400, "d"];
  return `${sign}${Number((a / size).toFixed(1))}${suffix}`;
}

const fmtTick = (seconds) => fmtDelta(seconds, { signed: false });
const fmtSpan = (seconds) => fmtAxis(seconds).replace(/^\+/, "");
const fmtInt = (n) => Number(n).toLocaleString("en-US");
function fmtPct(x) {
  if (x >= 1) return "100%";
  if (x <= 0) return "0%";
  if (x > 0.999) return ">99.9%";
  if (x < 0.001) return "<0.1%";
  return `${(x * 100).toFixed(1)}%`;
}
const fmtPctRound = (x) => `${Math.round(x * 100)}%`;
function fmtUtc(epoch) {
  const date = new Date(epoch * 1000);
  if (Number.isNaN(date.getTime())) return "date unavailable";
  return date.toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
}

function fmtUtcDate(epoch) {
  const stamp = fmtUtc(epoch);
  return /^\d{4}-\d{2}-\d{2}/.test(stamp) ? stamp.slice(0, 10) : stamp;
}

// Separate null/non-finite deltas so Number coercion never counts them as 0.
function partitionByDelta(rows) {
  const usable = [];
  const unavailable = [];
  for (const row of rows) {
    if (Number.isFinite(row.header_time_delta_s)) usable.push(row);
    else unavailable.push(row);
  }
  return { usable, unavailable };
}

function niceStep(raw) {
  const mag = Math.pow(10, Math.floor(Math.log10(Math.max(raw, 1e-9))));
  const norm = raw / mag;
  return (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10) * mag;
}

const TIME_STEPS = [
  1, 2, 5, 10, 15, 20, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 21600,
  43200, 86400, 172800, 604800, 1209600, 2592000, 7776000,
];

const BIN_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600, 21600, 86400, 604800];

function linearTicks(half) {
  const target = (half * 2) / 8;
  const step = TIME_STEPS.find((s) => s >= target) ?? niceStep(target);
  const ticks = [];
  for (let v = 0; v <= half; v += step) {
    ticks.push(v);
    if (v > 0) ticks.unshift(-v);
  }
  return ticks.sort((a, b) => a - b);
}

const SYM_TICKS = [0, 10, 60, 600, 3600, 86400, 604800, 2592000];

const MAX_BINS = 401;

const binsFor = (half, width) => (half * 2) / width + 1;

// Round ties away from zero on both sides so negative and positive bins stay symmetric.
const binIndex = (delta, width) => Math.sign(delta) * Math.round(Math.abs(delta) / width);

function binWidth(half, explicit) {
  const requested = explicit && explicit !== "auto"
    ? Number(explicit)
    : BIN_STEPS.find((step) => step >= (half * 2) / 110) ?? BIN_STEPS.at(-1);
  if (binsFor(half, requested) <= MAX_BINS) return requested;
  return BIN_STEPS.find((step) => binsFor(half, step) <= MAX_BINS)
    ?? Math.ceil((half * 2) / (MAX_BINS - 1));
}

function binKeyFor({ w, half }, delta) {
  if (!Number.isFinite(delta)) return null;
  if (Math.abs(delta) > half) return null;
  const kMax = Math.floor(half / w);
  return clamp(binIndex(delta, w), -kMax, kMax);
}

// Zero-centred binning clipped to +/-half at the outer edges.
function computeBins(rows, half, explicitWidth) {
  const w = binWidth(half, explicitWidth);
  const kMax = Math.floor(half / w);
  const edgeLo = -half;
  const edgeHi = half;
  const counts = new Map();
  const outside = [];
  let below = 0;
  let above = 0;
  for (const row of rows) {
    const d = row.header_time_delta_s;
    if (d < edgeLo) { below += 1; outside.push(row); continue; }
    if (d > edgeHi) { above += 1; outside.push(row); continue; }
    const k = clamp(binIndex(d, w), -kMax, kMax);
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  const bins = [];
  for (let k = -kMax; k <= kMax; k += 1) {
    const lo = k === -kMax ? edgeLo : Math.max(k * w - w / 2, edgeLo);
    const hi = k === kMax ? edgeHi : Math.min(k * w + w / 2, edgeHi);
    bins.push({ k, centre: (lo + hi) / 2, lo, hi, count: counts.get(k) ?? 0 });
  }
  return { bins, below, above, outside, w, edgeLo, edgeHi, half };
}

function countScale(count, max, yscale) {
  if (max <= 0) return 0;
  if (yscale === "log") return Math.log10(1 + count) / Math.log10(1 + max);
  return count / max;
}

function countAxis(maxCount, yscale) {
  if (maxCount <= 0) return { ticks: [0], domainMax: 1 };
  if (yscale === "log") {
    const ticks = [0];
    for (let p = 0; Math.pow(10, p) <= maxCount; p += 1) ticks.push(Math.pow(10, p));
    const domainMax = Math.pow(10, Math.ceil(Math.log10(maxCount)));
    if (ticks.at(-1) !== domainMax) ticks.push(domainMax);
    return { ticks, domainMax };
  }
  const step = niceStep(maxCount / 4);
  const domainMax = Math.ceil(maxCount / step) * step;
  const ticks = [];
  for (let v = 0; v <= domainMax + step / 2; v += step) ticks.push(Math.round(v));
  return { ticks, domainMax };
}

function quantile(sorted, p) {
  if (!sorted.length) return null;
  const position = clamp((sorted.length - 1) * p, 0, sorted.length - 1);
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  if (lower === upper) return sorted[lower];
  return sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower);
}

export {
  SYM_TICKS,
  clamp,
  binKeyFor,
  computeBins,
  countAxis,
  countScale,
  fmtAxis,
  fmtDelta,
  fmtInt,
  fmtPct,
  fmtPctRound,
  fmtSpan,
  fmtTick,
  fmtUtc,
  fmtUtcDate,
  linearTicks,
  partitionByDelta,
  quantile,
  symexp,
  symlog,
};
