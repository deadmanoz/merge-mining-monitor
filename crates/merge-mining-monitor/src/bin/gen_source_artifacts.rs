//! Generate (or `--check`) the registry-derived artifacts from
//! `src/source_registry`: the baseline source-seed SQL and the frontend
//! chain-metadata ES module. Thin IO around `source_registry::generate`.
//!
//! Usage:
//!   gen-source-artifacts            write the artifacts
//!   gen-source-artifacts --check    verify the committed artifacts match the
//!                                   registry (CI drift gate); writes nothing
//!
//! Paths are resolved relative to the current directory, so run from the repo
//! root (the `just gen-source-artifacts` target does).

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use mmm_capture::source_registry::generate::{
    FRONTEND_JS_PATH, SEED_SQL_PATH, render_frontend_js, render_seed_sql,
};

fn main() -> ExitCode {
    let check = std::env::args().skip(1).any(|a| a == "--check");
    let artifacts = [
        (SEED_SQL_PATH, render_seed_sql()),
        (FRONTEND_JS_PATH, render_frontend_js()),
    ];

    let mut drift = false;
    for (path, content) in &artifacts {
        if check {
            match fs::read_to_string(path) {
                Ok(on_disk) if &on_disk == content => println!("ok: {path}"),
                Ok(_) => {
                    eprintln!(
                        "DRIFT: {path} does not match the source registry. Run `just gen-source-artifacts`."
                    );
                    drift = true;
                }
                Err(err) => {
                    eprintln!(
                        "DRIFT: {path} is unreadable ({err}). Run `just gen-source-artifacts`."
                    );
                    drift = true;
                }
            }
        } else {
            if let Some(parent) = Path::new(path).parent()
                && let Err(err) = fs::create_dir_all(parent)
            {
                eprintln!("create_dir_all {} failed: {err}", parent.display());
                return ExitCode::FAILURE;
            }
            if let Err(err) = fs::write(path, content) {
                eprintln!("write {path} failed: {err}");
                return ExitCode::FAILURE;
            }
            println!("wrote {path}");
        }
    }

    if drift {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
