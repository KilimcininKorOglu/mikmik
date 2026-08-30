//! Build script for mikmik-tools.
//!
//! Concatenates every `src/output_filter/filters/*.toml` into a single
//! `builtin_filters.toml` under `OUT_DIR`, which `output_filter::toml_filter`
//! embeds with `include_str!`. Splitting the filters into one file per command
//! keeps them reviewable; the concat produces the one document the runtime
//! parses. The combined TOML is validated here so a broken filter fails the
//! build, not a user's session.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

fn main() {
    let filters_dir = Path::new("src/output_filter/filters");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR must be set by Cargo");
    let dest = Path::new(&out_dir).join("builtin_filters.toml");

    // Rebuild when any filter file changes.
    println!("cargo:rerun-if-changed=src/output_filter/filters");

    let mut files: Vec<_> = fs::read_dir(filters_dir)
        .expect("src/output_filter/filters/ directory must exist")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .collect();

    // Sort alphabetically for a deterministic filter order.
    files.sort_by_key(|e| e.file_name());

    let mut combined = String::from("schema_version = 1\n\n");
    for entry in &files {
        let content = fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", entry.path(), e));
        combined.push_str(&format!(
            "# --- {} ---\n",
            entry.file_name().to_string_lossy()
        ));
        combined.push_str(&content);
        combined.push_str("\n\n");
    }

    // Parse the combined TOML to catch syntax errors at build time.
    let parsed: toml::Value = combined.parse().unwrap_or_else(|e| {
        panic!(
            "TOML validation failed for combined filters:\n{}\n\nCheck src/output_filter/filters/*.toml",
            e
        )
    });

    // Reject duplicate filter names across files.
    if let Some(filters) = parsed.get("filters").and_then(|f| f.as_table()) {
        let mut seen: HashSet<String> = HashSet::new();
        for key in filters.keys() {
            if !seen.insert(key.clone()) {
                panic!(
                    "Duplicate filter name '{}' across src/output_filter/filters/*.toml",
                    key
                );
            }
        }
    }

    fs::write(&dest, combined).expect("Failed to write combined builtin_filters.toml");
}
