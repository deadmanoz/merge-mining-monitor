// Data-native evidence figures for Findings. The generated corpus owns the
// values and annotations; this module owns only scales and presentation.

import { esc } from "./frontend-state.js?v=0.7.4";

const DAY_MS = 86_400_000;
const CHART = { width: 880, height: 300, pad: { left: 64, right: 24, top: 54, bottom: 36 } };

function instantMs(value) {
  return Date.parse(value.includes("T") ? `${value}:00Z` : `${value}T00:00:00Z`);
}

function figureValue(value) {
  if (Number.isInteger(value)) return value.toLocaleString("en-US");
  return Math.abs(value) >= 10 ? value.toFixed(1) : value.toFixed(2).replace(/0+$/, "").replace(/\.$/, "");
}

function dateLabel(value, rangeMs) {
  const date = new Date(instantMs(value));
  if (rangeMs < 3 * DAY_MS) {
    return date.toLocaleString("en-GB", {
      timeZone: "UTC",
      day: "2-digit",
      month: "short",
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
    });
  }
  if (rangeMs > 540 * DAY_MS) {
    return date.toLocaleDateString("en-GB", { timeZone: "UTC", month: "short", year: "numeric" });
  }
  return date.toLocaleDateString("en-GB", {
    timeZone: "UTC",
    day: "2-digit",
    month: "short",
    year: rangeMs > 300 * DAY_MS ? "numeric" : undefined,
  });
}

function renderSummary(metrics) {
  return `<div class="figure-summary" aria-label="Evidence summary">${metrics
    .map(
      (metric) => `<div class="figure-summary-item">
        <span class="figure-summary-label">${esc(metric.label)}</span>
        <strong>${esc(metric.value)}</strong>
        <span class="figure-summary-detail">${esc(metric.detail)}</span>
      </div>`,
    )
    .join("")}</div>`;
}

function renderLegend(series) {
  if (!series?.length) return "";
  return `<div class="figure-legend" aria-label="Series">${series
    .map(
      (item, index) => `<span class="figure-legend-item">
        <span class="figure-legend-swatch fig-series-${index} fig-mark-${esc(item.mark)} fig-style-${esc(item.style || "solid")}" aria-hidden="true"></span>
        ${esc(item.label)}
      </span>`,
    )
    .join("")}</div>`;
}

function niceStep(span, target = 4) {
  const rough = span / target || 1;
  const power = 10 ** Math.floor(Math.log10(rough));
  const scaled = rough / power;
  const multiple = scaled <= 1 ? 1 : scaled <= 2 ? 2 : scaled <= 5 ? 5 : 10;
  return multiple * power;
}

function yDomain(figure, values) {
  const observedMin = Math.min(...values);
  const observedMax = Math.max(...values);
  const rawSpan = observedMax - observedMin || Math.abs(observedMax) || 1;
  let lo = figure.y_min ?? (observedMin >= 0 ? Math.max(0, observedMin - rawSpan * 0.08) : observedMin - rawSpan * 0.08);
  let hi = figure.y_max ?? observedMax + rawSpan * 0.1;
  const step = niceStep(hi - lo);
  if (figure.y_min === undefined) lo = Math.floor(lo / step) * step;
  if (figure.y_max === undefined) hi = Math.ceil(hi / step) * step;
  if (lo === hi) hi = lo + 1;
  return { lo, hi };
}

function seriesMark(series, index, px, py, baselineY, xSpan) {
  const klass = `fig-series fig-series-${index} fig-style-${series.style || "solid"}`;
  if (series.mark === "line") {
    const path = series.points
      .map((point, pointIndex) => `${pointIndex ? "L" : "M"} ${px(instantMs(point.t)).toFixed(1)} ${py(point.v).toFixed(1)}`)
      .join(" ");
    const last = series.points[series.points.length - 1];
    return `<path class="${klass}" d="${path}" fill="none" />
      <circle class="fig-series-end fig-series-${index}" cx="${px(instantMs(last.t)).toFixed(1)}" cy="${py(last.v).toFixed(1)}" r="3.4" />`;
  }
  if (series.mark === "bar") {
    const deltas = series.points.slice(1).map((point, pointIndex) => instantMs(point.t) - instantMs(series.points[pointIndex].t));
    const minDelta = deltas.length ? Math.min(...deltas) : xSpan;
    const plotWidth = CHART.width - CHART.pad.left - CHART.pad.right;
    const barWidth = Math.max(5, Math.min(54, (minDelta / xSpan) * plotWidth * 0.62));
    return series.points
      .map((point) => {
        const x = px(instantMs(point.t)) - barWidth / 2;
        const y = py(point.v);
        return `<rect class="${klass}" x="${x.toFixed(1)}" y="${Math.min(y, baselineY).toFixed(1)}" width="${barWidth.toFixed(1)}" height="${Math.max(1, Math.abs(baselineY - y)).toFixed(1)}" rx="1.5" />`;
      })
      .join("");
  }
  return series.points
    .map((point) => {
      const x = px(instantMs(point.t));
      const y = py(point.v);
      return `<line class="${klass} fig-lollipop-stem" x1="${x.toFixed(1)}" y1="${baselineY.toFixed(1)}" x2="${x.toFixed(1)}" y2="${y.toFixed(1)}" />
        <circle class="${klass} fig-lollipop-dot" cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="4" />`;
    })
    .join("");
}

function annotationAnchor(x, width, pad) {
  if (x < pad.left + 100) return { anchor: "start", dx: 6 };
  if (x > width - pad.right - 100) return { anchor: "end", dx: -6 };
  return { anchor: "middle", dx: 0 };
}

function renderSeriesChart(figure) {
  const { width: W, height: H, pad: PAD } = CHART;
  const points = figure.series.flatMap((series) => series.points);
  const xs = points.map((point) => instantMs(point.t));
  const values = [...points.map((point) => point.v), ...(figure.references || []).map((reference) => reference.v)];
  const x0 = Math.min(...xs);
  const x1 = Math.max(...xs);
  const xSpan = x1 - x0 || 1;
  const domain = yDomain(figure, values);
  const plotWidth = W - PAD.left - PAD.right;
  const plotHeight = H - PAD.top - PAD.bottom;
  const px = (value) => PAD.left + ((value - x0) / xSpan) * plotWidth;
  const py = (value) => PAD.top + ((domain.hi - value) / (domain.hi - domain.lo)) * plotHeight;
  const baselineY = py(Math.max(domain.lo, Math.min(domain.hi, 0)));

  const bands = (figure.bands || [])
    .map((band) => {
      const from = px(instantMs(band.from));
      const to = px(instantMs(band.to));
      const center = (from + to) / 2;
      return `<g class="fig-band">
        <rect x="${from.toFixed(1)}" y="${PAD.top}" width="${Math.max(1, to - from).toFixed(1)}" height="${plotHeight}" />
        <text x="${center.toFixed(1)}" y="${(PAD.top + plotHeight * 0.38).toFixed(1)}" text-anchor="middle" class="fig-annotation">
          <tspan x="${center.toFixed(1)}">${esc(band.label)}</tspan>
          ${band.detail ? `<tspan x="${center.toFixed(1)}" dy="14" class="fig-annotation-detail">${esc(band.detail)}</tspan>` : ""}
        </text>
      </g>`;
    })
    .join("");

  const yTicks = [];
  const tickCount = 4;
  for (let index = 0; index <= tickCount; index += 1) {
    yTicks.push(domain.lo + ((domain.hi - domain.lo) * index) / tickCount);
  }
  const grid = yTicks
    .map((value) => {
      const y = py(value);
      return `<line class="fig-grid" x1="${PAD.left}" y1="${y.toFixed(1)}" x2="${W - PAD.right}" y2="${y.toFixed(1)}" />
        <text class="fig-tick" x="${PAD.left - 9}" y="${(y + 3.5).toFixed(1)}" text-anchor="end">${esc(figureValue(value))}</text>`;
    })
    .join("");
  const references = (figure.references || [])
    .map((reference) => {
      const y = py(reference.v);
      return `<line class="fig-reference" x1="${PAD.left}" y1="${y.toFixed(1)}" x2="${W - PAD.right}" y2="${y.toFixed(1)}" />
        <text class="fig-reference-label" x="${W - PAD.right - 4}" y="${(y - 5).toFixed(1)}" text-anchor="end">${esc(reference.label)}</text>`;
    })
    .join("");
  const marks = figure.series
    .map((series, index) => seriesMark(series, index, px, py, baselineY, xSpan))
    .join("");
  const markers = (figure.markers || [])
    .map((marker) => {
      const x = px(instantMs(marker.t));
      const { anchor, dx } = annotationAnchor(x, W, PAD);
      return `<line class="fig-marker" x1="${x.toFixed(1)}" y1="${PAD.top - 4}" x2="${x.toFixed(1)}" y2="${H - PAD.bottom}" />
        <text class="fig-marker-label" x="${(x + dx).toFixed(1)}" y="${PAD.top - 27}" text-anchor="${anchor}">
          <tspan x="${(x + dx).toFixed(1)}">${esc(marker.label)}</tspan>
          ${marker.detail ? `<tspan x="${(x + dx).toFixed(1)}" dy="13" class="fig-annotation-detail">${esc(marker.detail)}</tspan>` : ""}
        </text>`;
    })
    .join("");
  const fallbackTicks = [x0, x1].map((instant) => new Date(instant).toISOString().slice(0, 16));
  const ticks = (figure.x_ticks?.length ? figure.x_ticks : fallbackTicks)
    .map((tick, index, all) => {
      const x = px(instantMs(tick));
      const anchor = index === 0 ? "start" : index === all.length - 1 ? "end" : "middle";
      return `<text class="fig-tick" x="${x.toFixed(1)}" y="${H - 9}" text-anchor="${anchor}">${esc(dateLabel(tick, xSpan))}</text>`;
    })
    .join("");

  return `${renderLegend(figure.series)}
    <div class="figure-plot-scroll" tabindex="0" aria-label="Scrollable chart">
      <svg class="figure-plot" viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(figure.accessible_summary)}">
        ${bands}${grid}${references}${marks}${markers}${ticks}
        <text class="fig-axis-title" transform="translate(16 ${(PAD.top + plotHeight / 2).toFixed(1)}) rotate(-90)" text-anchor="middle">${esc(figure.y_label)}</text>
      </svg>
    </div>`;
}

function timelineExtent(figure) {
  const instants = [
    ...(figure.events || []).map((event) => instantMs(event.t)),
    ...(figure.intervals || []).flatMap((interval) => [instantMs(interval.from), instantMs(interval.to)]),
  ];
  return [Math.min(...instants), Math.max(...instants)];
}

function renderTimeline(figure) {
  const W = 880;
  const PAD = { left: 116, right: 30, top: 44, bottom: 36 };
  const lanes = [];
  for (const item of [...(figure.intervals || []), ...(figure.events || [])]) {
    if (!lanes.includes(item.lane)) lanes.push(item.lane);
  }
  const H = PAD.top + PAD.bottom + Math.max(1, lanes.length) * 72;
  const [x0, x1] = timelineExtent(figure);
  const xSpan = x1 - x0 || 1;
  const px = (value) => PAD.left + ((value - x0) / xSpan) * (W - PAD.left - PAD.right);
  const laneY = (lane) => PAD.top + lanes.indexOf(lane) * 72 + 28;

  let cadence = "";
  if (figure.cadence) {
    const step = figure.cadence.days * DAY_MS;
    const ticks = [];
    for (let at = x0 + step; at < x1; at += step) ticks.push(at);
    cadence = `${ticks
      .map((at) => `<line class="timeline-cadence" x1="${px(at).toFixed(1)}" y1="${PAD.top - 8}" x2="${px(at).toFixed(1)}" y2="${H - PAD.bottom}" />`)
      .join("")}
      <text class="fig-reference-label" x="${W - PAD.right}" y="${PAD.top - 17}" text-anchor="end">${esc(figure.cadence.label)}</text>`;
  }
  const laneGuides = lanes
    .map((lane) => `<text class="timeline-lane-label" x="${PAD.left - 12}" y="${laneY(lane) + 4}" text-anchor="end">${esc(lane)}</text>
      <line class="timeline-lane" x1="${PAD.left}" y1="${laneY(lane)}" x2="${W - PAD.right}" y2="${laneY(lane)}" />`)
    .join("");
  const intervals = (figure.intervals || [])
    .map((interval, index) => {
      const from = px(instantMs(interval.from));
      const to = px(instantMs(interval.to));
      const y = laneY(interval.lane);
      return `<line class="timeline-interval fig-series-${index}" x1="${from.toFixed(1)}" y1="${y}" x2="${to.toFixed(1)}" y2="${y}" />
        <text class="fig-annotation" x="${((from + to) / 2).toFixed(1)}" y="${y - 13}" text-anchor="middle">${esc(interval.label)}</text>`;
    })
    .join("");
  const events = (figure.events || [])
    .map((event, index) => {
      const x = px(instantMs(event.t));
      const y = laneY(event.lane);
      const { anchor, dx } = annotationAnchor(x, W, PAD);
      const above = index % 2 === 0;
      const labelY = above ? y - 20 : y + 28;
      return `<line class="timeline-event-line" x1="${x.toFixed(1)}" y1="${y - 13}" x2="${x.toFixed(1)}" y2="${y + 13}" />
        <circle class="timeline-event-dot" cx="${x.toFixed(1)}" cy="${y}" r="4.5" />
        <text class="timeline-event-label" x="${(x + dx).toFixed(1)}" y="${labelY}" text-anchor="${anchor}">
          <tspan x="${(x + dx).toFixed(1)}">${esc(event.label)}</tspan>
          ${event.detail ? `<tspan x="${(x + dx).toFixed(1)}" dy="13" class="fig-annotation-detail">${esc(event.detail)}</tspan>` : ""}
        </text>`;
    })
    .join("");
  const ticks = (figure.x_ticks?.length ? figure.x_ticks : [new Date(x0).toISOString().slice(0, 10), new Date(x1).toISOString().slice(0, 10)])
    .map((tick, index, all) => {
      const x = px(instantMs(tick));
      const anchor = index === 0 ? "start" : index === all.length - 1 ? "end" : "middle";
      return `<text class="fig-tick" x="${x.toFixed(1)}" y="${H - 9}" text-anchor="${anchor}">${esc(dateLabel(tick, xSpan))}</text>`;
    })
    .join("");

  return `<div class="figure-plot-scroll" tabindex="0" aria-label="Scrollable timeline">
    <svg class="figure-plot" viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(figure.accessible_summary)}">
      ${cadence}${laneGuides}${intervals}${events}${ticks}
    </svg>
  </div>`;
}

function renderFigure(figure) {
  const visual = figure.kind === "event-timeline" ? renderTimeline(figure) : renderSeriesChart(figure);
  return `<figure class="finding-figure finding-figure-${esc(figure.kind)}">
    ${renderSummary(figure.summary)}
    <div class="finding-figure-visual">${visual}</div>
    <figcaption>${esc(figure.caption)}</figcaption>
    <span class="fig-note">${esc(figure.note)}</span>
  </figure>`;
}

export { renderFigure };
