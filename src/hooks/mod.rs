pub mod config;
pub mod decision;
pub mod payload;
pub mod response;

use clap::ValueEnum;

/// Which harness's response dialect a `hook-guard` invocation must speak.
/// Claude Code and Copilot CLI share one composite response (see
/// `response::render`); Codex has its own nested shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Protocol {
    ClaudeCompatible,
    Codex,
}

/// The hook event `hook-guard` was invoked for. Only `PreToolUse` is
/// implemented; the plan scopes this feature to that one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "PascalCase")]
pub enum Event {
    PreToolUse,
}

/// The one decision `hook-guard` ever renders: allow the tool call, or deny
/// it with a human-readable reason a harness can surface to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}
