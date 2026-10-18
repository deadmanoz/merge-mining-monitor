//! Generate (or `--check` for CI drift) the curated-data artifacts: baseline
//! source-seed SQL, frontend chain-metadata ES module, frontend findings ES
//! module, and the `/api/v1/version` golden fixture. Run from the repo root
//! (`just gen-source-artifacts`).

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use mmm_capture::findings_registry::{
    FINDINGS_DATA_DIR, FINDINGS_JS_PATH, load_findings, render_findings_js,
};
use mmm_capture::source_registry::generate::{
    FRONTEND_JS_PATH, SEED_SQL_PATH, render_frontend_js, render_seed_sql,
};
const VERSION_FIXTURE_PATH: &str = "fixtures/api/version.json";
const DEFAULT_VERSION_FIXTURE_GENERATED_AT: u64 = 1_792_333_752;

fn render_version_fixture() -> String {
    let generated_at = fs::read_to_string(VERSION_FIXTURE_PATH)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|prev| prev.get("generated_at").and_then(serde_json::Value::as_u64))
        .unwrap_or(DEFAULT_VERSION_FIXTURE_GENERATED_AT);
    mmm_api::render_version_fixture_json(generated_at)
}

fn main() -> ExitCode {
    let check = std::env::args().skip(1).any(|a| a == "--check");
    let findings = load_findings(Path::new(FINDINGS_DATA_DIR));
    let artifacts = [
        (SEED_SQL_PATH, render_seed_sql()),
        (FRONTEND_JS_PATH, render_frontend_js()),
        (FINDINGS_JS_PATH, render_findings_js(&findings)),
        (VERSION_FIXTURE_PATH, render_version_fixture()),
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
