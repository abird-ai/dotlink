//! Shared-engine conformance gates for the AW/dotlink cutover.
//!
//! Native filesystem production dispatch already executes `aw-tools` directly.
//! These tests therefore verify the actual host projection, not the retired
//! legacy implementations. Shell Jobs already bind the canonical shared
//! JobManager; dotlink retains only transport delivery/replay, process custody,
//! wall-clock retention and MCP projection. Multi-node model-facing Graph
//! construction/restart is the remaining cutover boundary.
#![cfg(test)]

use std::path::Path;

use aw_tools::filesystem::{request, schema::Tool as CoreTool};

use super::shared_native;
use crate::mcp::{Capabilities, LocalMachine};

fn semantic_schema(mut value: serde_json::Value) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                object.remove("description");
                for child in object.values_mut() {
                    strip(child);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    strip(child);
                }
            }
            _ => {}
        }
    }
    strip(&mut value);
    value
}

fn limits(caps: aw_tools::filesystem::schema::SchemaCaps) -> request::Limits {
    request::Limits {
        schema: caps,
        edit_text_bytes: caps.text_write_bytes,
        // Dotlink's raw decoded write ceiling is no larger than the encoded
        // schema ceiling used by these small canonical vectors.
        binary_decoded_bytes: caps.text_write_bytes,
    }
}

#[test]
fn shared_native_cutover_matches_linked_conformance() {
    let caps = shared_native::caps(
        crate::limits::DEFAULT_TRANSFER_BYTES,
        crate::limits::DEFAULT_TRANSFER_BYTES,
    )
    .expect("built-in dotlink transfer ceilings fit the shared engine");
    let request_limits = limits(caps);

    // First prove this exact linked aw-tools revision is internally coherent
    // under the same host-selected ceilings dotlink will advertise.
    aw_tools::conformance::validate_with_limits(request_limits)
        .expect("linked aw-tools conformance is internally coherent");

    // Then prove dotlink's actual advertised router is the same canonical
    // projection after normal policy filtering. Read+write capability exposes
    // the complete native filesystem surface; shell/Job tools are orthogonal.
    let router = LocalMachine::tool_router_for_policy_with_caps(
        Capabilities {
            read: true,
            write: true,
            read_write: true,
            shell: false,
        },
        caps,
        None,
    );
    let fixture = aw_tools::conformance::fixture_with_caps(caps);
    for vector in &fixture.schemas {
        let route = router
            .get(&vector.tool)
            .unwrap_or_else(|| panic!("dotlink did not advertise {}", vector.tool));
        assert_eq!(
            semantic_schema(serde_json::Value::Object(
                route.input_schema.as_ref().clone(),
            )),
            semantic_schema(vector.parameters.clone()),
            "{} semantic schema drifted from linked aw-tools",
            vector.tool
        );
    }

    // Every canonical request must be reachable through the exact same parser
    // and dotlink profile ceilings used by production shared-native dispatch.
    for vector in &fixture.requests {
        let tool = CoreTool::from_name(&vector.tool)
            .unwrap_or_else(|| panic!("fixture names unknown tool {}", vector.tool));
        request::parse(tool, &vector.arguments, request_limits)
            .unwrap_or_else(|error| panic!("{} rejected by dotlink limits: {error}", vector.id));
    }
}

fn load_reviewed(path: &Path) -> aw_tools::conformance::Fixture {
    let bytes = std::fs::read(path).expect("read reviewed aw-tools conformance fixture");
    let fixture: aw_tools::conformance::Fixture =
        serde_json::from_slice(&bytes).expect("decode reviewed aw-tools conformance fixture");
    assert_eq!(
        fixture.version,
        aw_tools::conformance::FORMAT_VERSION,
        "reviewed fixture format differs from linked aw-tools"
    );
    fixture
}

#[test]
#[ignore = "requires DOTLINK_SHARED_CONFORMANCE_JSON emitted by the reviewed AW revision"]
fn reviewed_aw_fixture_matches_linked_shared_revision() {
    let path = std::env::var_os("DOTLINK_SHARED_CONFORMANCE_JSON")
        .map(std::path::PathBuf::from)
        .expect("DOTLINK_SHARED_CONFORMANCE_JSON must name the reviewed AW fixture JSON");
    let reviewed = load_reviewed(&path);
    assert_eq!(
        reviewed,
        aw_tools::conformance::fixture(),
        "dotlink is linked against a different aw-tools semantic fixture than the reviewed AW revision"
    );
}
