use serde_json::{Map, Value};

/// Exact marker every canonical group carries, used both to build the group
/// and to recognize (and only ever touch) prior groups this tool wrote.
pub const CLAUDE_COMPATIBLE_MARKER_KEY: &str = "description";
pub const CLAUDE_COMPATIBLE_MARKER_VALUE: &str =
    "git-grove: protect grove metadata (managed by git grove setup)";
pub const CODEX_MARKER_KEY: &str = "_id";
pub const CODEX_MARKER_VALUE: &str = "git-grove.protect-metadata.v1";

/// The canonical Claude/Copilot `PreToolUse` hook group. `executable` is the
/// canonicalized absolute path to the current `git-grove` binary.
pub fn claude_compatible_group(executable: &str) -> Value {
    serde_json::json!({
        CLAUDE_COMPATIBLE_MARKER_KEY: CLAUDE_COMPATIBLE_MARKER_VALUE,
        "matcher": "Bash|Edit|Write",
        "hooks": [{
            "type": "command",
            "command": format!("{executable} hook-guard --protocol claude-compatible PreToolUse"),
            "timeout": 15
        }]
    })
}

/// The canonical Codex `PreToolUse` hook group. `executable` is the
/// canonicalized absolute path to the current `git-grove` binary.
pub fn codex_group(executable: &str) -> Value {
    serde_json::json!({
        CODEX_MARKER_KEY: CODEX_MARKER_VALUE,
        "matcher": "Bash|Edit|Write|apply_patch",
        "hooks": [{
            "type": "command",
            "command": format!("{executable} hook-guard --protocol codex PreToolUse"),
            "timeout": 15
        }]
    })
}

/// Merge `group` into `existing` under `hooks.PreToolUse`, touching nothing
/// else: parses `existing` as a JSON object (or starts from `{}` when
/// `existing` is empty), finds every array entry whose `marker_key` field is
/// exactly `marker_value`, replaces the first with `group` and drops any
/// duplicates, or appends `group` when none matched. Every unrelated
/// top-level value, event, group, and handler — and the array's ordering
/// for everything else — is preserved exactly; this never touches `hooks`
/// keys other than `PreToolUse`, and never rewrites the whole file.
///
/// Refuses (returning the path/type problem, producing no bytes to write)
/// when `existing` is not empty and not valid JSON, when its root or its
/// `hooks` value is present but not an object, or when `hooks.PreToolUse`
/// is present but not an array. A caller that only ever wrote through this
/// function cannot construct any of those shapes; they exist to protect a
/// file this tool does not own alone.
pub fn merge(
    existing: &[u8],
    marker_key: &str,
    marker_value: &str,
    group: Value,
) -> Result<Value, String> {
    let mut root: Map<String, Value> = if existing.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice(existing) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err("root is not a JSON object".to_string()),
            Err(error) => return Err(format!("not valid JSON: {error}")),
        }
    };

    let hooks_value = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks_map) = hooks_value else {
        return Err("`hooks` is not a JSON object".to_string());
    };

    let pre_tool_use = hooks_map
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(array) = pre_tool_use else {
        return Err("`hooks.PreToolUse` is not a JSON array".to_string());
    };

    let is_owned = |entry: &Value| {
        entry
            .as_object()
            .and_then(|object| object.get(marker_key))
            .and_then(Value::as_str)
            == Some(marker_value)
    };

    let mut replaced = false;
    let mut merged = Vec::with_capacity(array.len() + 1);
    for entry in array.drain(..) {
        if is_owned(&entry) {
            if !replaced {
                merged.push(group.clone());
                replaced = true;
            }
        } else {
            merged.push(entry);
        }
    }
    if !replaced {
        merged.push(group);
    }
    *array = merged;

    Ok(Value::Object(root))
}

/// Render a merged config as pretty-printed JSON with a trailing newline —
/// the canonical on-disk form every write and every idempotency check in
/// this module compares against.
pub fn render(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("a merged Value always serializes");
    bytes.push(b'\n');
    bytes
}

/// Idempotently add `entry` as its own line to a `.gitignore`-style exclude
/// file's bytes, preserving every unrelated byte and the file's own
/// line-ending convention (CRLF if any `\r\n` is already present, LF
/// otherwise). Returns `existing` unchanged, byte-for-byte, if `entry`
/// already appears as an exact line — repeated runs, and a second agent's
/// run after a first, converge rather than growing the file. Adds a
/// trailing line ending to `existing` first if it is missing one, since
/// appending directly onto an unterminated last line would corrupt it —
/// that one addition is necessary, not "more than needed".
pub fn add_exclude_entry(existing: &[u8], entry: &str) -> Vec<u8> {
    let line_ending: &[u8] = if existing.windows(2).any(|window| window == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    let text = String::from_utf8_lossy(existing);
    let already_present = text
        .split(['\n'])
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .any(|line| line == entry);
    if already_present {
        return existing.to_vec();
    }

    let mut updated = existing.to_vec();
    if !updated.is_empty() && !updated.ends_with(line_ending) {
        updated.extend_from_slice(line_ending);
    }
    updated.extend_from_slice(entry.as_bytes());
    updated.extend_from_slice(line_ending);
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_the_group_to_an_absent_file() {
        let group = claude_compatible_group("/bin/git-grove");
        let merged = merge(
            b"",
            CLAUDE_COMPATIBLE_MARKER_KEY,
            CLAUDE_COMPATIBLE_MARKER_VALUE,
            group.clone(),
        )
        .unwrap();
        assert_eq!(merged["hooks"]["PreToolUse"], json!([group]));
    }

    #[test]
    fn replaces_the_first_owned_entry_and_drops_duplicates_in_place() {
        let stale = claude_compatible_group("/old/git-grove");
        let existing = json!({
            "hooks": {"PreToolUse": [{"description": "unrelated"}, stale.clone(), stale.clone()]}
        });
        let fresh = claude_compatible_group("/new/git-grove");
        let merged = merge(
            &serde_json::to_vec(&existing).unwrap(),
            CLAUDE_COMPATIBLE_MARKER_KEY,
            CLAUDE_COMPATIBLE_MARKER_VALUE,
            fresh.clone(),
        )
        .unwrap();
        assert_eq!(
            merged["hooks"]["PreToolUse"],
            json!([{"description": "unrelated"}, fresh])
        );
    }

    #[test]
    fn preserves_unrelated_top_level_and_event_content() {
        let existing = json!({
            "unrelatedTopLevel": true,
            "hooks": {
                "SessionStart": [{"unrelated": "handler"}],
                "PreToolUse": [{"unrelated": "entry"}]
            }
        });
        let merged = merge(
            &serde_json::to_vec(&existing).unwrap(),
            CODEX_MARKER_KEY,
            CODEX_MARKER_VALUE,
            codex_group("/bin/git-grove"),
        )
        .unwrap();
        assert_eq!(merged["unrelatedTopLevel"], json!(true));
        assert_eq!(
            merged["hooks"]["SessionStart"],
            json!([{"unrelated": "handler"}])
        );
        assert_eq!(
            merged["hooks"]["PreToolUse"][0],
            json!({"unrelated": "entry"})
        );
    }

    #[test]
    fn refuses_malformed_json_a_non_object_root_and_a_non_array_pre_tool_use() {
        assert!(merge(b"not json", "k", "v", json!({})).is_err());
        assert!(merge(b"[1,2,3]", "k", "v", json!({})).is_err());
        assert!(merge(br#"{"hooks":{"PreToolUse":"nope"}}"#, "k", "v", json!({})).is_err());
        assert!(merge(br#"{"hooks":"nope"}"#, "k", "v", json!({})).is_err());
    }

    #[test]
    fn a_similar_but_unmarked_entry_is_left_alone() {
        let existing = json!({"hooks": {"PreToolUse": [{"description": "someone else's hook"}]}});
        let merged = merge(
            &serde_json::to_vec(&existing).unwrap(),
            CLAUDE_COMPATIBLE_MARKER_KEY,
            CLAUDE_COMPATIBLE_MARKER_VALUE,
            claude_compatible_group("/bin/git-grove"),
        )
        .unwrap();
        assert_eq!(
            merged["hooks"]["PreToolUse"][0],
            json!({"description": "someone else's hook"})
        );
        assert_eq!(merged["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn exclude_entry_is_added_once_and_repeated_calls_converge() {
        let once = add_exclude_entry(b"", "/.claude/settings.local.json");
        assert_eq!(once, b"/.claude/settings.local.json\n");
        let twice = add_exclude_entry(&once, "/.claude/settings.local.json");
        assert_eq!(twice, once);
    }

    #[test]
    fn exclude_entry_preserves_unrelated_lines_and_adds_a_missing_final_newline() {
        let existing = b"# comment\n*.log".to_vec();
        let updated = add_exclude_entry(&existing, "/.codex/hooks.json");
        assert_eq!(updated, b"# comment\n*.log\n/.codex/hooks.json\n");
    }

    #[test]
    fn exclude_entry_preserves_crlf_convention() {
        let existing = b"# comment\r\n".to_vec();
        let updated = add_exclude_entry(&existing, "/.claude/settings.local.json");
        assert_eq!(updated, b"# comment\r\n/.claude/settings.local.json\r\n");
    }

    #[test]
    fn claude_and_then_codex_entries_both_land_and_a_rerun_is_byte_stable() {
        let after_claude = add_exclude_entry(b"", "/.claude/settings.local.json");
        let after_codex = add_exclude_entry(&after_claude, "/.codex/hooks.json");
        let rerun = add_exclude_entry(&after_codex, "/.claude/settings.local.json");
        assert_eq!(rerun, after_codex);
        assert_eq!(
            after_codex,
            b"/.claude/settings.local.json\n/.codex/hooks.json\n"
        );
    }
}
