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

use std::path::Path;

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::json;
use sha2::{Digest, Sha256};

/// Emit one self-contained schema document per type: referenced types land in that
/// document's `$defs`, so every file validates standalone. Unlike the TypeScript tree,
/// no cross-file `$ref`/import closure exists to break — `validation.rs`'s import check
/// has no analogue to duplicate here.
fn emit<T: JsonSchema>(
    settings: &SchemaSettings,
    out: &Path,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let root = settings.clone().into_generator().into_root_schema_for::<T>();
    let mut rendered = serde_json::to_string_pretty(&root)?;
    rendered.push('\n');
    let relative = format!("schema/qaqh/{}.json", T::schema_name());
    crate::write(out, &relative, &rendered)?;
    Ok((relative, hex::encode(Sha256::digest(&rendered))))
}

/// Pilot slice: the whole `qaqh-memwatch` surface already listed by `export_types`.
/// Chosen because its type closure is self-contained — it exercises the projection
/// (unit enums with `rename_all`, `Option<u64>`, `usize`, nested structs, `Vec<_>`)
/// without dragging in the RingingV2 command directory.
pub(super) fn write_preview(out: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    // Contract = Serialize: what the daemon *emits* is the shape a pinned frontend must
    // tolerate, and it is exactly the shape `fixtures/` are serialized into (ADR-0005).
    // The deserialize view is one settings switch away and is the one that carries
    // `additionalProperties: false` for `deny_unknown_fields` (ADR-0004) — which is why
    // an *inbound* body is projected with that switch instead of a hand-written table.
    let serialize = SchemaSettings::draft2020_12().for_serialize();
    let deserialize = SchemaSettings::draft2020_12().for_deserialize();

    let mut entries: Vec<(String, String)> = Vec::new();
    macro_rules! preview_all {
        ($settings:expr, $($ty:ty),+ $(,)?) => { $( entries.push(emit::<$ty>(&$settings, out)?); )+ };
    }
    preview_all!(
        serialize,
        qaqh_memwatch::ComponentGroup,
        qaqh_memwatch::ComponentMemory,
        qaqh_memwatch::MemorySnapshot,
        qaqh_memwatch::PhaseKind,
        qaqh_memwatch::PhaseSample,
        qaqh_memwatch::ProcessMemory,
        qaqh_memwatch::SessionMemory,
    );

    // ADR-0004 §决策 3 的 JSON-in-JSON 边界：审批正文由 daemon 与每个读方**反序列化**，
    // 所以它的投影取 deserialize 视图 —— `deny_unknown_fields` 在这里派生出
    // `additionalProperties: false`，compat 类不必手抄。
    let inbound: Vec<(String, String)> = vec![emit::<
        qaqh_ringing_contract::PermissionRequestDetails,
    >(&deserialize, out)?];

    entries.sort();
    let files: Vec<_> = entries
        .iter()
        .map(|(path, sha256)| json!({ "path": path, "sha256": sha256 }))
        .collect();
    let inbound_files: Vec<_> = inbound
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
    Ok(entries.len() + inbound.len())
}
