//! Golden-file roundtrip tests for `jiuyue-protocol`.
//!
//! Every fixture in `tests/golden/` must deserialize into a [`Frame`],
//! re-serialize to a JSON value equal to the original input (key-order
//! insensitive), proving Rust↔JSON agreement in both directions.
//!
//! Fixtures whose file name starts with `unknown_` are forward-compat
//! cases instead: an unrecognized frame type must parse into a typed
//! `error` payload carrying the original type string — never panic,
//! never fail the surrounding stream.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use jiuyue_protocol::{ErrorCode, Frame, Payload};
use serde_json::Value;

const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

/// All M1 wire types that must have a roundtrip fixture.
const ALL_M1_TYPES: [&str; 8] = [
    "auth.ticket.req",
    "auth.ticket.res",
    "msg.send",
    "msg.ack",
    "msg.new",
    "sync.req",
    "sync.res",
    "error",
];

fn golden_files() -> Vec<(String, String)> {
    let dir = Path::new(GOLDEN_DIR);
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("golden dir {dir:?} must be readable: {e}"))
        .map(|entry| entry.expect("golden dir entry readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .expect("file name present")
                .to_string_lossy()
                .into_owned();
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("fixture {name} readable: {e}"));
            (name, contents)
        })
        .collect()
}

#[test]
fn every_known_golden_fixture_roundtrips_through_rust_and_json() {
    let mut seen_types = BTreeSet::new();

    for (name, contents) in golden_files() {
        if name.starts_with("unknown_") {
            continue;
        }
        let input: Value =
            serde_json::from_str(&contents).unwrap_or_else(|e| panic!("{name}: valid JSON: {e}"));
        let frame: Frame = serde_json::from_value(input.clone())
            .unwrap_or_else(|e| panic!("{name}: deserializes into Frame: {e}"));
        let output: Value = serde_json::to_value(&frame)
            .unwrap_or_else(|e| panic!("{name}: serializes back to JSON: {e}"));
        assert_eq!(input, output, "fixture {name} did not roundtrip losslessly");

        let t = input
            .get("t")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{name}: carries string field t"));
        seen_types.insert(t.to_owned());
    }

    let missing: Vec<&str> = ALL_M1_TYPES
        .iter()
        .copied()
        .filter(|t| !seen_types.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "golden fixtures missing for M1 types: {missing:?}"
    );
}

#[test]
fn unknown_frame_type_fixture_parses_into_typed_error_without_panicking() {
    let unknown_fixtures: Vec<_> = golden_files()
        .into_iter()
        .filter(|(name, _)| name.starts_with("unknown_"))
        .collect();
    assert!(
        !unknown_fixtures.is_empty(),
        "at least one unknown_* fixture must exist for forward-compat coverage"
    );

    for (name, contents) in unknown_fixtures {
        let input: Value =
            serde_json::from_str(&contents).unwrap_or_else(|e| panic!("{name}: valid JSON: {e}"));
        let original_t = input["t"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: carries string field t"))
            .to_owned();

        // The whole point: parsing succeeds even though `t` is unrecognized.
        let frame: Frame = serde_json::from_value(input)
            .unwrap_or_else(|e| panic!("{name}: unknown type still parses: {e}"));

        match &frame.payload {
            Payload::Error(payload) => {
                assert_eq!(
                    payload.code,
                    ErrorCode::UnknownType,
                    "{name}: typed unknown_type code"
                );
                assert!(
                    payload.message.contains(&original_t),
                    "{name}: message preserves original type `{original_t}`: {}",
                    payload.message
                );
                assert!(
                    !payload.retryable,
                    "{name}: unknown-type errors are not retryable"
                );
            }
            other => panic!("{name}: expected Error payload, got {other:?}"),
        }

        // Re-serialization is a well-formed v1 envelope with t == "error".
        let reserialized: Value =
            serde_json::to_value(&frame).unwrap_or_else(|e| panic!("{name}: serializes back: {e}"));
        assert_eq!(reserialized["v"], Value::from(1), "{name}: keeps version");
        assert_eq!(
            reserialized["t"],
            Value::from("error"),
            "{name}: becomes error frame"
        );
    }
}
