//! Shared command-line flag walking for the maintenance-command configs.
//!
//! Four `from_args`-style parsers (reconcile-read-model,
//! reclassify-unknown-parents, sync-bitcoin-core, reclassify-pools) shared one
//! hand-rolled Vec/index/match/bail scaffold;
//! this module owns that loop once. Each command keeps its OWN flag
//! semantics, validation, and error strings - the driver only owns the walk
//! and the `unknown <command> argument` bail, and the cursor offers both the
//! standard value/parse helpers and a raw accessor for commands with bespoke
//! error wording. No SQL, no I/O; mmm-producers reaches it through its
//! existing mmm-read-model dependency.

use anyhow::{Context, Result, bail};

/// Position state handed to the per-command flag closure.
pub struct ArgCursor<'a> {
    args: &'a [String],
    idx: usize,
}

impl<'a> ArgCursor<'a> {
    /// Consume and return the flag's value
    /// (standard error text: `{flag} requires a value`).
    pub fn value(&mut self, flag: &str) -> Result<&'a str> {
        self.idx += 1;
        self.args
            .get(self.idx)
            .map(String::as_str)
            .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
    }

    /// Consume and parse the flag's value
    /// (standard error text: `{flag} has invalid value {value:?}`).
    pub fn parse<T>(&mut self, flag: &str) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let value = self.value(flag)?;
        value
            .parse()
            .with_context(|| format!("{flag} has invalid value {value:?}"))
    }

    /// Consume and return the flag's value WITHOUT attaching error context -
    /// for commands whose wording predates the standard helpers and is
    /// pinned by tests (reclassify-pools).
    pub fn raw_value(&mut self) -> Option<&'a str> {
        self.idx += 1;
        self.args.get(self.idx).map(String::as_str)
    }
}

/// Validate that a numeric flag value is strictly positive
/// (standard error text: `{label} must be positive`). Shared by the
/// batch reclassification command parsers such as reclassify-pools.
pub fn require_positive(value: i64, label: &str) -> Result<()> {
    if value <= 0 {
        bail!("{label} must be positive");
    }
    Ok(())
}

/// Walk `args`, calling `on_flag` per flag; the closure returns false for an
/// unrecognized flag, producing the standard
/// `unknown {command} argument {flag:?}` bail.
pub fn drive_args<I, S, F>(command: &str, args: I, mut on_flag: F) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
    F: FnMut(&str, &mut ArgCursor<'_>) -> Result<bool>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let mut cursor = ArgCursor {
        args: &args,
        idx: 0,
    };
    while cursor.idx < args.len() {
        let flag = args[cursor.idx].clone();
        if !on_flag(&flag, &mut cursor)? {
            bail!("unknown {command} argument {flag:?}");
        }
        cursor.idx += 1;
    }
    Ok(())
}
