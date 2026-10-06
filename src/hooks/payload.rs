use serde_json::Value;
use std::path::PathBuf;

/// A tool call, normalized from whichever payload dialect the harness sent.
/// `Bash`/`ApplyPatch` carry raw, unparsed text; `decision` extracts path
/// candidates from them. `Edit`/`Write` already carry a single structured
/// path — no further extraction needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    Bash { command: String },
    Edit { path: String },
    Write { path: String },
    ApplyPatch { patch: String },
}

#[derive(Debug, Clone)]
pub struct NormalizedPayload {
    pub tool: Tool,
    /// The harness-reported working directory for this tool call, when the
    /// dialect included one. Relative candidate paths resolve against this;
    /// the caller falls back to its own process cwd when it is absent.
    pub cwd: Option<PathBuf>,
}

/// Normalize one of the two documented payload dialects
/// (`tool_name`/`tool_input` and `toolName`/`toolArgs`) into a
/// [`NormalizedPayload`], or a reason to deny closed: invalid JSON, a
/// non-object root, a missing/non-string tool name, an unsupported tool, or
/// a tool whose required field is missing or not a string.
pub fn normalize(raw: &[u8]) -> Result<NormalizedPayload, String> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| format!("payload is not valid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "payload is not a JSON object".to_string())?;

    let tool_name = object
        .get("tool_name")
        .or_else(|| object.get("toolName"))
        .and_then(Value::as_str)
        .ok_or_else(|| "payload has no tool_name/toolName string".to_string())?;

    let tool_input = object
        .get("tool_input")
        .or_else(|| object.get("toolArgs"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let cwd = object.get("cwd").and_then(Value::as_str).map(PathBuf::from);

    let field = |name: &str| -> Result<String, String> {
        tool_input
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{tool_name} payload has no `{name}` string field"))
    };

    let tool = match tool_name {
        "Bash" => Tool::Bash {
            command: field("command")?,
        },
        "Edit" => Tool::Edit {
            path: field("file_path")?,
        },
        "Write" => Tool::Write {
            path: field("file_path")?,
        },
        "apply_patch" => Tool::ApplyPatch {
            patch: field("command")?,
        },
        other => return Err(format!("unsupported tool: {other}")),
    };

    Ok(NormalizedPayload { tool, cwd })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_snake_case_bash() {
        let payload =
            normalize(br#"{"tool_name":"Bash","tool_input":{"command":"ls"},"cwd":"/tmp"}"#)
                .unwrap();
        assert_eq!(
            payload.tool,
            Tool::Bash {
                command: "ls".to_string()
            }
        );
        assert_eq!(payload.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn accepts_camel_case_edit() {
        let payload =
            normalize(br#"{"toolName":"Edit","toolArgs":{"file_path":"/tmp/x"}}"#).unwrap();
        assert_eq!(
            payload.tool,
            Tool::Edit {
                path: "/tmp/x".to_string()
            }
        );
        assert_eq!(payload.cwd, None);
    }

    #[test]
    fn accepts_write_and_apply_patch() {
        let write =
            normalize(br#"{"tool_name":"Write","tool_input":{"file_path":"/tmp/y"}}"#).unwrap();
        assert_eq!(
            write.tool,
            Tool::Write {
                path: "/tmp/y".to_string()
            }
        );
        let patch = normalize(
            br#"{"tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** End Patch"}}"#,
        )
        .unwrap();
        assert_eq!(
            patch.tool,
            Tool::ApplyPatch {
                patch: "*** Begin Patch\n*** End Patch".to_string()
            }
        );
    }

    #[test]
    fn rejects_invalid_json_a_non_object_root_and_a_missing_tool_name() {
        assert!(normalize(b"not json").is_err());
        assert!(normalize(b"[1,2,3]").is_err());
        assert!(normalize(br#"{"tool_input":{}}"#).is_err());
    }

    #[test]
    fn rejects_an_unsupported_tool_and_a_missing_required_field() {
        assert!(normalize(br#"{"tool_name":"Glob","tool_input":{}}"#).is_err());
        assert!(normalize(br#"{"tool_name":"Bash","tool_input":{}}"#).is_err());
        assert!(normalize(br#"{"tool_name":"Edit","tool_input":{}}"#).is_err());
    }
}
