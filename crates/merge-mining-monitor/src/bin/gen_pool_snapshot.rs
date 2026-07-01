//! `gen-pool-snapshot`: regenerate `data/pools/current.json` from the upstream
//! `bitcoin-data/mining-pools` per-pool files.
//!
//! All field-mapping, slug-remap, ordering, JSON-formatting, and diff
//! logic lives in the pure, unit-tested `pool_snapshot_gen` module; this binary
//! only performs IO: read upstream files, verify the upstream tree is clean,
//! write the output, and print a reviewable churn diff.
//!
//! Usage:
//!   gen-pool-snapshot <upstream-pools-dir> [options]
//!
//! Options:
//!   --generated-at <DATE>   Explicit generated_at value (default: today, UTC).
//!                           Pin this for byte-for-byte reproduction.
//!   --out-dir <DIR>         Output dir for current.json (default: data/pools).
//!   --slug-map <FILE>       Pinned slug map (default: <out-dir>/slug-map.json).
//!   --check                 Do not write; verify the committed current.json's
//!                           POOL CONTENT matches a fresh regeneration and exit
//!                           non-zero if it drifted. `--check` writes nothing.
//!                           Useful for CI drift detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use mmm_capture::pool_resolver::{PoolResolver, PoolSnapshot};
use mmm_capture::pool_snapshot_gen::{
    GeneratedPool, GeneratedSnapshot, SlugMap, UpstreamPool, UpstreamPoolFile, build_snapshot,
    diff_pools, generated_pools_from_snapshot, map_pools, render_snapshot_json, today_utc,
};

struct Args {
    pools_dir: PathBuf,
    generated_at: Option<String>,
    out_dir: PathBuf,
    slug_map: Option<PathBuf>,
    check: bool,
}

fn parse_args() -> Result<Args> {
    let mut iter = std::env::args().skip(1);
    let pools_dir = iter
        .next()
        .context("usage: gen-pool-snapshot <upstream-pools-dir> [options]")?;
    if pools_dir.starts_with("--") {
        bail!("first positional argument must be the upstream pools directory");
    }
    let mut args = Args {
        pools_dir: PathBuf::from(pools_dir),
        generated_at: None,
        out_dir: PathBuf::from("data/pools"),
        slug_map: None,
        check: false,
    };
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--generated-at" => {
                args.generated_at = Some(iter.next().context("--generated-at needs a value")?);
            }
            "--out-dir" => {
                args.out_dir = PathBuf::from(iter.next().context("--out-dir needs a value")?);
            }
            "--slug-map" => {
                args.slug_map = Some(PathBuf::from(
                    iter.next().context("--slug-map needs a value")?,
                ));
            }
            "--check" => args.check = true,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(args)
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("run git {args:?} in {}", repo.display()))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("git output was not valid UTF-8")?
        .trim()
        .to_owned())
}

/// The upstream repo root is the parent of the `pools/` directory.
fn upstream_repo_root(pools_dir: &Path) -> Result<PathBuf> {
    pools_dir
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("cannot determine repo root for {}", pools_dir.display()))
}

/// `--check` drift gate: compare pool CONTENT, not provenance. Render the
/// comparison snapshot with the COMMITTED file's own `generated_at`, so a
/// different run date never false-fails, but any real pool, format, metadata,
/// or ordering drift still shows up byte-for-byte. A missing committed file is
/// itself drift.
fn run_check_gate(
    pools: &[GeneratedPool],
    committed: &Option<PoolSnapshot>,
    current_path: &Path,
) -> Result<()> {
    let Some(committed) = &committed else {
        bail!(
            "current.json is missing at {}; run gen-pool-snapshot to create it",
            current_path.display()
        );
    };
    let snapshot = build_validated_snapshot(pools, &committed.generated_at)?;
    let snapshot_json = render_snapshot_json(&snapshot).map_err(|err| anyhow::anyhow!("{err}"))?;
    let committed_json = fs::read_to_string(current_path)
        .with_context(|| format!("read committed {}", current_path.display()))?;

    let committed_pools = generated_pools_from_snapshot(committed);
    let diff = diff_pools(&committed_pools, pools);
    print_diff(&diff);

    if committed_json != snapshot_json {
        // Distinguish pool-content drift from formatting/provenance-only
        // drift so the message does not send an operator hunting for a
        // non-existent pool change.
        if diff.is_empty() {
            bail!(
                "current.json is out of date: pool CONTENT is identical but the rendered \
                 bytes differ (JSON formatting, key order, or source metadata drifted). \
                 Re-run gen-pool-snapshot to normalize."
            );
        }
        bail!(
            "current.json pool content is out of date with the generator output; re-run \
             gen-pool-snapshot (added={} removed={} changed={}).",
            diff.added.len(),
            diff.removed.len(),
            diff.changed.len()
        );
    }

    eprintln!("current.json is up to date ({} pools)", pools.len());
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let repo = upstream_repo_root(&args.pools_dir)?;

    let git_status_short = git(&repo, &["status", "--short", "--", "pools"])?;
    let working_tree_dirty = !git_status_short.is_empty();

    // The committed repo carries the forward registry contract, not a frozen
    // audit bundle for a dirty input checkout. Regenerate and check from a
    // clean upstream tree so the source data is reviewable at its own origin.
    if working_tree_dirty {
        bail!(
            "upstream working tree is dirty (uncommitted/untracked pool files). Re-run on a \
             clean bitcoin-data/mining-pools checkout.\n\ngit status --short:\n{git_status_short}"
        );
    }

    // Read every pool file currently on disk (tracked or not), in sorted order.
    let mut dir_entries: Vec<PathBuf> = fs::read_dir(&args.pools_dir)
        .with_context(|| format!("read pools dir {}", args.pools_dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    dir_entries.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
    dir_entries.sort();

    let mut files = Vec::new();
    for path in &dir_entries {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("pool filename is not valid UTF-8: {}", path.display()))?
            .to_owned();
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let pool: UpstreamPool = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse upstream pool {}", path.display()))?;
        files.push(UpstreamPoolFile {
            filename_stem: stem,
            pool,
        });
    }

    let slug_map_path = args
        .slug_map
        .clone()
        .unwrap_or_else(|| args.out_dir.join("slug-map.json"));
    let slug_map_json = fs::read_to_string(&slug_map_path)
        .with_context(|| format!("read slug map {}", slug_map_path.display()))?;
    let slug_map =
        SlugMap::from_json_str(&slug_map_json).map_err(|err| anyhow::anyhow!("{err}"))?;

    let pools = map_pools(&files, &slug_map).map_err(|err| anyhow::anyhow!("{err}"))?;

    let current_path = args.out_dir.join("current.json");
    let committed = read_committed_snapshot(&current_path)?;

    if args.check {
        return run_check_gate(&pools, &committed, &current_path);
    }

    let generated_at = args.generated_at.unwrap_or_else(today_utc);
    let snapshot = build_validated_snapshot(&pools, &generated_at)?;
    let snapshot_json = render_snapshot_json(&snapshot).map_err(|err| anyhow::anyhow!("{err}"))?;

    // Diff against the committed snapshot for a reviewable churn report.
    let committed_pools = committed
        .as_ref()
        .map(generated_pools_from_snapshot)
        .unwrap_or_default();
    let diff = diff_pools(&committed_pools, &pools);
    print_diff(&diff);

    fs::write(&current_path, snapshot_json)
        .with_context(|| format!("write {}", current_path.display()))?;

    eprintln!("wrote {} ({} pools)", current_path.display(), pools.len());
    Ok(())
}

/// Build the snapshot and fail fast if it violates the resolver's own
/// invariants (duplicate slugs / coinbase tags / payout addresses). This runs
/// the same `validate_snapshot` the producers run at startup, so a bad regen
/// fails at generation time (write AND --check) instead of crashing every
/// producer (and `reclassify-pools`) later.
fn build_validated_snapshot(
    pools: &[GeneratedPool],
    generated_at: &str,
) -> Result<GeneratedSnapshot> {
    let snapshot = build_snapshot(pools.to_vec(), generated_at);
    let snapshot_json = render_snapshot_json(&snapshot).map_err(|err| anyhow::anyhow!("{err}"))?;
    PoolResolver::from_json_str(&snapshot_json)
        .map_err(|err| anyhow::anyhow!("generated snapshot fails resolver validation: {err}"))?;
    Ok(snapshot)
}

/// Read the committed `current.json` (if any). A missing file yields `None`
/// (everything is "added" for the diff; `--check` treats it as drift).
fn read_committed_snapshot(current_path: &Path) -> Result<Option<PoolSnapshot>> {
    let Ok(json) = fs::read_to_string(current_path) else {
        return Ok(None);
    };
    let snapshot: PoolSnapshot = serde_json::from_str(&json)
        .with_context(|| format!("parse committed {}", current_path.display()))?;
    Ok(Some(snapshot))
}

fn print_diff(diff: &mmm_capture::pool_snapshot_gen::SnapshotDiff) {
    if diff.is_empty() {
        eprintln!("snapshot diff: no changes");
        return;
    }
    eprintln!(
        "snapshot diff: {} added, {} removed, {} changed",
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len()
    );
    for slug in &diff.added {
        eprintln!("  + {slug}");
    }
    for slug in &diff.removed {
        eprintln!("  - {slug} (tombstoned: DB row and FKs survive; stops resolving new captures)");
    }
    for slug in &diff.changed {
        eprintln!("  ~ {slug}");
    }
}
