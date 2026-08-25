"""Shared finding schema + JSON emission for the audit tools.

Every detector's `collect()` returns a list of `Finding`s in one uniform shape,
so `report.py` can aggregate them and any `--json` path emits the same
machine-readable "data contract" an LLM refactoring loop (or a human) consumes.
Keeping the schema in one place is the point: a stable contract is what stops a
downstream agent from re-deriving (and hallucinating) the findings.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field

# Severity ordering for ranking (higher = more worth acting on first).
SEVERITY_RANK = {"info": 0, "low": 1, "medium": 2, "high": 3}


@dataclass(frozen=True)
class Loc:
    file: str
    line: int = 0
    name: str = ""


@dataclass
class Finding:
    tool: str  # which detector produced it (clones, sqldup, configscan, ...)
    kind: str  # machine tag, e.g. "structural-clone", "sql-exact-dup"
    summary: str  # one-line human description
    score: float = 0.0  # tool-native strength (Jaccard, ratio, count); 0 if n/a
    severity: str = "info"  # info | low | medium | high
    locations: list[Loc] = field(default_factory=list)
    metrics: dict = field(default_factory=dict)  # tool-specific extras

    def sort_key(self):
        return (SEVERITY_RANK.get(self.severity, 0), self.score, len(self.locations))


def as_dicts(findings: list[Finding]) -> list[dict]:
    return [asdict(f) for f in findings]


def print_json(findings: list[Finding]) -> None:
    print(json.dumps(as_dicts(findings), indent=2))
