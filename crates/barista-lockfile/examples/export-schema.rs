// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Export the JSON Schema for `barista.lock` to stdout.
//!
//! Run:
//!
//! ```text
//! cargo run -p barista-lockfile --release --example export-schema \
//!     > schema/lockfile/v1.json
//! ```
//!
//! The generated schema is a Draft 2020-12 JSON Schema describing the
//! TOML shape of a `barista.lock` file (the TOML and JSON serde forms
//! share the same struct, so the schema applies to either encoding's
//! payload). External tools — IDE validation, lint rules, schema-aware
//! editors — can consume it directly.
//!
//! The schema's `$id` points at the canonical public URL
//! (`https://barista.build/schema/lockfile/v1.json`), which is also
//! where the file is published.

use barista_lockfile::Lockfile;
use schemars::schema_for;
use serde_json::{Map, Value};

fn main() {
    // Generate the raw schema from the typed `Lockfile` root.
    let schema = schema_for!(Lockfile);
    let mut value: Value = serde_json::to_value(&schema).expect("schema is serializable");

    // Inject identifying metadata. `schemars` already sets `$schema`
    // and `title`; we override the title and add `$id` + a short
    // human-readable description so the file is self-explanatory.
    if let Value::Object(obj) = &mut value {
        // Put $id and $schema at the top of the document for
        // readability. Use a fresh map and re-insert keys in the
        // desired order.
        let mut ordered: Map<String, Value> = Map::new();
        ordered.insert(
            "$schema".to_string(),
            Value::String("https://json-schema.org/draft/2020-12/schema".to_string()),
        );
        ordered.insert(
            "$id".to_string(),
            Value::String("https://barista.build/schema/lockfile/v1.json".to_string()),
        );
        ordered.insert(
            "title".to_string(),
            Value::String("barista.lock (v1)".to_string()),
        );
        ordered.insert(
            "description".to_string(),
            Value::String(
                "JSON Schema for the `barista.lock` file format. The lockfile is \
                 serialized as TOML on disk; this schema describes the equivalent \
                 JSON shape and is suitable for IDE validation and external tooling."
                    .to_string(),
            ),
        );

        // Carry over every other field from the generated schema.
        for (k, v) in obj.iter() {
            if k == "$schema" || k == "$id" || k == "title" || k == "description" {
                continue;
            }
            ordered.insert(k.clone(), v.clone());
        }

        *obj = ordered;
    }

    let pretty = serde_json::to_string_pretty(&value).expect("schema is JSON-serializable");
    println!("{pretty}");
}
