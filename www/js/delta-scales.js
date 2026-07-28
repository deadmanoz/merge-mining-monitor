// Pure scale, binning and formatting helpers for the header-time-delta view.
// No DOM and no module state, so the awkward parts (the symmetric-log mapping,
// the zero-centred binning, the null partition) stay directly testable.

/// Symmetric log: linear inside +/-T seconds, log decades beyond,
/// sign-preserving. T = 10s keeps the tightly-bound core off the zero tick
/// while still fitting seven decades of tail on one axis.
const SYMLOG_T = 10;

const symlog = (x) => {
  const a = Math.abs(x);
  return Math.sign(x) * (a <= SYMLOG_T ? a / SYMLOG_T : 1 + Math.log10(a / SYMLOG_T));
};

/// Inverse of `symlog`, for turning a pixel position back into seconds.
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

/// Compact signed duration: 0s, +26s, -31m, +7.5d.
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

/// Axis-tick form that keeps the unit readable at the step sizes the linear
/// axis actually uses: seconds stay seconds up to two minutes, so a 30s step
/// reads -90s rather than -1.5m.
function fmtAxis(seconds) {
  if (seconds === 0) return "0";
  const a = Math.abs(seconds);
  const sign = seconds < 0 ? "-" : "+";
  const [size, suffix] = a < 120 ? [1, "s"] : a < 7200 ? [60, "m"] : a < 172800 ? [3600, "h"] : [86400, "d"];
  return `${sign}${Number((a / size).toFixed(1))}${suffix}`;
}

const fmtTick = (seconds) => fmtDelta(seconds, { signed: false });
/// Unsigned span, for bin widths and window half-widths.
const fmtSpan = (seconds) => fmtAxis(seconds).replace(/^\+/, "");
const fmtInt = (n) => Number(n).toLocaleString("en-US");
/// Share as a percentage. 100% and 0% are reserved for exact ratios: rounding
/// 999/1000 up to "100%" while the outlier panel lists the excluded record is
/// the kind of disagreement that makes a reader distrust both numbers.
function fmtPct(x) {
  if (x >= 1) return "100%";
  if (x <= 0) return "0%";
  if (x > 0.999) return ">99.9%";
  if (x < 0.001) return "<0.1%";
  return `${(x * 100).toFixed(1)}%`;
}
/// Whole-percent form for axis ticks and quantile callouts.
const fmtPctRound = (x) => `${Math.round(x * 100)}%`;
/// UTC stamp, or a stable fallback when the epoch is not a representable date.
/// `stale_header_time` is an unbounded integer in the wire contract, and
/// `toISOString` throws RangeError outside the Date range; one such row reaching
/// an outlier label would abort the whole render.
function fmtUtc(epoch) {
  const date = new Date(epoch * 1000);
  if (Number.isNaN(date.getTime())) return "date unavailable";
  return date.toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
}

/// Date-only form of the same. A separate function rather than slicing the
/// full stamp, because slicing would also chop the unavailable fallback.
function fmtUtcDate(epoch) {
  const stamp = fmtUtc(epoch);
  return /^\d{4}-\d{2}-\d{2}/.test(stamp) ? stamp.slice(0, 10) : stamp;
}

/// Split rows into those with a usable delta and those without.
///
/// `header_time_delta_s` is nullable by contract (the API nulls a difference
/// outside i32). JavaScript coerces `null` to `0`, so feeding these straight
/// into the binner would file them in the zero bin and inflate the
/// "tied to the second" count. Every numeric view reads `usable`; `unavailable`
/// is only ever reported as a count.
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

// Tick and bin steps that land on readable clock durations rather than decimal
// seconds.
const TIME_STEPS = [
  1, 2, 5, 10, 15, 20, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 21600,
  43200, 86400, 172800, 604800, 1209600, 2592000, 7776000,
];

const BIN_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600, 21600, 86400, 604800];

/// Nice, zero-anchored ticks for the linear delta axis.
function linearTicks(half) {
  const target = (half * 2) / 8;
  // Past the largest clock-shaped step, synthesise one rather than repeating
  // the 90d entry: at the i32 extreme that put over 1,100 labels on the axis.
  const step = TIME_STEPS.find((s) => s >= target) ?? niceStep(target);
  const ticks = [];
  for (let v = 0; v <= half; v += step) {
    ticks.push(v);
    if (v > 0) ticks.unshift(-v);
  }
  return ticks.sort((a, b) => a - b);
}

/// Human-readable tick positions for the symmetric-log strip. Decade ticks
/// would land on 16m40s and 2h47m, so these are clock durations mapped through
/// `symlog` instead.
const SYM_TICKS = [0, 10, 60, 600, 3600, 86400, 604800, 2592000];

/// Hard ceiling on rendered bins. Well above any readable histogram, and far
/// below the point where materialising them costs anything.
const MAX_BINS = 401;

const binsFor = (half, width) => (half * 2) / width + 1;

/// Bin index for a delta, rounding ties away from zero on BOTH sides.
///
/// `Math.round` breaks ties toward positive infinity, so at a 10s width it puts
/// -5 in the zero bin and +5 in the positive one. That is not a mirror image,
/// and it quietly skews the sign-based counts and the diverging colours the
/// whole view is built on.
const binIndex = (delta, width) => Math.sign(delta) * Math.round(Math.abs(delta) / width);

/// Bin width: either an explicit pick, or a nice step giving roughly 110 bins,
/// widened if needed so the bin count stays bounded.
///
/// The window and the width are chosen independently, so nothing stops a fine
/// width being asked of a very wide window: the full range at one-second bins
/// is over thirteen million bins, which allocates for long enough to hang the
/// tab. When the request does not fit, the next step up that does is used
/// instead, and the meta line reports the width actually applied.
function binWidth(half, explicit) {
  const requested = explicit && explicit !== "auto"
    ? Number(explicit)
    : BIN_STEPS.find((step) => step >= (half * 2) / 110) ?? BIN_STEPS.at(-1);
  if (binsFor(half, requested) <= MAX_BINS) return requested;
  // Falling back to the widest listed step is not enough on its own: a
  // full-range i32 delta still needs ~7,100 weekly bins. Synthesise a width when
  // the table runs out.
  return BIN_STEPS.find((step) => binsFor(half, step) <= MAX_BINS)
    ?? Math.ceil((half * 2) / (MAX_BINS - 1));
}

/**
 * Zero-centred binning: bin k spans [k*w - w/2, k*w + w/2), so delta = 0 always
 * owns a bin of its own and the two signs stay mirror images. The window edges
 * are snapped to bin edges, so no rendered bar is ever a partial bin.
 *
 * The requested `half` is the single authority on membership, and the window
 * edges are exactly +/-half. The outermost bin on each side is clipped to that
 * edge rather than being allowed to overhang it, because a bin centred on
 * +/-half would otherwise extend half a width past the window and count records
 * the label, the brush and the outlier list all treat as outside. (The default
 * two-minute window at five-second bins would really have ended at 122.5s.)
 *
 * `rows` must already be delta-usable (see `partitionByDelta`).
 */
/// The bin a delta belongs to, by the same rule computeBins used to count it.
/// Callers that need to point AT a bin (marking the selection, say) have to
/// agree with the pass that filled them, so both go through binIndex and the
/// same clamp rather than re-deriving the arithmetic.
function binKeyFor({ w, half }, delta) {
  if (!Number.isFinite(delta)) return null;
  if (Math.abs(delta) > half) return null;
  const kMax = Math.floor(half / w);
  return clamp(binIndex(delta, w), -kMax, kMax);
}

function computeBins(rows, half, explicitWidth) {
  const w = binWidth(half, explicitWidth);
  // floor, not round: rounding up puts the outermost bin CENTRE beyond the
  // window (half=30 with 60s bins centres a bin on +60), and the chart places
  // bars by centre, so that bar renders off-plot with a degenerate 30..30 range.
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
    // Clamped: a record inside the window but past the last bin centre belongs
    // to that outermost, clipped bin.
    const k = clamp(binIndex(d, w), -kMax, kMax);
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  const bins = [];
  for (let k = -kMax; k <= kMax; k += 1) {
    // The outermost bins absorb everything between the last centre and the
    // window edge (records are clamped into them), so their reported range must
    // reach that edge. Without this, half=25 with a 30s width reports -15 … 15
    // while counting a +25 record.
    const lo = k === -kMax ? edgeLo : Math.max(k * w - w / 2, edgeLo);
    const hi = k === kMax ? edgeHi : Math.min(k * w + w / 2, edgeHi);
    // Position by the midpoint of the real bounds so a bin trimmed or extended
    // at the window edge renders where it actually sits.
    bins.push({ k, centre: (lo + hi) / 2, lo, hi, count: counts.get(k) ?? 0 });
  }
  return { bins, below, above, outside, w, edgeLo, edgeHi, half };
}

/// Map a count onto 0..1 for the chosen vertical transform.
function countScale(count, max, yscale) {
  if (max <= 0) return 0;
  if (yscale === "log") return Math.log10(1 + count) / Math.log10(1 + max);
  return count / max;
}

/// Ticks for the count axis plus the domain maximum they imply, so the topmost
/// gridline is the top of the plot rather than floating above it.
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

/// Linearly interpolated percentile of a pre-sorted numeric array.
///
/// Interpolated rather than nearest-index, so an even-sized sample gets the
/// conventional midpoint: nearest-index on [-5, 12] returns 12 for the median
/// instead of 3.5, biasing the headline tile toward the upper observation. This
/// also matches Postgres percentile_cont, which is what the same numbers are
/// checked against.
function quantile(sorted, p) {
  if (!sorted.length) return null;
  const position = clamp((sorted.length - 1) * p, 0, sorted.length - 1);
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  if (lower === upper) return sorted[lower];
  return sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower);
}

export {
  BIN_STEPS,
  MAX_BINS,
  SYMLOG_T,
  SYM_TICKS,
  binWidth,
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
  niceStep,
  partitionByDelta,
  quantile,
  symexp,
  symlog,
};
