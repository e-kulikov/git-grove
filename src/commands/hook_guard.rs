use crate::error::{GroveError, Result};
use crate::grove::discover::Grove;
use crate::hooks::{decision, payload, Protocol, Verdict};
use std::io::{Read, Write};
use std::path::PathBuf;

/// Handle one `hook-guard` invocation end to end: read the payload from
/// `reader`, decide allow/deny, and write exactly one JSON response object
/// (plus a trailing newline) to `writer`. `process_cwd` is the outcome of
/// reading the process's own current directory — used both to discover the
/// grove (never the payload's own `cwd`, which is agent-influenced; see
/// `src/hooks/decision.rs`) and, when the payload omits its own `cwd`, to
/// absolutize relative candidate paths. It is a `Result` rather than an
/// already-unwrapped `&Path` deliberately: even a failure to read the
/// current directory must still produce a protocol-valid deny, not a
/// process error, for the same reason grove-discovery failure does (below).
///
/// Always returns `Ok(())` except for a genuine I/O failure reading the
/// payload or writing the response — a malformed payload, an unreadable
/// current directory, or a `process_cwd` outside any grove all still
/// produce a protocol-valid deny and `Ok(())`. The hook contract's
/// harnesses read allow/deny from stdout JSON on exit 0, not from the
/// process exit code; an unhandled error here would exit nonzero, which
/// every measured harness treats as "the hook did not run" and lets the
/// tool call through — a nonzero exit is fail-open, not fail-closed, and
/// would silently discard exactly the denial this command exists to print.
pub fn run(
    protocol: Protocol,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    process_cwd: std::io::Result<PathBuf>,
) -> Result<()> {
    let mut raw = Vec::new();
    reader
        .read_to_end(&mut raw)
        .map_err(|error| GroveError::failure(format!("cannot read hook payload: {error}")))?;

    let verdict = decide(&raw, process_cwd);

    let body = crate::hooks::response::render(protocol, &verdict);
    let mut bytes = serde_json::to_vec(&body)
        .map_err(|error| GroveError::failure(format!("cannot serialize hook response: {error}")))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .and_then(|_| writer.flush())
        .map_err(|error| GroveError::failure(format!("cannot write hook response: {error}")))
}

fn decide(raw: &[u8], process_cwd: std::io::Result<PathBuf>) -> Verdict {
    let normalized = match payload::normalize(raw) {
        Ok(normalized) => normalized,
        Err(reason) => return Verdict::Deny(format!("grove invariant: {reason}")),
    };

    let process_cwd = match process_cwd {
        Ok(cwd) => cwd,
        Err(error) => {
            return Verdict::Deny(format!(
                "grove invariant: refusing -- cannot determine the current directory ({error}), \
                 so grove invariants cannot be checked."
            ))
        }
    };

    let grove = match Grove::discover(&process_cwd) {
        Ok(grove) => grove,
        Err(_) => {
            return Verdict::Deny(format!(
                "grove invariant: refusing -- not inside a grove (discovery from {} found no \
                 grove signature), so grove invariants cannot be checked. If this project is \
                 not a grove worktree, remove the git-grove hook group from the agent's local \
                 hook configuration.",
                process_cwd.display()
            ))
        }
    };
    let canonical_bare = grove.bare_dir();
    let canonical_git = grove.root.join(".git");

    decision::decide(&normalized, &canonical_bare, &canonical_git, &process_cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_grove_process_cwd_denies_with_an_actionable_reason_on_both_protocols() {
        let dir = tempfile::tempdir().unwrap();
        for protocol in [Protocol::ClaudeCompatible, Protocol::Codex] {
            let mut output = Vec::new();
            run(
                protocol,
                &mut br#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#.as_slice(),
                &mut output,
                Ok(dir.path().to_path_buf()),
            )
            .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&output).unwrap();
            let reason = match protocol {
                Protocol::ClaudeCompatible => {
                    assert_eq!(body["permissionDecision"], "deny");
                    body["permissionDecisionReason"].as_str().unwrap()
                }
                Protocol::Codex => {
                    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
                    body["hookSpecificOutput"]["permissionDecisionReason"]
                        .as_str()
                        .unwrap()
                }
            };
            assert!(reason.contains("not inside a grove"));
            assert!(reason.contains("remove the git-grove hook group"));
        }
    }

    #[test]
    fn an_unreadable_current_directory_denies_rather_than_erroring() {
        let mut output = Vec::new();
        let result = run(
            Protocol::ClaudeCompatible,
            &mut br#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#.as_slice(),
            &mut output,
            Err(std::io::Error::other("stale cwd")),
        );
        assert!(result.is_ok());
        let body: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(body["permissionDecision"], "deny");
        assert!(body["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("cannot determine the current directory"));
    }

    #[test]
    fn a_malformed_payload_denies_rather_than_erroring() {
        let mut output = Vec::new();
        let result = run(
            Protocol::Codex,
            &mut b"not json".as_slice(),
            &mut output,
            Ok(PathBuf::from("/")),
        );
        assert!(result.is_ok());
        let body: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn output_is_exactly_one_json_object_followed_by_a_newline() {
        let mut output = Vec::new();
        run(
            Protocol::Codex,
            &mut b"not json".as_slice(),
            &mut output,
            Ok(PathBuf::from("/")),
        )
        .unwrap();
        assert_eq!(output.last(), Some(&b'\n'));
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        serde_json::from_str::<serde_json::Value>(text.trim_end()).unwrap();
    }
}
