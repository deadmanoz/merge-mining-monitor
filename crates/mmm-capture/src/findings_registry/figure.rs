//! Validated data grammar for editorial evidence figures.

use serde::{Deserialize, Serialize};
use time::{Date, PrimitiveDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum FigureKind {
    SeriesChart,
    EventTimeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum FigureMark {
    Line,
    Bar,
    Lollipop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum FigureLineStyle {
    Solid,
    Dashed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigurePoint {
    t: String,
    v: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureSeries {
    label: String,
    mark: FigureMark,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    style: Option<FigureLineStyle>,
    points: Vec<FigurePoint>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureMetric {
    label: String,
    value: String,
    detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureMarker {
    t: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureBand {
    from: String,
    to: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureReference {
    v: f64,
    label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureEvent {
    t: String,
    lane: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureInterval {
    from: String,
    to: String,
    lane: String,
    label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FigureCadence {
    days: u32,
    label: String,
}

/// One structured evidence figure, rendered client-side from data so it stays
/// theme-aware and drift-gated with the rest of the corpus.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Figure {
    kind: FigureKind,
    caption: String,
    accessible_summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary: Vec<FigureMetric>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    y_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    y_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    y_max: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    series: Vec<FigureSeries>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    markers: Vec<FigureMarker>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    bands: Vec<FigureBand>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    references: Vec<FigureReference>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    x_ticks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    events: Vec<FigureEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    intervals: Vec<FigureInterval>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    cadence: Option<FigureCadence>,
    note: String,
}

fn parse_date(value: &str) -> Result<Date, String> {
    let format =
        time::format_description::parse("[year]-[month]-[day]").expect("static date format parses");
    Date::parse(value, &format).map_err(|err| format!("invalid calendar date {value:?}: {err}"))
}

fn parse_instant(value: &str) -> Result<PrimitiveDateTime, String> {
    if let Ok(date) = parse_date(value) {
        return Ok(date.midnight());
    }
    let format = time::format_description::parse("[year]-[month]-[day]T[hour]:[minute]")
        .expect("static datetime format parses");
    PrimitiveDateTime::parse(value, &format).map_err(|err| {
        format!("invalid instant {value:?} (want YYYY-MM-DD or YYYY-MM-DDTHH:MM): {err}")
    })
}

fn validate_non_empty(
    ctx: &impl Fn(String) -> String,
    label: &str,
    value: &str,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(ctx(format!("empty {label}")));
    }
    Ok(())
}

fn validate_summary(ctx: &impl Fn(String) -> String, figure: &Figure) -> Result<(), String> {
    if figure.summary.is_empty() {
        return Err(ctx("figure summary needs at least one metric".into()));
    }
    for metric in &figure.summary {
        for (label, value) in [
            ("figure metric label", &metric.label),
            ("figure metric value", &metric.value),
            ("figure metric detail", &metric.detail),
        ] {
            validate_non_empty(ctx, label, value)?;
        }
    }
    Ok(())
}

fn validate_series_chart(
    ctx: &impl Fn(String) -> String,
    figure: &Figure,
) -> Result<(PrimitiveDateTime, PrimitiveDateTime), String> {
    let y_label = figure
        .y_label
        .as_deref()
        .ok_or_else(|| ctx("series chart needs y_label".into()))?;
    validate_non_empty(ctx, "figure y_label", y_label)?;
    if figure.series.is_empty() {
        return Err(ctx("series chart needs at least one series".into()));
    }
    if !figure.events.is_empty() || !figure.intervals.is_empty() || figure.cadence.is_some() {
        return Err(ctx("series chart cannot contain timeline fields".into()));
    }
    let mark = figure.series[0].mark;
    if figure.series.iter().any(|series| series.mark != mark) {
        return Err(ctx("series chart series must use the same mark".into()));
    }

    let mut first = None;
    let mut last = None;
    let mut value_min = f64::INFINITY;
    let mut value_max = f64::NEG_INFINITY;
    for series in &figure.series {
        validate_non_empty(ctx, "figure series label", &series.label)?;
        if series.points.len() < 2 {
            return Err(ctx(format!(
                "figure series {:?} needs at least two points",
                series.label
            )));
        }
        if series.mark != FigureMark::Line && series.style.is_some() {
            return Err(ctx(format!(
                "figure series {:?} sets a line style on a non-line mark",
                series.label
            )));
        }
        let mut instants = Vec::with_capacity(series.points.len());
        for point in &series.points {
            if !point.v.is_finite() {
                return Err(ctx(format!(
                    "figure point {:?} has a non-finite value",
                    point.t
                )));
            }
            let at = parse_instant(&point.t).map_err(|e| ctx(format!("figure point: {e}")))?;
            instants.push(at);
            first = Some(first.map_or(at, |value: PrimitiveDateTime| value.min(at)));
            last = Some(last.map_or(at, |value: PrimitiveDateTime| value.max(at)));
            value_min = value_min.min(point.v);
            value_max = value_max.max(point.v);
        }
        if instants.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ctx(format!(
                "figure series {:?} points must be strictly increasing in time",
                series.label
            )));
        }
    }
    let (first, last) = (
        first.expect("non-empty series points"),
        last.expect("non-empty series points"),
    );
    validate_y_domain(ctx, figure, value_min, value_max)?;
    Ok((first, last))
}

fn validate_y_domain(
    ctx: &impl Fn(String) -> String,
    figure: &Figure,
    value_min: f64,
    value_max: f64,
) -> Result<(), String> {
    if figure.y_min.is_some_and(|value| !value.is_finite())
        || figure.y_max.is_some_and(|value| !value.is_finite())
    {
        return Err(ctx("figure y bounds must be finite".into()));
    }
    if let (Some(min), Some(max)) = (figure.y_min, figure.y_max)
        && min >= max
    {
        return Err(ctx("figure y_min must be less than y_max".into()));
    }
    if figure.y_min.is_some_and(|min| min > value_min)
        || figure.y_max.is_some_and(|max| max < value_max)
    {
        return Err(ctx("figure y bounds exclude series values".into()));
    }
    for reference in &figure.references {
        validate_non_empty(ctx, "figure reference label", &reference.label)?;
        if !reference.v.is_finite() {
            return Err(ctx("figure reference value must be finite".into()));
        }
    }
    Ok(())
}

fn validate_event_timeline(
    ctx: &impl Fn(String) -> String,
    figure: &Figure,
) -> Result<(PrimitiveDateTime, PrimitiveDateTime), String> {
    if figure.y_label.is_some()
        || figure.y_min.is_some()
        || figure.y_max.is_some()
        || !figure.series.is_empty()
        || !figure.references.is_empty()
    {
        return Err(ctx(
            "event timeline cannot contain series-chart fields".into()
        ));
    }
    if figure.events.is_empty() && figure.intervals.is_empty() {
        return Err(ctx("event timeline needs events or intervals".into()));
    }
    let mut instants = Vec::new();
    for event in &figure.events {
        validate_non_empty(ctx, "timeline event lane", &event.lane)?;
        validate_non_empty(ctx, "timeline event label", &event.label)?;
        instants.push(parse_instant(&event.t).map_err(|e| ctx(format!("timeline event: {e}")))?);
    }
    for interval in &figure.intervals {
        validate_non_empty(ctx, "timeline interval lane", &interval.lane)?;
        validate_non_empty(ctx, "timeline interval label", &interval.label)?;
        let from = parse_instant(&interval.from)
            .map_err(|e| ctx(format!("timeline interval start: {e}")))?;
        let to =
            parse_instant(&interval.to).map_err(|e| ctx(format!("timeline interval end: {e}")))?;
        if from >= to {
            return Err(ctx("timeline interval start must precede its end".into()));
        }
        instants.extend([from, to]);
    }
    if let Some(cadence) = &figure.cadence {
        if cadence.days == 0 {
            return Err(ctx("timeline cadence days must be positive".into()));
        }
        validate_non_empty(ctx, "timeline cadence label", &cadence.label)?;
    }
    instants.sort_unstable();
    Ok((
        instants[0],
        *instants.last().expect("timeline has at least one instant"),
    ))
}

fn validate_annotations(
    ctx: &impl Fn(String) -> String,
    figure: &Figure,
    first: PrimitiveDateTime,
    last: PrimitiveDateTime,
) -> Result<(), String> {
    for marker in &figure.markers {
        validate_non_empty(ctx, "figure marker label", &marker.label)?;
        let at = parse_instant(&marker.t).map_err(|e| ctx(format!("figure marker: {e}")))?;
        if at < first || at > last {
            return Err(ctx(format!(
                "figure marker {:?} is outside the plotted range",
                marker.t
            )));
        }
    }
    for band in &figure.bands {
        validate_non_empty(ctx, "figure band label", &band.label)?;
        let from = parse_instant(&band.from).map_err(|e| ctx(format!("figure band: {e}")))?;
        let to = parse_instant(&band.to).map_err(|e| ctx(format!("figure band: {e}")))?;
        if from >= to || from < first || to > last {
            return Err(ctx(
                "figure band must be ordered inside the plotted range".into()
            ));
        }
    }
    for tick in &figure.x_ticks {
        let at = parse_instant(tick).map_err(|e| ctx(format!("figure x tick: {e}")))?;
        if at < first || at > last {
            return Err(ctx(format!(
                "figure x tick {tick:?} is outside the plotted range"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_figure(
    ctx: &impl Fn(String) -> String,
    figure: &Figure,
) -> Result<(), String> {
    for (label, value) in [
        ("figure caption", &figure.caption),
        ("figure accessible_summary", &figure.accessible_summary),
        ("figure note", &figure.note),
    ] {
        validate_non_empty(ctx, label, value)?;
    }
    validate_summary(ctx, figure)?;
    let (first, last) = match figure.kind {
        FigureKind::SeriesChart => validate_series_chart(ctx, figure)?,
        FigureKind::EventTimeline => validate_event_timeline(ctx, figure)?,
    };
    validate_annotations(ctx, figure, first, last)
}
