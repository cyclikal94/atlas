//! Exports the API contract version so `/health` can report exactly the version of the
//! `api/openapi.json` this binary was built with. There is no second copy to drift.
use std::{env, fs, path::PathBuf};

/// Strict `MAJOR.MINOR.PATCH`: numeric parts, no prefix, prerelease, metadata or leading zeros.
/// Keep in step with the `pattern` on the `/health` response schema and `scripts/validate_contract.py`.
fn is_api_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (*part == "0" || !part.starts_with('0'))
        })
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let contract = manifest.join("../../api/openapi.json");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", contract.display());
    let text = fs::read_to_string(&contract)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", contract.display()));
    let document: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", contract.display()));
    let version = document["info"]["version"].as_str().unwrap_or_else(|| {
        panic!(
            "{}: info.version must be a string of the form MAJOR.MINOR.PATCH",
            contract.display()
        )
    });
    assert!(
        is_api_version(version),
        "{}: info.version {version:?} must be MAJOR.MINOR.PATCH with numeric parts and no leading zeros",
        contract.display()
    );
    println!("cargo:rustc-env=ATLAS_API_VERSION={version}");
}
