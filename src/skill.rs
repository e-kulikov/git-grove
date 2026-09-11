use crate::error::{GroveError, Result};
use std::io::Write;

/// The generic, version-free agent skill document, embedded at compile time.
/// Printed verbatim by `git grove --skill` before any policy, filesystem, or
/// Git work.
pub const SKILL: &str = include_str!("skill.md");

pub fn write(writer: &mut dyn Write) -> Result<()> {
    writer
        .write_all(SKILL.as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|error| GroveError::failure(format!("cannot write stdout: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_emits_the_embedded_document_verbatim() {
        let mut buffer = Vec::new();
        write(&mut buffer).unwrap();
        assert_eq!(buffer, SKILL.as_bytes());
    }

    #[test]
    fn the_document_has_yaml_frontmatter_naming_the_tool() {
        assert!(SKILL.starts_with("---\nname: git-grove\n"));
    }

    #[test]
    fn the_document_protects_bare_and_never_smuggles_the_orchestration_convention() {
        assert!(SKILL.contains("Never edit or delete anything inside `.bare/`"));
        assert!(!SKILL.contains("Work inside a worktree, never at the grove root"));
    }

    #[test]
    fn the_document_carries_no_version_stamp() {
        assert!(!SKILL.contains(env!("CARGO_PKG_VERSION")));
        assert!(!SKILL.to_lowercase().contains("version"));
    }
}
