#!/usr/bin/env python3
"""Aggregate every detector into one ranked consolidation report.

This is the deterministic *Introspect* front-end of a metric-in-the-loop
refactoring workflow (see prompts/README.md): it runs all detectors, merges
their findings into the shared schema, ranks by severity then tool-native score,
and emits either a human Markdown report or a single JSON array - the "data
contract" a human or an LLM consumes so it never has to re-derive (and possibly
hallucinate) the findings.

Advisory; stdlib-only. Nothing here edits source.
"""

from __future__ import annotations

import argparse
import datetime as _dt

import _report
import clones
import complexity
import configscan
import coupling
import modules
import naming
import sqldup
import traits

SECTIONS = ["clones", "sqldup", "configscan", "traits", "modules", "coupling", "complexity", "naming"]

# Cross-tool ranking priority: raw scores are not comparable across tools (a
# complexity of 45 is not "more" than a Jaccard of 1.0), so within a severity we
# surface the consolidation-oriented detectors ahead of the simplification and
# behavioral ones. Consolidation is what this report is for.
TOOL_PRIORITY = {"clones": 5, "sqldup": 5, "configscan": 4, "traits": 4, "coupling": 3, "modules": 3, "naming": 2, "complexity": 1}


def gather(root: str) -> list[_report.Finding]:
    findings: list[_report.Finding] = []
    # Report-tuned thresholds: stronger than each tool's standalone default so the
    # aggregate stays focused on the highest-value consolidation targets.
    findings += clones.collect(root, min_jaccard=0.8)
    findings += sqldup.collect(root, near=0.9)
    findings += configscan.collect(root)
    findings += traits.collect(root)
    findings += modules.collect(root)
    findings += coupling.collect(min_co=6, min_ratio=0.6, root=root)[0]
    findings += complexity.collect(root, min_dp=30)
    findings += naming.collect(root, min_family=3)
    return findings


def _rank(findings: list[_report.Finding]) -> list[_report.Finding]:
    return sorted(
        findings,
        key=lambda f: (_report.SEVERITY_RANK.get(f.severity, 0), TOOL_PRIORITY.get(f.tool, 0), f.score, len(f.locations)),
        reverse=True,
    )


def markdown(findings: list[_report.Finding], root: str, top: int) -> str:
    ts = _dt.datetime.now().strftime("%Y-%m-%d %H:%M")
    by_tool: dict[str, list[_report.Finding]] = {s: [] for s in SECTIONS}
    for f in findings:
        by_tool.setdefault(f.tool, []).append(f)
    sev_count = {s: sum(1 for f in findings if f.severity == s) for s in ("high", "medium", "low", "info")}

    out = [
        f"# Consolidation audit report",
        "",
        f"Generated {ts} over `{root}`. Advisory only - every item is a lead to verify "
        f"by reading the code, not a verdict. See `scripts/audit/README.md`.",
        "",
        f"**Totals:** {len(findings)} findings "
        f"({sev_count['high']} high, {sev_count['medium']} medium, {sev_count['low']} low, {sev_count['info']} info).",
        "",
        "## Top consolidation targets",
        "",
        "Highest-severity findings across all detectors, ranked. Feed one into the "
        "matching brief under `scripts/audit/prompts/` to plan a fix.",
        "",
    ]
    top_findings = [f for f in _rank(findings) if f.severity in ("high", "medium")][:top]
    for f in top_findings:
        loc = f.locations[0] if f.locations else None
        where = f" - `{loc.file}:{loc.line}`" if loc and loc.line else (f" - `{loc.file}`" if loc else "")
        out.append(f"- **[{f.severity}] {f.tool}/{f.kind}** (score {f.score}): {f.summary}{where}")
    if not top_findings:
        out.append("- _None above the medium bar. The mechanical gates are holding._")
    out.append("")

    for tool in SECTIONS:
        items = _rank(by_tool.get(tool, []))
        if not items:
            continue
        out.append(f"## {tool} ({len(items)})")
        out.append("")
        for f in items[:top]:
            loc = f.locations[0] if f.locations else None
            where = f" `{loc.file}:{loc.line}`" if loc and loc.line else ""
            out.append(f"- [{f.severity}] {f.summary}{where}")
        if len(items) > top:
            out.append(f"- _...and {len(items) - top} more (run `{tool}.py` directly)._")
        out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--top", type=int, default=15, help="max items per section (default: 15)")
    ap.add_argument("--json", action="store_true", help="emit all findings as one JSON array (the data contract)")
    args = ap.parse_args()

    findings = gather(args.root)
    if args.json:
        _report.print_json(_rank(findings))
        return 0
    print(markdown(findings, args.root, args.top))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
