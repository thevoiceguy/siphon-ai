//! The conformance report: human summary on stdout, JSON on `--report`.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    /// Protocol violations + unmet expectations, in observation order.
    pub failures: Vec<String>,
    /// Non-failure observations (e.g. "server sent hangup — honored").
    pub notes: Vec<String>,
    pub audio_frames_received: u64,
    pub commands_received: u64,
}

impl ScenarioResult {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            duration_ms: 0,
            failures: Vec::new(),
            notes: Vec::new(),
            audio_frames_received: 0,
            commands_received: 0,
        }
    }

    pub fn finalize(mut self) -> Self {
        self.passed = self.failures.is_empty();
        self
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// The candidate server under test.
    pub target: String,
    /// WS bridge protocol version the testkit speaks.
    pub protocol_version: String,
    pub testkit_version: String,
    pub scenarios: Vec<ScenarioResult>,
    pub passed: usize,
    pub failed: usize,
    /// True iff every scenario passed: "conformant with protocol v1"
    /// as a machine-checkable claim.
    pub conformant: bool,
}

impl Report {
    pub fn new(target: &str, scenarios: Vec<ScenarioResult>) -> Self {
        let passed = scenarios.iter().filter(|s| s.passed).count();
        let failed = scenarios.len() - passed;
        Self {
            target: target.to_string(),
            protocol_version: siphon_ai_bridge::PROTOCOL_VERSION.to_string(),
            testkit_version: env!("CARGO_PKG_VERSION").to_string(),
            scenarios,
            passed,
            failed,
            conformant: failed == 0,
        }
    }

    /// The human-readable summary printed to stdout.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for s in &self.scenarios {
            out.push_str(&format!(
                "─── {} ── {} ({} ms, {} audio frames, {} commands)\n",
                s.name,
                if s.passed { "OK" } else { "FAIL" },
                s.duration_ms,
                s.audio_frames_received,
                s.commands_received,
            ));
            for note in &s.notes {
                out.push_str(&format!("      note: {note}\n"));
            }
            for failure in &s.failures {
                out.push_str(&format!("      FAIL: {failure}\n"));
            }
        }
        out.push_str(&format!(
            "\n{}: {} passed, {} failed — target {} is {}conformant with protocol v{}\n",
            if self.conformant { "PASS" } else { "FAIL" },
            self.passed,
            self.failed,
            self.target,
            if self.conformant { "" } else { "NOT " },
            self.protocol_version,
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformant_only_when_all_pass() {
        let ok = ScenarioResult::new("a").finalize();
        let mut bad = ScenarioResult::new("b");
        bad.failures.push("boom".into());
        let bad = bad.finalize();
        assert!(ok.passed);
        assert!(!bad.passed);

        let report = Report::new("ws://x", vec![ok, bad]);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert!(!report.conformant);
        assert!(report.render_text().contains("NOT conformant"));
    }

    #[test]
    fn report_serializes_to_json() {
        let report = Report::new("ws://x", vec![ScenarioResult::new("a").finalize()]);
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"conformant\": true"));
    }
}
