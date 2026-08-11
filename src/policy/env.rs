use bstr::ByteSlice;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

pub const UNSAFE_VARIABLES: &[(&str, &str)] = &[
    (
        "GIT_DIR",
        "names a different repository than the one this command builds",
    ),
    (
        "GIT_COMMON_DIR",
        "redirects the shared part of a repository",
    ),
    (
        "GIT_WORK_TREE",
        "names a working tree that does not belong to this grove",
    ),
    (
        "GIT_INDEX_FILE",
        "redirects the index that records staged changes",
    ),
    (
        "GIT_OBJECT_DIRECTORY",
        "redirects where objects are written and breaks cloning",
    ),
    (
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "borrows objects from another repository",
    ),
    ("GIT_NAMESPACE", "hides refs behind a namespace"),
    (
        "GIT_CONFIG",
        "silently redirects `git config` writes to another file",
    ),
    (
        "GIT_CONFIG_COUNT",
        "overlays configuration the layout depends on",
    ),
    (
        "GIT_CONFIG_PARAMETERS",
        "overlays configuration the layout depends on",
    ),
];

const OVERLAY_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

#[derive(Debug, Clone)]
pub struct Finding {
    pub name: String,
    pub value: String,
    pub reason: &'static str,
}

fn reason_for(name: &str) -> Option<&'static str> {
    if let Some((_, reason)) = UNSAFE_VARIABLES
        .iter()
        .find(|(variable, _)| *variable == name)
    {
        return Some(reason);
    }
    if OVERLAY_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Some("overlays configuration the layout depends on");
    }
    None
}

pub fn scan<I>(vars: I) -> Vec<Finding>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut findings = vars
        .into_iter()
        .filter_map(|(name, value)| {
            reason_for(&name).map(|reason| Finding {
                name,
                value,
                reason,
            })
        })
        .collect::<Vec<_>>();
    findings.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.value.cmp(&right.value))
    });
    findings
}

pub fn scan_os<I>(vars: I) -> Vec<Finding>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut findings = vars
        .into_iter()
        .filter_map(|(name, value)| {
            let name = std::str::from_utf8(name.as_bytes()).ok()?;
            reason_for(name).map(|reason| Finding {
                name: name.to_string(),
                value: value.as_bytes().escape_bytes().to_string(),
                reason,
            })
        })
        .collect::<Vec<_>>();
    findings.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.value.cmp(&right.value))
    });
    findings
}

pub fn sanitize(cmd: &mut Command) {
    for (name, _) in UNSAFE_VARIABLES {
        cmd.env_remove(name);
    }

    let inherited_overlay_names = std::env::vars_os()
        .filter_map(|(name, _)| {
            let is_overlay = name.to_str().is_some_and(|name| {
                OVERLAY_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            });
            is_overlay.then_some(name)
        })
        .collect::<Vec<_>>();
    let configured_overlay_names = cmd
        .get_envs()
        .filter_map(|(name, _)| {
            name.to_str()
                .filter(|name| {
                    OVERLAY_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
                .map(|_| name.to_os_string())
        })
        .collect::<Vec<_>>();

    for name in inherited_overlay_names
        .into_iter()
        .chain(configured_overlay_names)
    {
        cmd.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn finds_repository_redirecting_variables() {
        let found = scan(vars(&[
            ("GIT_DIR", "/elsewhere/.git"),
            ("PATH", "/usr/bin"),
        ]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "GIT_DIR");
        assert!(found[0].reason.contains("repository"));
    }

    #[test]
    fn finds_numbered_config_overlay_variables() {
        let found = scan(vars(&[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.bare"),
            ("GIT_CONFIG_VALUE_0", "false"),
        ]));
        let mut names: Vec<_> = found.iter().map(|finding| finding.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"]
        );
    }

    #[test]
    fn returns_findings_in_stable_name_and_value_order() {
        let findings = scan(vars(&[
            ("GIT_WORK_TREE", "/worktree"),
            ("GIT_CONFIG_KEY_1", "z.value"),
            ("GIT_DIR", "/repository/.git"),
            ("GIT_CONFIG_KEY_1", "a.value"),
        ]));
        let names_and_values: Vec<_> = findings
            .into_iter()
            .map(|finding| (finding.name, finding.value))
            .collect();

        assert_eq!(
            names_and_values,
            [
                ("GIT_CONFIG_KEY_1".to_string(), "a.value".to_string()),
                ("GIT_CONFIG_KEY_1".to_string(), "z.value".to_string()),
                ("GIT_DIR".to_string(), "/repository/.git".to_string()),
                ("GIT_WORK_TREE".to_string(), "/worktree".to_string()),
            ]
        );
    }

    #[test]
    fn leaves_transport_variables_alone() {
        let found = scan(vars(&[
            ("GIT_SSH_COMMAND", "ssh -i key"),
            ("GIT_ASKPASS", "/usr/bin/true"),
            ("GIT_TEMPLATE_DIR", "/tmp/t"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ]));
        assert!(found.is_empty(), "unexpected findings: {found:?}");
    }

    #[cfg(unix)]
    #[test]
    fn scans_non_utf8_values_without_panicking_or_losing_bytes() {
        let findings = scan_os([(
            OsString::from("GIT_DIR"),
            OsString::from_vec(b"/redirected/\xff".to_vec()),
        )]);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].name, "GIT_DIR");
        assert_eq!(findings[0].value, r"/redirected/\xFF");
    }

    #[test]
    fn removes_every_unsafe_variable_from_a_child_process() {
        let mut command = Command::new("env");
        for (name, _) in UNSAFE_VARIABLES {
            command.env(name, "unsafe");
        }
        command.env("GIT_CONFIG_KEY_0", "core.bare");
        command.env("GIT_CONFIG_VALUE_0", "false");
        command.env("GIT_SSH_COMMAND", "ssh -i key");

        sanitize(&mut command);

        let output = command.output().unwrap();
        assert!(output.status.success());
        let environment = String::from_utf8(output.stdout).unwrap();
        for (name, _) in UNSAFE_VARIABLES {
            assert!(
                !environment
                    .lines()
                    .any(|line| line.starts_with(&format!("{name}="))),
                "{name} was inherited by the child"
            );
        }
        assert!(!environment
            .lines()
            .any(|line| line.starts_with("GIT_CONFIG_KEY_0=")));
        assert!(!environment
            .lines()
            .any(|line| line.starts_with("GIT_CONFIG_VALUE_0=")));
        assert!(environment
            .lines()
            .any(|line| line == "GIT_SSH_COMMAND=ssh -i key"));
    }

    #[cfg(unix)]
    #[test]
    fn sanitizes_when_an_unrelated_parent_variable_is_not_unicode() {
        const CHILD_MARKER: &str = "GIT_GROVE_TEST_NON_UNICODE_PARENT";

        if std::env::var_os(CHILD_MARKER).is_some() {
            let mut command = Command::new("sh");
            command.args(["-c", "test -z \"${GIT_DIR-}\""]);
            command.env("GIT_DIR", "unsafe");
            sanitize(&mut command);

            let output = command.output().unwrap();
            assert!(output.status.success());
            return;
        }

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "policy::env::tests::sanitizes_when_an_unrelated_parent_variable_is_not_unicode",
                "--exact",
            ])
            .env(CHILD_MARKER, "1")
            .env(OsString::from_vec(b"UNRELATED_\xff".to_vec()), "value")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
