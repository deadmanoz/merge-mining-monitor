#!/usr/bin/env python3
"""Regenerate data/consensus/btc_nbits_by_epoch.json from a Bitcoin Core node.

Reads the canonical BTC nBits at each 2016-block retarget (DAA) boundary up to
the current tip and writes the embedded nBits-by-epoch table that the
public-RPC producers (Hathor and Elastos) use for OFFLINE contamination verdicts
(rejecting BCH/BSV-family parents that share a BTC ancestor but use an easier
target).

The embedded table is the fast in-range path and the offline-backfill / test
seed. Live producers resolve a beyond-horizon parent's epoch nBits from Bitcoin
Core at runtime (mmm-producers::chains::nbits_horizon), so this script is only
needed to extend the offline / test coverage, NOT to keep live capture advancing.
Stdlib only, no third-party deps.

Usage:
    python3 scripts/gen-nbits-table.py --rpc-url http://127.0.0.1:8332

If the Core endpoint requires HTTP basic auth, pass credentials through your
local endpoint configuration or a throwaway shell variable instead of committing
or documenting real credentials.
"""
from __future__ import annotations

import argparse
import base64
import datetime
import json
import urllib.request
from urllib.parse import urlsplit, urlunsplit

EPOCH = 2016


def split_auth(url: str) -> tuple[str, str | None]:
    """Pull HTTP basic-auth out of an RPC URL into a header."""
    parts = urlsplit(url)
    if not parts.username:
        return url, None
    token = base64.b64encode(
        f"{parts.username}:{parts.password or ''}".encode()
    ).decode()
    netloc = parts.hostname or ""
    if parts.port:
        netloc += f":{parts.port}"
    stripped = urlunsplit((parts.scheme, netloc, parts.path, parts.query, parts.fragment))
    return stripped, token


def rpc(url: str, auth: str | None, method: str, params: list) -> object:
    body = json.dumps(
        {"jsonrpc": "1.0", "id": "gen-nbits", "method": method, "params": params}
    ).encode()
    headers = {"content-type": "application/json"}
    if auth:
        headers["authorization"] = f"Basic {auth}"
    req = urllib.request.Request(url, data=body, headers=headers)
    with urllib.request.urlopen(req, timeout=30) as resp:
        decoded = json.load(resp)
    if decoded.get("error"):
        raise SystemExit(f"RPC {method} error: {decoded['error']}")
    return decoded["result"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument(
        "--rpc-url",
        required=True,
        help="Bitcoin Core RPC URL, for example http://127.0.0.1:8332",
    )
    parser.add_argument("--out", default="data/consensus/btc_nbits_by_epoch.json")
    args = parser.parse_args()

    url, auth = split_auth(args.rpc_url)
    tip = int(rpc(url, auth, "getblockcount", []))
    print(f"BTC tip: {tip:,}")

    epochs: dict[str, str] = {}
    epoch_times: dict[str, int] = {}
    height = 0
    last_header = None
    while height <= tip:
        block_hash = rpc(url, auth, "getblockhash", [height])
        header = rpc(url, auth, "getblockheader", [block_hash, True])
        epochs[str(height)] = header["bits"]
        epoch_times[str(height)] = int(header["time"])
        last_header = header
        height += EPOCH

    covered_max = max(int(k) for k in epochs)
    # The latest covered epoch is partial: weak (timestamp-epoch) classification
    # needs an upper time bound for it, so record the generation tip header's
    # time. Reuse the last boundary header when the tip IS that boundary.
    if tip == covered_max:
        covered_max_time = int(last_header["time"])
    else:
        tip_hash = rpc(url, auth, "getblockhash", [tip])
        covered_max_time = int(rpc(url, auth, "getblockheader", [tip_hash, True])["time"])

    out = {
        "generated_at": datetime.date.today().isoformat(),
        "source": "Bitcoin Core getblockheader at each 2016-block retarget boundary",
        "epoch_interval": EPOCH,
        "covered_min_height": 0,
        "covered_max_height": covered_max,
        "covered_max_time": covered_max_time,
        "nbits_by_epoch": epochs,
        "time_by_epoch": epoch_times,
    }
    with open(args.out, "w") as f:
        json.dump(out, f, indent=1)
    print(
        f"wrote {args.out}: {len(epochs)} epochs, covered 0..{covered_max:,}, "
        f"max time {covered_max_time}"
    )


if __name__ == "__main__":
    main()
