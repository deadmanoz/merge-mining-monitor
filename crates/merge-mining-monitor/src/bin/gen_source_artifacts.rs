//! Generate (or `--check`) the curated-data artifacts: the baseline
//! source-seed SQL and the frontend chain-metadata ES module (both from
//! `src/source_registry`), and the frontend findings ES module (from
//! `data/findings/` via `src/findings_registry`). Thin IO around
//! `source_registry::generate` and `findings_registry`.
//!
//! Usage:
//!   gen-source-artifacts            write the artifacts
//!   gen-source-artifacts --check    verify the committed artifacts match the
//!                                   curated data (CI drift gate); writes nothing
//!
//! Paths are resolved relative to the current directory, so run from the repo
//! root (the `just gen-source-artifacts` target does).

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use mmm_capture::findings_registry::{
    FINDINGS_DATA_DIR, FINDINGS_JS_PATH, load_findings, render_findings_js,
};
use mmm_capture::source_registry::generate::{
    FRONTEND_JS_PATH, SEED_SQL_PATH, render_frontend_js, render_seed_sql,
};

fn main() -> ExitCode {
    let check = std::env::args().skip(1).any(|a| a == "--check");
    let findings = load_findings(Path::new(FINDINGS_DATA_DIR));
    let artifacts = [
        (SEED_SQL_PATH, render_seed_sql()),
        (FRONTEND_JS_PATH, render_frontend_js()),
        (FINDINGS_JS_PATH, render_findings_js(&findings)),
    ];

    let mut drift = false;
    for (path, content) in &artifacts {
        if check {
            match fs::read_to_string(path) {
                Ok(on_disk) if &on_disk == content => println!("ok: {path}"),
                Ok(_) => {
                    eprintln!(
                        "DRIFT: {path} does not match the curated data. Run `just gen-source-artifacts`."
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
