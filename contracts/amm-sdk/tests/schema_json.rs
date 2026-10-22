//! Keeps `docs/events.json` in step with the decoder's schema table.
//!
//! `docs/events.json` is generated from `soroban_amm_sdk::events::EVENT_SCHEMAS`.
//! This test fails when the committed file differs from the table. To
//! regenerate it after an intentional change:
//!
//! ```text
//! UPDATE_EVENTS_JSON=1 cargo test -p soroban_amm_sdk --test schema_json
//! ```

use std::collections::HashSet;

use soroban_amm_sdk::events::EVENT_SCHEMAS;

const DOCS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/events.json");

/// Renders the table as a JSON array with one event shape per line. The output
/// is written by hand so the test needs no extra dependency.
fn render() -> String {
    let mut out = String::from("[\n");
    for (i, schema) in EVENT_SCHEMAS.iter().enumerate() {
        let fields: Vec<String> = schema
            .fields
            .iter()
            .map(|(name, ty)| format!("{{\"name\": \"{name}\", \"type\": \"{ty}\"}}"))
            .collect();
        out.push_str(&format!(
            "  {{\"contract\": \"{}\", \"topic\": \"{}\", \"version\": {}, \"fields\": [{}]}}",
            schema.contract,
            schema.topic,
            schema.version,
            fields.join(", ")
        ));
        if i + 1 < EVENT_SCHEMAS.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

#[test]
fn schema_json_matches_docs() {
    let rendered = render();

    if std::env::var_os("UPDATE_EVENTS_JSON").is_some() {
        std::fs::write(DOCS_PATH, &rendered).expect("write docs/events.json");
        return;
    }

    let committed = std::fs::read_to_string(DOCS_PATH).expect("read docs/events.json");
    assert_eq!(
        committed, rendered,
        "docs/events.json is stale; regenerate it with \
         UPDATE_EVENTS_JSON=1 cargo test -p soroban_amm_sdk --test schema_json"
    );
}

#[test]
fn schema_table_has_no_duplicate_shapes() {
    let mut seen = HashSet::new();
    for schema in EVENT_SCHEMAS {
        let key = (schema.contract, schema.topic, schema.fields.len());
        assert!(
            seen.insert(key),
            "duplicate schema for {} / {} with {} fields",
            schema.contract,
            schema.topic,
            schema.fields.len()
        );
    }
}
