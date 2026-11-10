//! JSON Schema projection: Rust wire DTO → JSON Schema 2020-12, read **straight off the
//! DTO**. This backend never parses generated TypeScript — that is the point of ADR-0008:
//! `cs_backend`'s TypeScript parser is exactly the shape this module refuses to repeat,
//! because a projection that goes through another language's syntax loses information on
//! the way (see `FLOAT_FIELDS` in `cs_backend.rs`).
//!
//! Output is a **preview** tree (`schema/qaqh/<Type>.json` + `schema/index.json`). It is
//! deliberately kept outside `manifest.json` until the sweep covers every exported type;
//! when it joins `generate()`, the golden snapshot must be re-cut through the ADR-0007
//! refreeze path.
//!
//! `surface()` is the ONE list of schema-visible types. Both consumers — `write_preview`
//! (the preview tree) and `ir_diff` (the dual-run against `cs/*.cs`) — read that list, so
//! the diagnostic can never drift from what the projection actually covers. A second
//! hand-copied list is exactly the `FLOAT_FIELDS` failure this module exists to remove.
//!
//! 两条通道是分开的，且这是有意的：`published` 直接 `to_string_pretty(&RootSchema)`，
//! 保持 schemars 的**插入顺序**；`facts` 才转成 `Value` 给 diff 读结构。把预览树也走
//! `Value` 往返会让键被排序（serde_json 未开 `preserve_order`），等于一次"纯诊断"的
//! 改动顺手改掉产物字节 —— 本轮实测过这个陷阱。

use std::path::Path;

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn serialize_settings() -> SchemaSettings {
    SchemaSettings::draft2020_12().for_serialize()
}

/// The view that carries `additionalProperties: false` for `deny_unknown_fields`
/// (ADR-0004 决策 1) — i.e. what a consumer must be able to *accept*.
fn deserialize_settings() -> SchemaSettings {
    SchemaSettings::draft2020_12().for_deserialize()
}

fn root_schema<T: JsonSchema + 'static>(make: fn() -> SchemaSettings) -> impl Serialize + 'static {
    make().into_generator().into_root_schema_for::<T>()
}

fn facts<T: JsonSchema + 'static>(make: fn() -> SchemaSettings) -> Result<Value, String> {
    serde_json::to_value(root_schema::<T>(make))
        .map_err(|error| format!("{}: {error}", T::schema_name()))
}

fn published<T: JsonSchema + 'static>(make: fn() -> SchemaSettings) -> Result<String, String> {
    let mut rendered = serde_json::to_string_pretty(&root_schema::<T>(make))
        .map_err(|error| format!("{}: {error}", T::schema_name()))?;
    rendered.push('\n');
    Ok(rendered)
}

/// One schema-visible contract type.
pub(super) struct SchemaRoot {
    pub name: String,
    /// Serialize view (what the daemon emits) — structural facts for `ir_diff`.
    pub serialize: fn() -> Result<Value, String>,
    /// Deserialize view (what consumers must accept) — structural facts for `ir_diff`.
    pub deserialize: fn() -> Result<Value, String>,
    /// The exact bytes the preview tree publishes for this type (insertion order kept).
    pub published: fn() -> Result<String, String>,
    /// `true` = an inbound body (daemon and consumers *parse* it), so the preview tree
    /// publishes its deserialize view.
    pub inbound: bool,
}

fn root<T: JsonSchema + 'static>(inbound: bool) -> SchemaRoot {
    SchemaRoot {
        name: T::schema_name().into_owned(),
        serialize: || facts::<T>(serialize_settings),
        deserialize: || facts::<T>(deserialize_settings),
        published: if inbound {
            || published::<T>(deserialize_settings)
        } else {
            || published::<T>(serialize_settings)
        },
        inbound,
    }
}

/// Pilot slice, deliberately self-contained: the whole `qaqh-memwatch` surface already
/// listed by `export_types` (unit enums with `rename_all`, `Option<u64>`, `usize`, nested
/// structs, `Vec<_>`) plus the approval body — the JSON-in-JSON boundary ADR-0004 决策 3
/// pulled into the contract, and the only inbound root in this list.
pub(super) fn surface() -> Vec<SchemaRoot> {
    vec![
        root::<qaqh_memwatch::ComponentGroup>(false),
        root::<qaqh_memwatch::ComponentMemory>(false),
        root::<qaqh_memwatch::MemorySnapshot>(false),
        root::<qaqh_memwatch::PhaseKind>(false),
        root::<qaqh_memwatch::PhaseSample>(false),
        root::<qaqh_memwatch::ProcessMemory>(false),
        root::<qaqh_memwatch::SessionMemory>(false),
        root::<qaqh_ringing_contract::PermissionRequestDetails>(true),
    ]
}

/// Write one self-contained schema document: referenced types land in that document's
/// `$defs`, so every file validates standalone. Unlike the TypeScript tree, no cross-file
/// `$ref`/import closure exists to break — `validation.rs`'s import check has no analogue
/// to duplicate here.
fn emit(
    rendered: &str,
    out: &Path,
    name: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let relative = format!("schema/qaqh/{name}.json");
    crate::write(out, &relative, rendered)?;
    Ok((relative, hex::encode(Sha256::digest(rendered.as_bytes()))))
}

pub(super) fn write_preview(out: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut inbound_entries: Vec<(String, String)> = Vec::new();
    for item in surface() {
        let rendered = (item.published)().map_err(|error| format!("schema preview: {error}"))?;
        let written = emit(&rendered, out, &item.name)?;
        if item.inbound {
            inbound_entries.push(written);
        } else {
            entries.push(written);
        }
    }

    entries.sort();
    inbound_entries.sort();
    let files: Vec<_> = entries
        .iter()
        .map(|(path, sha256)| json!({ "path": path, "sha256": sha256 }))
        .collect();
    let inbound_files: Vec<_> = inbound_entries
        .iter()
        .map(|(path, sha256)| json!({ "path": path, "sha256": sha256 }))
        .collect();
    let index = json!({
        "status": "preview: outside manifest.json, not part of the frozen contract artifacts",
        "projection": "Rust DTO -> JSON Schema via schemars; TypeScript is not an input to this backend",
        "schema_draft": "https://json-schema.org/draft/2020-12/schema",
        "contract": "serialize for emitted surfaces, deserialize for inbound JSON-in-JSON bodies",
        "generator_version": crate::GENERATOR_VERSION,
        "pilot_slice": "qaqh-memwatch exported surface + PermissionRequestDetails (approval body)",
        "files": files,
        "inbound_deserialize_files": inbound_files,
    });
    crate::write(
        out,
        "schema/index.json",
        &(serde_json::to_string_pretty(&index)? + "\n"),
    )?;
    Ok(entries.len() + inbound_entries.len())
}
