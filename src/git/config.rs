//! Remote and configuration plumbing shared by the commands that create or
//! repair a grove's remote: `clone` and `publish`.
//!
//! This module is plumbing with no policy. It reads and writes configuration
//! through the caller's pinned absolute `--file` path, expands and validates
//! fetch refspecs, and derives a remote's default branch. Which of those a
//! command *may* do, and when, is the command's decision, not this module's.

use crate::commands::init::state_conflict;
use crate::error::{GroveError, Result};
use crate::git::runner::{GitOutput, GitRunner, Invocation};
use bstr::ByteSlice;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

pub(crate) fn escaped(bytes: &[u8]) -> String {
    bytes.escape_bytes().to_string()
}

pub(crate) fn required(
    runner: &dyn GitRunner,
    invocation: Invocation,
    operation: &str,
) -> Result<GitOutput> {
    let output = runner.run(invocation)?;
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "git {operation} failed with exit status {}",
            output.status
        ))
        .with_detail(escaped(&output.stderr)));
    }
    Ok(output)
}

pub(crate) fn trim_one_line(mut bytes: Vec<u8>, description: &str) -> Result<Vec<u8>> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(GroveError::failure(format!(
            "git returned an invalid {description}"
        )));
    }
    Ok(bytes)
}

pub(crate) fn config_key(prefix: &[u8], name: &OsStr, suffix: &[u8]) -> OsString {
    let mut key = Vec::with_capacity(prefix.len() + name.as_bytes().len() + suffix.len());
    key.extend_from_slice(prefix);
    key.extend_from_slice(name.as_bytes());
    key.extend_from_slice(suffix);
    OsString::from_vec(key)
}

pub(crate) fn config_values(
    runner: &dyn GitRunner,
    config: &Path,
    key: &OsStr,
) -> Result<Vec<Vec<u8>>> {
    let output = runner.run(Invocation::new().args([
        OsStr::new("config"),
        OsStr::new("--null"),
        OsStr::new("--file"),
        config.as_os_str(),
        OsStr::new("--get-all"),
        key,
    ]))?;
    if output.status == 1 {
        return Ok(Vec::new());
    }
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "git config --get-all {} failed with exit status {}",
            escaped(key.as_bytes()),
            output.status
        ))
        .with_detail(escaped(&output.stderr)));
    }
    if output.stdout.last() != Some(&b'\0') {
        return Err(GroveError::failure(
            "git config returned a truncated NUL-delimited value",
        ));
    }
    Ok(output.stdout[..output.stdout.len() - 1]
        .split(|byte| *byte == b'\0')
        .map(<[u8]>::to_vec)
        .collect())
}

pub(crate) fn set_config(
    runner: &dyn GitRunner,
    config: &Path,
    key: &OsStr,
    value: &OsStr,
    add: bool,
) -> Result<()> {
    let mut args = vec![
        OsString::from("config"),
        OsString::from("--file"),
        config.as_os_str().to_os_string(),
    ];
    if add {
        args.push(OsString::from("--add"));
    }
    args.push(key.to_os_string());
    args.push(value.to_os_string());
    required(runner, Invocation::new().args(args), "config write")?;
    Ok(())
}

pub(crate) fn list_local_heads(runner: &dyn GitRunner, bare: &Path) -> Result<Vec<Vec<u8>>> {
    let output = required(
        runner,
        Invocation::new().git_dir(bare).args([
            "for-each-ref",
            "--format=%(refname)",
            "--",
            "refs/heads",
        ]),
        "for-each-ref local heads",
    )?;
    let mut heads = Vec::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(b"refs/heads/") || line.contains(&b'\r') {
            return Err(GroveError::failure(
                "git returned an invalid local branch ref",
            ));
        }
        heads.push(line.to_vec());
    }
    Ok(heads)
}

pub(crate) fn refspec_destination(spec: &[u8], source: &[u8]) -> Option<Vec<u8>> {
    let spec = spec.strip_prefix(b"+").unwrap_or(spec);
    if spec.starts_with(b"^") {
        return None;
    }
    let colon = spec.iter().position(|byte| *byte == b':')?;
    let (from, to_with_colon) = spec.split_at(colon);
    let to = &to_with_colon[1..];
    match (
        from.iter().position(|byte| *byte == b'*'),
        to.iter().position(|byte| *byte == b'*'),
    ) {
        (None, None) if from == source => Some(to.to_vec()),
        (Some(from_star), Some(to_star))
            if !from[from_star + 1..].contains(&b'*') && !to[to_star + 1..].contains(&b'*') =>
        {
            let matched = source
                .strip_prefix(&from[..from_star])?
                .strip_suffix(&from[from_star + 1..])?;
            let mut destination = Vec::with_capacity(to.len() + matched.len());
            destination.extend_from_slice(&to[..to_star]);
            destination.extend_from_slice(matched);
            destination.extend_from_slice(&to[to_star + 1..]);
            Some(destination)
        }
        _ => None,
    }
}

pub(crate) fn validate_refspec_destinations(refspecs: &[Vec<u8>], remote: &OsStr) -> Result<()> {
    let mut prefix = b"refs/remotes/".to_vec();
    prefix.extend_from_slice(remote.as_bytes());
    prefix.push(b'/');
    for refspec in refspecs {
        let Some(colon) = refspec.iter().position(|byte| *byte == b':') else {
            return Err(state_conflict(
                "the clone wrote an invalid fetch refspec",
                escaped(refspec),
            ));
        };
        let destination = &refspec[colon + 1..];
        if !destination.starts_with(&prefix) || destination == prefix {
            return Err(state_conflict(
                "the clone wrote a fetch refspec outside the grove remote namespace",
                escaped(refspec),
            ));
        }
    }
    Ok(())
}

pub(crate) fn configure_upstreams(
    runner: &dyn GitRunner,
    bare: &Path,
    remote: &OsStr,
    refspecs: &[Vec<u8>],
) -> Result<()> {
    let config = bare.join("config");
    for local in list_local_heads(runner, bare)? {
        let branch = local.strip_prefix(b"refs/heads/").expect("validated head");
        let candidates = refspecs
            .iter()
            .filter_map(|spec| refspec_destination(spec, &local))
            .collect::<Vec<_>>();
        let Some(destination) = candidates.first() else {
            continue;
        };
        if candidates.iter().any(|candidate| candidate != destination) {
            return Err(state_conflict(
                "fetch refspecs map a branch to several destinations",
                escaped(&local),
            ));
        }
        let exists = runner.run(Invocation::new().git_dir(bare).args([
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::from_bytes(destination),
        ]))?;
        match exists.status {
            0 => {}
            1 => continue,
            _ => {
                return Err(
                    GroveError::failure("git show-ref failed while configuring upstreams")
                        .with_detail(escaped(&exists.stderr)),
                )
            }
        }
        let branch_os = OsStr::from_bytes(branch);
        let remote_key = config_key(b"branch.", branch_os, b".remote");
        let merge_key = config_key(b"branch.", branch_os, b".merge");
        set_config(runner, &config, &remote_key, remote, false)?;
        set_config(
            runner,
            &config,
            &merge_key,
            OsStr::from_bytes(&local),
            false,
        )?;
        if config_values(runner, &config, &remote_key)? != [remote.as_bytes()]
            || config_values(runner, &config, &merge_key)? != [local.as_slice()]
        {
            return Err(GroveError::failure(
                "upstream configuration verification failed",
            ));
        }
    }
    Ok(())
}

pub(crate) fn remote_head_branch(
    runner: &dyn GitRunner,
    bare: &Path,
    remote: &OsStr,
) -> Result<OsString> {
    let mut head = b"refs/remotes/".to_vec();
    head.extend_from_slice(remote.as_bytes());
    head.extend_from_slice(b"/HEAD");
    let target = trim_one_line(
        required(
            runner,
            Invocation::new().git_dir(bare).args([
                OsStr::new("symbolic-ref"),
                OsStr::new("--quiet"),
                OsStr::from_bytes(&head),
            ]),
            "symbolic-ref remote HEAD",
        )?
        .stdout,
        "remote HEAD",
    )?;
    let mut prefix = b"refs/remotes/".to_vec();
    prefix.extend_from_slice(remote.as_bytes());
    prefix.push(b'/');
    let branch = target.strip_prefix(prefix.as_slice()).ok_or_else(|| {
        state_conflict(
            "the remote HEAD points outside its remote namespace",
            escaped(&target),
        )
    })?;
    if branch.is_empty() || branch == b"HEAD" {
        return Err(state_conflict(
            "the remote HEAD does not name a branch",
            escaped(&target),
        ));
    }

    let exact = runner.run(Invocation::new().git_dir(bare).args([
        OsStr::new("show-ref"),
        OsStr::new("--verify"),
        OsStr::new("--hash"),
        OsStr::new("--"),
        OsStr::from_bytes(&target),
    ]))?;
    if exact.status == 1 {
        return Err(GroveError::failure(format!(
            "remote HEAD target {} does not exist",
            escaped(&target)
        )));
    }
    if !exact.ok() {
        return Err(GroveError::failure(format!(
            "git show-ref failed while verifying remote HEAD target {}",
            escaped(&target)
        ))
        .with_detail(escaped(&exact.stderr)));
    }
    let expected_oid = trim_one_line(exact.stdout, "remote HEAD target object ID")?;
    let mut commit = OsString::from_vec(target.clone());
    commit.push("^{commit}");
    let resolved_oid = trim_one_line(
        required(
            runner,
            Invocation::new().git_dir(bare).args([
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                commit.as_os_str(),
            ]),
            "rev-parse remote HEAD target",
        )?
        .stdout,
        "resolved remote HEAD target object ID",
    )?;
    if resolved_oid != expected_oid {
        return Err(GroveError::failure(format!(
            "remote HEAD target {} resolved to an unexpected object",
            escaped(&target)
        ))
        .with_detail(format!(
            "expected {}, found {}",
            escaped(&expected_oid),
            escaped(&resolved_oid)
        )));
    }
    Ok(OsString::from_vec(branch.to_vec()))
}
