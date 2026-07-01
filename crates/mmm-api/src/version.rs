//! Runtime version metadata and release notes.

use serde::Serialize;

const RELEASE_NOTES: &str = include_str!("../../../RELEASE_NOTES.md");
const RELEASE_NOTES_SOURCE: &str = "RELEASE_NOTES.md";
// The About dialog renders release notes in a dedicated, scrollable, per-release
// collapsible pane, so the projection is unbounded: every section and every item
// in RELEASE_NOTES.md is exposed. The `truncated` / `*_count` fields and the
// max-cap parameters threaded through `parse_release_notes` are retained as a
// dormant safety net, so reintroducing a hard cap (or surviving a runaway file)
// degrades gracefully without a wire-contract change.
const MAX_RELEASES: usize = usize::MAX;
const MAX_ITEMS_PER_RELEASE: usize = usize::MAX;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VersionPayload {
    pub(crate) version: &'static str,
    pub(crate) release_notes: ReleaseNotes,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReleaseNotes {
    pub(crate) source: &'static str,
    pub(crate) release_count: usize,
    pub(crate) truncated: bool,
    pub(crate) releases: Vec<ReleaseNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReleaseNote {
    pub(crate) version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) date: Option<String>,
    pub(crate) items: Vec<String>,
    pub(crate) item_count: usize,
    pub(crate) truncated: bool,
}

pub(crate) fn payload() -> VersionPayload {
    let parsed = parse_release_notes(RELEASE_NOTES, MAX_RELEASES, MAX_ITEMS_PER_RELEASE);
    VersionPayload {
        version: super::APPLICATION_VERSION,
        release_notes: ReleaseNotes {
            source: RELEASE_NOTES_SOURCE,
            release_count: parsed.release_count,
            truncated: parsed.release_count > parsed.releases.len(),
            releases: parsed.releases,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedReleaseNotes {
    releases: Vec<ReleaseNote>,
    release_count: usize,
}

fn parse_release_notes(notes: &str, max_releases: usize, max_items: usize) -> ParsedReleaseNotes {
    let mut releases = Vec::new();
    let mut release_count = 0;
    let mut current: Option<ReleaseBuilder> = None;

    for line in notes.lines() {
        if let Some((version, date)) = parse_release_heading(line) {
            if let Some(builder) = current.take()
                && releases.len() < max_releases
            {
                releases.push(builder.finish());
            }
            release_count += 1;
            current = (releases.len() < max_releases).then(|| ReleaseBuilder::new(version, date));
        } else if let Some(builder) = current.as_mut() {
            builder.consume_line(line, max_items);
        }
    }

    if releases.len() < max_releases
        && let Some(builder) = current
    {
        releases.push(builder.finish());
    }

    ParsedReleaseNotes {
        releases,
        release_count,
    }
}

fn parse_release_heading(line: &str) -> Option<(String, Option<String>)> {
    let heading = line.strip_prefix("## ")?.trim();
    if let Some(stripped) = heading.strip_prefix('[') {
        let close = stripped.find(']')?;
        let rest = stripped[close + 1..].trim();
        let date = rest
            .strip_prefix('-')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Some((stripped[..close].to_owned(), date))
    } else {
        let mut parts = heading.splitn(2, " - ");
        let version = parts.next()?.trim().to_owned();
        let date = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Some((version, date))
    }
}

struct ReleaseBuilder {
    version: String,
    date: Option<String>,
    items: Vec<String>,
    item_count: usize,
    current_item: Option<usize>,
}

impl ReleaseBuilder {
    fn new(version: String, date: Option<String>) -> Self {
        Self {
            version,
            date,
            items: Vec::new(),
            item_count: 0,
            current_item: None,
        }
    }

    fn consume_line(&mut self, line: &str, max_items: usize) {
        // RELEASE_NOTES.md is intentionally flat: top-level bullets plus
        // optional wrapped continuation lines for the previous bullet.
        if let Some(item) = line.strip_prefix("- ") {
            self.item_count += 1;
            if self.items.len() < max_items {
                self.items.push(item.trim().to_owned());
                self.current_item = Some(self.items.len() - 1);
            } else {
                self.current_item = None;
            }
            return;
        }

        let continuation = line.trim();
        if continuation.is_empty() {
            return;
        }
        if let Some(index) = self.current_item {
            self.items[index].push(' ');
            self.items[index].push_str(continuation);
        }
    }

    fn finish(self) -> ReleaseNote {
        ReleaseNote {
            version: self.version,
            date: self.date,
            truncated: self.item_count > self.items.len(),
            item_count: self.item_count,
            items: self.items,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_release_note_sections() {
        let notes = r#"
# Release Notes

## [Unreleased]

- First note spans
  multiple lines.
- Second note.
- Third note.

## [0.1.0] - 2026-06-23

- Released the first monitor build.
"#;

        let releases = parse_release_notes(notes, 2, 2);

        assert_eq!(releases.release_count, 2);
        assert_eq!(releases.releases.len(), 2);
        assert_eq!(releases.releases[0].version, "Unreleased");
        assert_eq!(releases.releases[0].date, None);
        assert_eq!(
            releases.releases[0].items,
            vec!["First note spans multiple lines.", "Second note."]
        );
        assert_eq!(releases.releases[0].item_count, 3);
        assert!(releases.releases[0].truncated);
        assert_eq!(releases.releases[1].version, "0.1.0");
        assert_eq!(releases.releases[1].date.as_deref(), Some("2026-06-23"));
    }

    #[test]
    fn parses_non_bracket_date_heading() {
        let releases = parse_release_notes("## 0.1.0 - 2026-06-23\n\n- Released.\n", 3, 8);

        assert_eq!(releases.release_count, 1);
        assert_eq!(releases.releases[0].version, "0.1.0");
        assert_eq!(releases.releases[0].date.as_deref(), Some("2026-06-23"));
    }

    #[test]
    fn notes_without_sections_yield_no_releases() {
        let releases = parse_release_notes("# Release Notes\n\nNo sections yet.\n", 3, 8);

        assert_eq!(releases.release_count, 0);
        assert!(releases.releases.is_empty());
    }

    #[test]
    fn reports_top_level_release_truncation_inputs() {
        let releases = parse_release_notes(
            "## [Unreleased]\n\n- Current.\n\n## [0.1.0]\n\n- First.\n\n## [0.0.1]\n\n- Older.\n",
            2,
            8,
        );

        assert_eq!(releases.release_count, 3);
        assert_eq!(releases.releases.len(), 2);
        assert_eq!(releases.releases[0].version, "Unreleased");
        assert_eq!(releases.releases[1].version, "0.1.0");
    }

    #[test]
    fn production_caps_expose_every_section_and_item() {
        // Four sections (> the old 3-release cap), the newest with nine bullets
        // (> the old 8-item cap): the lifted production constants return them all,
        // untruncated, so the scrollable About pane sees the whole file.
        let notes = "## [Unreleased]\n\n- a\n- b\n- c\n- d\n- e\n- f\n- g\n- h\n- i\n\n\
## [0.9.0] - 2026-06-23\n\n- x\n\n## [0.8.0] - 2026-06-10\n\n- y\n\n\
## [0.7.0] - 2026-05-28\n\n- z\n";

        let parsed = parse_release_notes(notes, MAX_RELEASES, MAX_ITEMS_PER_RELEASE);

        assert_eq!(parsed.release_count, 4);
        assert_eq!(parsed.releases.len(), 4);
        assert!(parsed.releases.iter().all(|release| !release.truncated));
        assert_eq!(parsed.releases[0].items.len(), 9);
        assert_eq!(parsed.releases[0].item_count, 9);
    }
}
