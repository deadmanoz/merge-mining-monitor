//! Shared CLI flag walking for maintenance command configs.

use anyhow::{Context, Result, bail};

pub struct ArgCursor<'a> {
    args: &'a [String],
    idx: usize,
}

impl<'a> ArgCursor<'a> {
    pub fn value(&mut self, flag: &str) -> Result<&'a str> {
        self.idx += 1;
        self.args
            .get(self.idx)
            .map(String::as_str)
            .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
    }

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

    pub fn raw_value(&mut self) -> Option<&'a str> {
        self.idx += 1;
        self.args.get(self.idx).map(String::as_str)
    }
}

pub fn require_positive(value: i64, label: &str) -> Result<()> {
    if value <= 0 {
        bail!("{label} must be positive");
    }
    Ok(())
}

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
