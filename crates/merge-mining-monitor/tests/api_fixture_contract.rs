use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[test]
fn api_fixture_examples_are_listed_and_parse() {
    let dir = fixture_dir();
    let manifest = load_json(&dir.join("manifest.json"));
    assert_eq!(manifest["manifest_schema_version"], "manifest-v1");

    let listed = listed_fixture_files(&manifest);
    let actual = actual_fixture_files(&dir);
    assert_eq!(listed, actual, "manifest must list every API fixture");

    for file in listed {
        let fixture = load_json(&dir.join(&file));
        assert_fixture_envelope(&file, &fixture);
    }
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/api")
}

fn load_json(path: &Path) -> Value {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("failed to parse {} as JSON: {err}", path.display()))
}

fn listed_fixture_files(manifest: &Value) -> BTreeSet<String> {
    manifest["fixtures"]
        .as_array()
        .expect("manifest.fixtures must be an array")
        .iter()
        .map(|row| {
            let file = string_field(row, "file").to_owned();
            assert_ne!(file, "manifest.json");
            assert!(!string_field(row, "endpoint_family").is_empty());
            assert!(!string_field(row, "scenario").is_empty());
            file
        })
        .collect()
}

fn actual_fixture_files(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .map(|entry| {
            entry
                .expect("fixture dir entry")
                .file_name()
                .into_string()
                .expect("fixture filename must be UTF-8")
        })
        .filter(|file| file.ends_with(".json") && file != "manifest.json")
        .collect()
}

fn assert_fixture_envelope(file: &str, fixture: &Value) {
    assert!(
        fixture.is_object(),
        "{file} fixture must be a JSON object, not a scalar or array"
    );
    assert_eq!(
        fixture["schema_version"], "v1",
        "{file} must carry the v1 response envelope"
    );
    assert!(
        fixture["generated_at"].as_u64().is_some(),
        "{file} must carry numeric generated_at"
    );
    if file.starts_with("error-") {
        assert!(fixture["error"].is_object(), "{file} must carry error");
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
}
