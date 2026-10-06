use crate::hooks::{Protocol, Verdict};
use serde_json::{json, Value};

/// Render one `Verdict` as the exact JSON object the requested protocol's
/// harness expects, always exactly one object: the Claude/Copilot composite
/// carries both Copilot's top-level `permissionDecision`/
/// `permissionDecisionReason` and Claude's nested `hookSpecificOutput`, so
/// either harness reads its own fields and ignores the other's; Codex reads
/// only the nested `hookSpecificOutput` form, mirroring the same field
/// names. An allow verdict is deliberately minimal on both protocols: an
/// empty object asserts no denial, letting the harness's own default-allow
/// behavior take the tool call through, rather than risking an invented
/// "explicit allow" shape neither harness's hook documentation was measured
/// against.
pub fn render(protocol: Protocol, verdict: &Verdict) -> Value {
    match (protocol, verdict) {
        (Protocol::ClaudeCompatible, Verdict::Allow) => json!({}),
        (Protocol::Codex, Verdict::Allow) => json!({}),
        (Protocol::ClaudeCompatible, Verdict::Deny(reason)) => json!({
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        (Protocol::Codex, Verdict::Deny(reason)) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_compatible_deny_carries_both_dialects_composite() {
        let body = render(
            Protocol::ClaudeCompatible,
            &Verdict::Deny("nope".to_string()),
        );
        assert_eq!(body["permissionDecision"], "deny");
        assert_eq!(body["permissionDecisionReason"], "nope");
        assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            body["hookSpecificOutput"]["permissionDecisionReason"],
            "nope"
        );
    }

    #[test]
    fn codex_deny_is_nested_only() {
        let body = render(Protocol::Codex, &Verdict::Deny("nope".to_string()));
        assert!(body.get("permissionDecision").is_none());
        assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            body["hookSpecificOutput"]["permissionDecisionReason"],
            "nope"
        );
    }

    #[test]
    fn allow_is_an_empty_object_on_both_protocols() {
        assert_eq!(
            render(Protocol::ClaudeCompatible, &Verdict::Allow),
            json!({})
        );
        assert_eq!(render(Protocol::Codex, &Verdict::Allow), json!({}));
    }
}
