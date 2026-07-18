//! The engine's one internal tool-call schema. Provider adapters translate
//! to/from their own dialect (`anthropic-tools`, `openai-json`,
//! `gemini-functions`). Nothing outside `stella-model` should ever construct a
//! provider-native tool-call shape directly.

use serde::{Deserialize, Serialize};

/// A tool schema advertised to the model: name, description, and a JSON
/// Schema for its input. Kept as `serde_json::Value` rather than a typed
/// schema struct so any tool (built-in or MCP-supplied) can describe itself
/// without a second schema language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// True when the tool cannot mutate any state (filesystem, processes,
    /// environment) — the engine may run consecutive read-only calls from
    /// one step concurrently. Defaults to false so unknown/external tools
    /// are treated as mutating, the safe direction.
    #[serde(default)]
    pub read_only: bool,
}

/// One tool invocation the model requested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Stable id correlating this call to its eventual `ToolResult`.
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The output of running a tool — success or a typed, named failure. Never a
/// bare string: every tool result is inspectable without string-sniffing
/// (the isErrorResult() alignment lesson from the TS era).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutput {
    Ok { content: String },
    Error { message: String },
}

impl ToolOutput {
    pub fn is_error(&self) -> bool {
        matches!(self, ToolOutput::Error { .. })
    }
}

/// A tool result reported back to the model, correlated to its call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_roundtrips_ok_and_error() {
        let ok = ToolOutput::Ok {
            content: "hi".into(),
        };
        let err = ToolOutput::Error {
            message: "boom".into(),
        };
        for variant in [ok, err] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ToolOutput = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn is_error_reports_correctly() {
        assert!(
            !ToolOutput::Ok {
                content: String::new()
            }
            .is_error()
        );
        assert!(
            ToolOutput::Error {
                message: String::new()
            }
            .is_error()
        );
    }

    #[test]
    fn read_only_defaults_to_false_the_safe_direction() {
        // A schema serialized before the field existed (or by an external
        // MCP tool that doesn't know about it) must deserialize as mutating.
        let json = r#"{"name":"t","description":"d","input_schema":{}}"#;
        let schema: ToolSchema = serde_json::from_str(json).unwrap();
        assert!(!schema.read_only);
    }

    #[test]
    fn tool_call_roundtrips() {
        let call = ToolCall {
            call_id: "call_1".into(),
            name: "read_file".into(),
            input: serde_json::json!({ "path": "src/main.rs" }),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back, call);
    }
}
