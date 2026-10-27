//! Codex apply-patch conformance cases.
//!
//! These are adapted from `codex-rs/apply-patch` (Apache-2.0) parser,
//! streaming-parser, and file-update tests. They exercise QAQH's sync engine
//! through its public surface; security/path-resolution tests stay in the
//! crate's unit suite.

use std::path::Path;

use qaqh_file_tools::apply_patch_engine::{
    Hunk, StreamingPatchParser, apply_patch_engine, parse_patch,
};

const COMPLEX_PATCH: &str = "\
*** Begin Patch
*** Environment ID: env-1
*** Add File: added.txt
+hello
+world
*** Update File: old.txt
*** Move to: renamed.txt
@@
-old
+new
*** Delete File: gone.txt
*** End Patch
";

#[test]
fn parser_accepts_codex_complex_patch_and_environment_id() {
    let parsed = parse_patch(COMPLEX_PATCH).expect("complex patch must parse");
    assert_eq!(parsed.environment_id.as_deref(), Some("env-1"));
    assert_eq!(parsed.hunks.len(), 3);
    assert!(matches!(
        &parsed.hunks[0],
        Hunk::AddFile { path, contents }
            if path == Path::new("added.txt") && contents == "hello\nworld\n"
    ));
    assert!(matches!(
        &parsed.hunks[1],
        Hunk::UpdateFile {
            path,
            move_path: Some(move_path),
            ..
        } if path == Path::new("old.txt") && move_path == Path::new("renamed.txt")
    ));
    assert!(matches!(
        &parsed.hunks[2],
        Hunk::DeleteFile { path } if path == Path::new("gone.txt")
    ));
}

#[test]
fn parser_rejects_codex_invalid_shapes() {
    let cases = [
        // Missing begin marker.
        "*** Add File: a.txt\n+x\n*** End Patch\n",
        // Missing end marker.
        "*** Begin Patch\n*** Add File: a.txt\n+x\n",
        // Empty update hunk.
        "*** Begin Patch\n*** Update File: a.txt\n*** End Patch\n",
        // A marker line cannot be a hunk path.
        "*** Begin Patch\n*** Add File: *** End Patch\n*** End Patch\n",
    ];
    for patch in cases {
        assert!(
            parse_patch(patch).is_err(),
            "Codex must reject this patch:\n{patch}"
        );
    }
    // The parser is lenient about an empty body; the engine is the layer that
    // refuses to apply a no-op patch.
    let empty = parse_patch("*** Begin Patch\n*** End Patch\n").expect("parser is lenient");
    assert!(empty.hunks.is_empty());
}

#[test]
fn streaming_parser_is_chunk_invariant() {
    let mut once = StreamingPatchParser::default();
    once.push_delta(COMPLEX_PATCH).expect("whole patch");
    let once_hunks = once.finish().expect("finish whole patch");
    let once_stats = once.stats();

    let mut split = StreamingPatchParser::default();
    for ch in COMPLEX_PATCH.chars() {
        split
            .push_delta(&ch.to_string())
            .expect("one-character chunk");
    }
    let split_hunks = split.finish().expect("finish split patch");
    let split_stats = split.stats();

    assert_eq!(once_hunks, split_hunks, "hunks must not depend on chunking");
    assert_eq!(once_stats, split_stats, "stats must not depend on chunking");
}

#[test]
fn engine_applies_codex_update_and_preserves_crlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "alpha\r\nbeta\r\ngamma\r\n").expect("fixture");
    let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-beta
+BETA
*** End Patch
";
    let outcome = apply_patch_engine(patch, dir.path()).expect("apply");
    assert_eq!(outcome.affected.modified, vec!["a.txt"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "alpha\r\nBETA\r\ngamma\r\n",
        "Codex preserves the source file's CRLF line endings"
    );
}

#[test]
fn engine_applies_codex_move_and_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("old.txt"), "old\n").expect("fixture");
    std::fs::write(dir.path().join("gone.txt"), "bye\n").expect("fixture");
    let patch = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: renamed.txt
@@
-old
+new
*** Delete File: gone.txt
*** End Patch
";
    let outcome = apply_patch_engine(patch, dir.path()).expect("apply");
    assert!(!dir.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("renamed.txt")).expect("read"),
        "new\n"
    );
    assert!(!dir.path().join("gone.txt").exists());
    assert_eq!(outcome.affected.modified, vec!["renamed.txt"]);
    assert_eq!(outcome.affected.deleted, vec!["gone.txt"]);
}
