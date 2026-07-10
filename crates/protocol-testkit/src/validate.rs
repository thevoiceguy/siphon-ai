//! Schema + typed validation of server→daemon messages.
//!
//! Two independent legs, both must pass (DESIGN_PROTOCOL_SDKS §4):
//! 1. JSON Schema — the message validates against `$defs/BridgeIn` in the
//!    committed `schemas/siphon-ai.v1.json` (embedded at compile time, so
//!    the binary is self-contained for third parties).
//! 2. Typed parse — the message deserializes into the daemon's real
//!    [`BridgeIn`] type, exactly as the daemon itself would parse it.

use anyhow::{Context, Result};
use jsonschema::Validator;
use serde_json::Value;
use siphon_ai_bridge::BridgeIn;

/// The protocol schema the daemon's CI regenerates and diffs on every PR.
pub const SCHEMA_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/siphon-ai.v1.json"
));

pub struct MessageValidator {
    bridge_in: Validator,
}

impl MessageValidator {
    pub fn new() -> Result<Self> {
        let schema: Value =
            serde_json::from_str(SCHEMA_JSON).context("embedded protocol schema is not JSON")?;
        let defs = schema
            .get("$defs")
            .context("embedded protocol schema has no $defs")?
            .clone();
        let wrapper = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/BridgeIn",
            "$defs": defs,
        });
        let bridge_in = jsonschema::validator_for(&wrapper)
            .context("embedded protocol schema failed to compile")?;
        Ok(Self { bridge_in })
    }

    /// Validate one text frame from the candidate server. Returns the
    /// parsed [`BridgeIn`] command on success; a human-readable violation
    /// description on failure.
    pub fn check(&self, text: &str) -> std::result::Result<BridgeIn, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| format!("not valid JSON: {e} — frame: {}", clip(text)))?;
        let schema_errors: Vec<String> = self
            .bridge_in
            .iter_errors(&value)
            .map(|e| format!("{} (at {})", e, e.instance_path()))
            .collect();
        if !schema_errors.is_empty() {
            let ty = value.get("type").and_then(Value::as_str).unwrap_or("?");
            return Err(format!(
                "`{ty}` fails schema $defs/BridgeIn: {} — frame: {}",
                schema_errors.join("; "),
                clip(text),
            ));
        }
        serde_json::from_value::<BridgeIn>(value).map_err(|e| {
            format!(
                "schema-valid but not parseable as BridgeIn: {e} — frame: {}",
                clip(text)
            )
        })
    }
}

fn clip(text: &str) -> String {
    const MAX: usize = 200;
    if text.len() <= MAX {
        text.to_string()
    } else {
        let mut end = MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_commands_pass_both_legs() {
        let v = MessageValidator::new().expect("schema compiles");
        for cmd in [
            r#"{"type":"hangup","call_id":"c1","cause":"normal"}"#,
            r#"{"type":"mark","call_id":"c1","name":"greeting_done"}"#,
            r#"{"type":"clear","call_id":"c1"}"#,
            r#"{"type":"send_dtmf","call_id":"c1","digit":"5","duration_ms":160}"#,
        ] {
            v.check(cmd)
                .unwrap_or_else(|e| panic!("{cmd} should pass: {e}"));
        }
    }

    #[test]
    fn unknown_type_fails() {
        let v = MessageValidator::new().unwrap();
        assert!(v.check(r#"{"type":"yodel","call_id":"c1"}"#).is_err());
    }

    #[test]
    fn wrong_field_shape_fails() {
        let v = MessageValidator::new().unwrap();
        // digit must be a single char
        assert!(v
            .check(r#"{"type":"send_dtmf","call_id":"c1","digit":"55","duration_ms":160}"#)
            .is_err());
        assert!(v.check("{nope").is_err());
    }
}
