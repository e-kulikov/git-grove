use serde_json::{Map, Value};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};

/// Exact marker every canonical group carries, used both to build the group
/// and to recognize (and only ever touch) prior groups this tool wrote.
pub const CLAUDE_COMPATIBLE_MARKER_KEY: &str = "description";
pub const CLAUDE_COMPATIBLE_MARKER_VALUE: &str =
    "git-grove: protect grove metadata (managed by git grove setup)";
pub const CODEX_MARKER_KEY: &str = "_id";
pub const CODEX_MARKER_VALUE: &str = "git-grove.protect-metadata.v1";

/// Seconds a harness gives one installed handler before abandoning it.
pub const TIMEOUT_SECONDS: i64 = 15;

/// Which serialization an agent's native hook-config file uses. The tree is
/// identical either way — see [`HookGroup`]; only the bytes differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Claude Code and Copilot CLI: `.claude/settings.local.json`.
    Json,
    /// Codex CLI: `.codex/config.toml`, with the group defined inline.
    Toml,
}

/// One canonical `PreToolUse` hook group, described independently of the
/// file format it lands in. Both emitters below consume exactly this, so
/// the JSON and TOML targets cannot drift apart in matcher, command,
/// timeout, or marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookGroup {
    /// Field name whose exact `marker_value` identifies a group this tool
    /// owns, and is therefore allowed to replace.
    pub marker_key: &'static str,
    pub marker_value: &'static str,
    pub matcher: &'static str,
    pub command: String,
}

impl HookGroup {
    /// The group Claude Code and Copilot CLI share. `executable` is the
    /// canonicalized absolute path to the current `git-grove` binary.
    pub fn claude_compatible(executable: &str) -> Self {
        Self {
            marker_key: CLAUDE_COMPATIBLE_MARKER_KEY,
            marker_value: CLAUDE_COMPATIBLE_MARKER_VALUE,
            matcher: "Bash|Edit|Write",
            command: format!("{executable} hook-guard --protocol claude-compatible PreToolUse"),
        }
    }

    /// The Codex group. Its matcher carries Codex's own `apply_patch` tool
    /// on top of the shared three. `executable` is the canonicalized
    /// absolute path to the current `git-grove` binary.
    pub fn codex(executable: &str) -> Self {
        Self {
            marker_key: CODEX_MARKER_KEY,
            marker_value: CODEX_MARKER_VALUE,
            matcher: "Bash|Edit|Write|apply_patch",
            command: format!("{executable} hook-guard --protocol codex PreToolUse"),
        }
    }

    /// This group as Claude/Copilot JSON.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            self.marker_key: self.marker_value,
            "matcher": self.matcher,
            "hooks": [{
                "type": "command",
                "command": self.command,
                "timeout": TIMEOUT_SECONDS
            }]
        })
    }

    /// This group as one `[[hooks.PreToolUse]]` TOML table, whose handler
    /// list renders as a nested `[[hooks.PreToolUse.hooks]]`.
    fn to_toml_table(&self) -> Table {
        let mut handler = Table::new();
        handler.insert("type", Item::Value("command".into()));
        handler.insert("command", Item::Value(self.command.as_str().into()));
        handler.insert("timeout", Item::Value(TIMEOUT_SECONDS.into()));
        let mut handlers = ArrayOfTables::new();
        handlers.push(handler);

        let mut group = Table::new();
        group.insert(self.marker_key, Item::Value(self.marker_value.into()));
        group.insert("matcher", Item::Value(self.matcher.into()));
        group.insert("hooks", Item::ArrayOfTables(handlers));
        group
    }

    /// The same group as an inline table, for the one shape this module
    /// does not write but must not destroy: a file whose author already
    /// spelled `PreToolUse` as an inline `[ { … } ]` array.
    fn to_toml_inline(&self) -> InlineTable {
        let mut handler = InlineTable::new();
        handler.insert("type", "command".into());
        handler.insert("command", self.command.as_str().into());
        handler.insert("timeout", TIMEOUT_SECONDS.into());
        let mut handlers = Array::new();
        handlers.push(toml_edit::Value::InlineTable(handler));

        let mut group = InlineTable::new();
        group.insert(self.marker_key, self.marker_value.into());
        group.insert("matcher", self.matcher.into());
        group.insert("hooks", toml_edit::Value::Array(handlers));
        group
    }
}

/// Merge `group` into `existing` in `format`, returning the exact bytes to
/// write. The one entry point every caller that writes a hook config uses;
/// a refusal returns the path/type problem and produces no bytes at all.
pub fn merged_bytes(existing: &[u8], format: Format, group: &HookGroup) -> Result<Vec<u8>, String> {
    match format {
        Format::Json => Ok(render(&merge(existing, group)?)),
        Format::Toml => merge_toml(existing, group),
    }
}

/// Merge `group` into `existing` under `hooks.PreToolUse`, touching nothing
/// else: parses `existing` as a JSON object (or starts from `{}` when
/// `existing` is empty), finds every array entry whose `group.marker_key`
/// field is exactly `group.marker_value`, replaces the first with `group`
/// and drops any duplicates, or appends `group` when none matched. Every
/// unrelated top-level value, event, group, and handler — and the array's
/// ordering for everything else — is preserved exactly; this never touches
/// `hooks` keys other than `PreToolUse`, and never rewrites the whole file.
///
/// Refuses (returning the path/type problem, producing no bytes to write)
/// when `existing` is not empty and not valid JSON, when its root or its
/// `hooks` value is present but not an object, or when `hooks.PreToolUse`
/// is present but not an array. A caller that only ever wrote through this
/// function cannot construct any of those shapes; they exist to protect a
/// file this tool does not own alone.
pub fn merge(existing: &[u8], group: &HookGroup) -> Result<Value, String> {
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
            .and_then(|object| object.get(group.marker_key))
            .and_then(Value::as_str)
            == Some(group.marker_value)
    };

    let mut replaced = false;
    let mut merged = Vec::with_capacity(array.len() + 1);
    for entry in array.drain(..) {
        if is_owned(&entry) {
            if !replaced {
                merged.push(group.to_json());
                replaced = true;
            }
        } else {
            merged.push(entry);
        }
    }
    if !replaced {
        merged.push(group.to_json());
    }
    *array = merged;

    Ok(Value::Object(root))
}

/// Render a merged config as pretty-printed JSON with a trailing newline —
/// the canonical on-disk form every JSON write and every idempotency check
/// in this module compares against.
pub fn render(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("a merged Value always serializes");
    bytes.push(b'\n');
    bytes
}

/// The TOML counterpart of [`merge`], with the same ownership discipline
/// against the same two keys — `hooks.PreToolUse`, plus the
/// `features.hooks = true` toggle Codex needs before it reads hooks at all.
/// Every unrelated key, table, comment, and formatting byte in the file is
/// preserved: Codex's `config.toml` legitimately holds settings that have
/// nothing to do with this tool, so nothing outside those two keys is
/// rewritten, reflowed, or reordered.
///
/// Refuses (producing no bytes) when `existing` is not valid UTF-8 or not
/// valid TOML, when `features` is present but not table-like, when
/// `features.hooks` is present but not a boolean, when `hooks` is present
/// but not a standard table, or when `hooks.PreToolUse` is present in any
/// shape other than an array of tables (`[[hooks.PreToolUse]]`) or an
/// array whose every element is an inline table.
pub fn merge_toml(existing: &[u8], group: &HookGroup) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(existing).map_err(|error| format!("is not UTF-8: {error}"))?;
    let mut document: DocumentMut = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse()
            .map_err(|error| format!("not valid TOML: {error}"))?
    };

    enable_hooks_feature(&mut document)?;
    merge_entries(pre_tool_use(&mut document)?, group);

    let mut bytes = document.to_string().into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Idempotently assert `features.hooks = true`, the gate Codex requires
/// before it loads any hook definition. Already-`true` is left byte-for-byte
/// alone rather than reassigned, so a rerun does not disturb the key's own
/// formatting or comments.
fn enable_hooks_feature(document: &mut DocumentMut) -> Result<(), String> {
    let root = document.as_table_mut();
    if !root.contains_key("features") {
        let mut created = Table::new();
        created.insert("hooks", Item::Value(true.into()));
        root.insert("features", Item::Table(created));
        return Ok(());
    }
    let features = root
        .get_mut("features")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| "`features` is not a TOML table".to_string())?;
    match features.get("hooks") {
        Some(item) if item.as_bool() == Some(true) => Ok(()),
        Some(item) if item.as_bool() == Some(false) => {
            features.insert("hooks", Item::Value(true.into()));
            Ok(())
        }
        Some(_) => Err("`features.hooks` is not a boolean".to_string()),
        None => {
            features.insert("hooks", Item::Value(true.into()));
            Ok(())
        }
    }
}

/// The two shapes a TOML `hooks.PreToolUse` can legitimately hold a list of
/// groups in. This module only ever *writes* `Tables` — the
/// `[[hooks.PreToolUse]]` form Codex's own documentation and every measured
/// working config use — but it must merge into `Inline` without destroying
/// it, because that form is equally valid TOML and this file is not ours
/// alone.
enum Entries<'a> {
    Tables(&'a mut ArrayOfTables),
    Inline(&'a mut Array),
}

impl Entries<'_> {
    fn len(&self) -> usize {
        match self {
            Entries::Tables(array) => array.len(),
            Entries::Inline(array) => array.len(),
        }
    }

    fn is_owned(&self, index: usize, group: &HookGroup) -> bool {
        let marker = match self {
            Entries::Tables(array) => array
                .get(index)
                .and_then(|table| table.get(group.marker_key))
                .and_then(Item::as_str),
            Entries::Inline(array) => array
                .get(index)
                .and_then(|value| value.as_inline_table())
                .and_then(|table| table.get(group.marker_key))
                .and_then(toml_edit::Value::as_str),
        };
        marker == Some(group.marker_value)
    }

    fn replace(&mut self, index: usize, group: &HookGroup) {
        match self {
            Entries::Tables(array) => {
                array.replace(index, group.to_toml_table());
            }
            Entries::Inline(array) => {
                array.replace(index, toml_edit::Value::InlineTable(group.to_toml_inline()));
            }
        }
    }

    fn remove(&mut self, index: usize) {
        match self {
            Entries::Tables(array) => {
                array.remove(index);
            }
            Entries::Inline(array) => {
                array.remove(index);
            }
        }
    }

    fn push(&mut self, group: &HookGroup) {
        match self {
            Entries::Tables(array) => array.push(group.to_toml_table()),
            Entries::Inline(array) => {
                array.push(toml_edit::Value::InlineTable(group.to_toml_inline()))
            }
        }
    }
}

/// Borrow `hooks.PreToolUse` as a mergeable list, creating an empty
/// `[[hooks.PreToolUse]]` array of tables when it is absent. The `hooks`
/// table is created implicit, so an otherwise-empty section header is not
/// emitted for it.
fn pre_tool_use(document: &mut DocumentMut) -> Result<Entries<'_>, String> {
    let root = document.as_table_mut();
    if !root.contains_key("hooks") {
        let mut created = Table::new();
        created.set_implicit(true);
        root.insert("hooks", Item::Table(created));
    }
    let hooks = root
        .get_mut("hooks")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            "`hooks` is not a TOML table (an inline `hooks = { .. }` cannot hold a \
             `[[hooks.PreToolUse]]` array of tables)"
                .to_string()
        })?;
    if !hooks.contains_key("PreToolUse") {
        hooks.insert("PreToolUse", Item::ArrayOfTables(ArrayOfTables::new()));
    }

    let all_inline_tables = matches!(
        hooks.get("PreToolUse"),
        Some(Item::Value(toml_edit::Value::Array(array)))
            if array.iter().all(toml_edit::Value::is_inline_table)
    );
    match hooks.get_mut("PreToolUse") {
        Some(Item::ArrayOfTables(array)) => Ok(Entries::Tables(array)),
        Some(Item::Value(toml_edit::Value::Array(array))) if all_inline_tables => {
            Ok(Entries::Inline(array))
        }
        _ => Err("`hooks.PreToolUse` is not an array of tables".to_string()),
    }
}

/// Replace the first entry this tool owns with `group`, drop any further
/// duplicates of it, or append `group` when it owns none yet — the same
/// ownership rule [`merge`] applies to JSON, and for the same reason: every
/// entry someone else wrote keeps its place and its content.
fn merge_entries(mut entries: Entries<'_>, group: &HookGroup) {
    let mut replaced = false;
    let mut index = 0;
    while index < entries.len() {
        if entries.is_owned(index, group) {
            if replaced {
                entries.remove(index);
                continue;
            }
            entries.replace(index, group);
            replaced = true;
        }
        index += 1;
    }
    if !replaced {
        entries.push(group);
    }
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

    fn claude(executable: &str) -> HookGroup {
        HookGroup::claude_compatible(executable)
    }

    fn codex(executable: &str) -> HookGroup {
        HookGroup::codex(executable)
    }

    fn toml_text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn parsed(bytes: &[u8]) -> DocumentMut {
        toml_text(bytes).parse().unwrap()
    }

    #[test]
    fn appends_the_group_to_an_absent_file() {
        let group = claude("/bin/git-grove");
        let merged = merge(b"", &group).unwrap();
        assert_eq!(merged["hooks"]["PreToolUse"], json!([group.to_json()]));
    }

    #[test]
    fn replaces_the_first_owned_entry_and_drops_duplicates_in_place() {
        let stale = claude("/old/git-grove").to_json();
        let existing = json!({
            "hooks": {"PreToolUse": [{"description": "unrelated"}, stale.clone(), stale.clone()]}
        });
        let fresh = claude("/new/git-grove");
        let merged = merge(&serde_json::to_vec(&existing).unwrap(), &fresh).unwrap();
        assert_eq!(
            merged["hooks"]["PreToolUse"],
            json!([{"description": "unrelated"}, fresh.to_json()])
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
            &codex("/bin/git-grove"),
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
        let group = claude("/bin/git-grove");
        assert!(merge(b"not json", &group).is_err());
        assert!(merge(b"[1,2,3]", &group).is_err());
        assert!(merge(br#"{"hooks":{"PreToolUse":"nope"}}"#, &group).is_err());
        assert!(merge(br#"{"hooks":"nope"}"#, &group).is_err());
    }

    #[test]
    fn a_similar_but_unmarked_entry_is_left_alone() {
        let existing = json!({"hooks": {"PreToolUse": [{"description": "someone else's hook"}]}});
        let merged = merge(
            &serde_json::to_vec(&existing).unwrap(),
            &claude("/bin/git-grove"),
        )
        .unwrap();
        assert_eq!(
            merged["hooks"]["PreToolUse"][0],
            json!({"description": "someone else's hook"})
        );
        assert_eq!(merged["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_json_and_toml_emitters_describe_one_group() {
        let group = codex("/abs/git-grove");
        let json = group.to_json();
        let document = parsed(&merge_toml(b"", &group).unwrap());
        let entry = &document["hooks"]["PreToolUse"][0];

        assert_eq!(
            entry[CODEX_MARKER_KEY].as_str(),
            json[CODEX_MARKER_KEY].as_str()
        );
        assert_eq!(entry["matcher"].as_str(), json["matcher"].as_str());
        assert_eq!(
            entry["hooks"][0]["command"].as_str(),
            json["hooks"][0]["command"].as_str()
        );
        assert_eq!(
            entry["hooks"][0]["timeout"].as_integer(),
            Some(TIMEOUT_SECONDS)
        );
        assert_eq!(entry["hooks"][0]["type"].as_str(), Some("command"));
    }

    /// The exact shape measured working against installed Codex: the
    /// feature toggle, one `[[hooks.PreToolUse]]` group carrying the
    /// marker and matcher, and one nested `[[hooks.PreToolUse.hooks]]`
    /// command handler.
    #[test]
    fn writes_the_measured_codex_shape_from_nothing() {
        let bytes = merge_toml(b"", &codex("/abs/git-grove")).unwrap();
        assert_eq!(
            toml_text(&bytes),
            "[features]\n\
             hooks = true\n\
             \n\
             [[hooks.PreToolUse]]\n\
             _id = \"git-grove.protect-metadata.v1\"\n\
             matcher = \"Bash|Edit|Write|apply_patch\"\n\
             \n\
             [[hooks.PreToolUse.hooks]]\n\
             type = \"command\"\n\
             command = \"/abs/git-grove hook-guard --protocol codex PreToolUse\"\n\
             timeout = 15\n"
        );
    }

    #[test]
    fn toml_rerun_is_byte_stable() {
        let group = codex("/abs/git-grove");
        let once = merge_toml(b"", &group).unwrap();
        let twice = merge_toml(&once, &group).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn toml_replaces_a_stale_command_in_place_and_drops_duplicates() {
        let stale = merge_toml(b"", &codex("/old/git-grove")).unwrap();
        let doubled = merge_toml(&stale, &codex("/old/git-grove")).unwrap();
        assert_eq!(stale, doubled, "a rerun must not grow the array");

        let fresh = merge_toml(&stale, &codex("/new/git-grove")).unwrap();
        let document = parsed(&fresh);
        let array = document["hooks"]["PreToolUse"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(array.len(), 1);
        assert_eq!(
            array.get(0).unwrap()["hooks"][0]["command"].as_str(),
            Some("/new/git-grove hook-guard --protocol codex PreToolUse")
        );
    }

    #[test]
    fn toml_preserves_unrelated_settings_comments_and_other_events() {
        let existing = b"# a comment this tool must not eat\n\
                         model = \"gpt-5\"\n\
                         \n\
                         [features]\n\
                         # another one\n\
                         web_search = true\n\
                         \n\
                         [[hooks.SessionStart]]\n\
                         matcher = \"*\"\n\
                         \n\
                         [[hooks.PreToolUse]]\n\
                         _id = \"someone.elses.hook\"\n\
                         matcher = \"Bash\"\n";
        let merged = merge_toml(existing, &codex("/abs/git-grove")).unwrap();
        let text = toml_text(&merged);

        assert!(text.contains("# a comment this tool must not eat"));
        assert!(text.contains("# another one"));
        assert!(text.contains("model = \"gpt-5\""));
        assert!(text.contains("web_search = true"));
        assert!(text.contains("[[hooks.SessionStart]]"));

        let document = parsed(&merged);
        assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
        let array = document["hooks"]["PreToolUse"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(array.len(), 2);
        assert_eq!(
            array.get(0).unwrap()[CODEX_MARKER_KEY].as_str(),
            Some("someone.elses.hook"),
            "an entry this tool does not own keeps its place"
        );
        assert_eq!(
            array.get(1).unwrap()[CODEX_MARKER_KEY].as_str(),
            Some(CODEX_MARKER_VALUE)
        );
    }

    #[test]
    fn toml_flips_an_explicitly_disabled_hooks_feature_on() {
        let merged = merge_toml(b"[features]\nhooks = false\n", &codex("/abs/git-grove")).unwrap();
        assert_eq!(parsed(&merged)["features"]["hooks"].as_bool(), Some(true));
    }

    #[test]
    fn toml_merges_into_an_inline_features_table_without_flattening_it() {
        let merged = merge_toml(
            b"features = { web_search = true }\n",
            &codex("/abs/git-grove"),
        )
        .unwrap();
        let text = toml_text(&merged);
        assert!(text.contains("features = {"), "got {text}");
        let document = parsed(&merged);
        assert_eq!(document["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(document["features"]["web_search"].as_bool(), Some(true));
    }

    #[test]
    fn toml_merges_into_an_inline_pre_tool_use_array_without_rewriting_it() {
        let existing = b"[hooks]\nPreToolUse = [{ _id = \"someone.elses.hook\" }]\n";
        let merged = merge_toml(existing, &codex("/abs/git-grove")).unwrap();
        let document = parsed(&merged);
        let array = document["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(array.len(), 2);
        assert_eq!(
            array.get(0).unwrap().as_inline_table().unwrap()[CODEX_MARKER_KEY].as_str(),
            Some("someone.elses.hook")
        );
        let ours = array.get(1).unwrap().as_inline_table().unwrap();
        assert_eq!(ours[CODEX_MARKER_KEY].as_str(), Some(CODEX_MARKER_VALUE));
        assert_eq!(
            ours["hooks"]
                .as_array()
                .unwrap()
                .get(0)
                .unwrap()
                .as_inline_table()
                .unwrap()["command"]
                .as_str(),
            Some("/abs/git-grove hook-guard --protocol codex PreToolUse")
        );

        let rerun = merge_toml(&merged, &codex("/abs/git-grove")).unwrap();
        assert_eq!(merged, rerun);
    }

    #[test]
    fn refuses_malformed_toml_and_every_wrong_typed_key() {
        let group = codex("/abs/git-grove");
        for existing in [
            &b"this is not = = toml"[..],
            &b"features = 3\n"[..],
            &b"[features]\nhooks = \"yes\"\n"[..],
            &b"hooks = { PreToolUse = [] }\n"[..],
            &b"[hooks]\nPreToolUse = \"nope\"\n"[..],
            &b"[hooks]\nPreToolUse = [1, 2, 3]\n"[..],
        ] {
            assert!(
                merge_toml(existing, &group).is_err(),
                "expected a refusal for {:?}",
                String::from_utf8_lossy(existing)
            );
        }
        assert!(merge_toml(&[0xff, 0xfe], &group).is_err());
    }

    #[test]
    fn merged_bytes_dispatches_on_format() {
        let json = merged_bytes(b"", Format::Json, &claude("/abs/git-grove")).unwrap();
        assert_eq!(
            json,
            render(&merge(b"", &claude("/abs/git-grove")).unwrap())
        );
        let toml = merged_bytes(b"", Format::Toml, &codex("/abs/git-grove")).unwrap();
        assert_eq!(toml, merge_toml(b"", &codex("/abs/git-grove")).unwrap());
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
        let updated = add_exclude_entry(&existing, "/.codex/config.toml");
        assert_eq!(updated, b"# comment\n*.log\n/.codex/config.toml\n");
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
        let after_codex = add_exclude_entry(&after_claude, "/.codex/config.toml");
        let rerun = add_exclude_entry(&after_codex, "/.claude/settings.local.json");
        assert_eq!(rerun, after_codex);
        assert_eq!(
            after_codex,
            b"/.claude/settings.local.json\n/.codex/config.toml\n"
        );
    }
}
