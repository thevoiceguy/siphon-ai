//! Generate the machine-readable protocol schema (0.27.0).
//!
//! ```sh
//! cargo run -p siphon-ai-bridge --example gen_schema --features json-schema \
//!     > schemas/siphon-ai.v1.json
//! ```
//!
//! The committed artifact is drift-checked in CI by
//! `scripts/check-protocol-schema.py`, which also validates every
//! `docs/PROTOCOL.md` JSON example against it. The Rust types stay the
//! source of truth (CLAUDE.md §4.2); this example only renders them.

use schemars::generate::SchemaSettings;
use siphon_ai_bridge::{BridgeIn, BridgeOut};

fn main() {
    let mut generator = SchemaSettings::draft2020_12().into_generator();

    // Collect both unions into one shared `$defs` pool.
    let bridge_out = generator.subschema_for::<BridgeOut>();
    let bridge_in = generator.subschema_for::<BridgeIn>();
    let defs = generator.take_definitions(true);

    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://raw.githubusercontent.com/thevoiceguy/siphon-ai/main/schemas/siphon-ai.v1.json",
        "title": "SiphonAI WebSocket protocol v1",
        "description": "Every JSON text frame on a SiphonAI bridge WebSocket \
    is exactly one of these messages, discriminated by `type`. `BridgeOut` = \
    daemon\u{2192}server, `BridgeIn` = server\u{2192}daemon. The canonical prose \
    spec is docs/PROTOCOL.md; this schema is generated from the Rust types in \
    crates/bridge (do not edit by hand).",
        "x-protocol-version": siphon_ai_bridge::PROTOCOL_VERSION,
        "x-ws-subprotocol": "siphon-ai.v1",
        "x-binary-frames": {
            "description": "Audio travels as WS binary frames: raw PCM16 \
    little-endian mono, no header bytes. One frame = exactly 20 ms at the \
    call's negotiated rate.",
            "encoding": "pcm16le",
            "channels": 1,
            "frame_ms": 20,
            "bytes_per_frame": { "8000": 320, "16000": 640 }
        },
        // `anyOf`, deliberately not `oneOf`: three discriminators exist in
        // BOTH directions (`hold`/`resume` — far-end events out, commands
        // in; `mark` — echo out, request in). A frame's direction comes
        // from who sent it, so validate against the matching union
        // ($defs/BridgeOut or $defs/BridgeIn) when the direction is known.
        "anyOf": [
            { "$ref": "#/$defs/BridgeOut" },
            { "$ref": "#/$defs/BridgeIn" }
        ],
        "$defs": {
            "BridgeOut": bridge_out,
            "BridgeIn": bridge_in,
        },
    });

    let mut schema = schema;
    // Merge the collected definitions alongside the two unions.
    let defs_obj = schema["$defs"].as_object_mut().expect("$defs is an object");
    for (name, def) in defs {
        defs_obj.insert(name, def);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("schema serializes")
    );
}
