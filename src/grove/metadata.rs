use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use bstr::{BString, ByteSlice};

use crate::error::{GroveError, Result};
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishState {
    Unpublished,
    Publishing,
    Published,
}

impl PublishState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished",
            Self::Publishing => "publishing",
            Self::Published => "published",
        }
    }

    pub fn parse(value: impl AsRef<[u8]>) -> Result<Self> {
        match value.as_ref() {
            b"unpublished" => Ok(Self::Unpublished),
            b"publishing" => Ok(Self::Publishing),
            b"published" => Ok(Self::Published),
            _ => Err(GroveError::failure(
                "grove publish state must be unpublished, publishing, or published",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub version: Option<u32>,
    pub default_branch: Option<BString>,
    pub remote: Option<BString>,
    pub publish_state: PublishState,
}

fn config_path(grove: &Grove) -> PathBuf {
    grove.bare_dir().join("config")
}

fn set(runner: &dyn GitRunner, grove: &Grove, key: &str, value: &[u8]) -> Result<()> {
    let config_path = config_path(grove);
    runner.run_ok(Invocation::new().args([
        OsStr::new("config"),
        OsStr::new("--file"),
        config_path.as_os_str(),
        OsStr::new(key),
        OsStr::from_bytes(value),
    ]))?;
    Ok(())
}

fn get(runner: &dyn GitRunner, grove: &Grove, key: &str) -> Result<Option<BString>> {
    let config_path = config_path(grove);
    let output = runner.run(Invocation::new().args([
        OsStr::new("config"),
        OsStr::new("--file"),
        config_path.as_os_str(),
        OsStr::new("--get"),
        OsStr::new(key),
    ]))?;
    if output.status == 1 {
        return Ok(None);
    }
    if !output.ok() {
        return Err(GroveError::failure(format!(
            "git config --get {key} failed with exit status {}",
            output.status
        ))
        .with_detail(output.stderr.as_slice().escape_bytes().to_string()));
    }

    let mut value = output.stdout;
    if value.last() == Some(&b'\n') {
        value.pop();
    }
    Ok(Some(BString::from(value)))
}

fn parse_version(value: &[u8]) -> Result<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(GroveError::failure("grove.version must be an ASCII u32"));
    }

    let value =
        std::str::from_utf8(value).expect("ASCII digit validation makes grove.version valid UTF-8");
    value
        .parse()
        .map_err(|_| GroveError::failure("grove.version must be an ASCII u32"))
}

pub fn write(runner: &dyn GitRunner, grove: &Grove, metadata: &Metadata) -> Result<()> {
    ensure_supported(metadata)?;

    if let Some(version) = metadata.version {
        set(
            runner,
            grove,
            "grove.version",
            version.to_string().as_bytes(),
        )?;
    }
    if let Some(default_branch) = &metadata.default_branch {
        set(
            runner,
            grove,
            "grove.defaultBranch",
            default_branch.as_ref(),
        )?;
    }
    if let Some(remote) = &metadata.remote {
        set(runner, grove, "grove.remote", remote.as_ref())?;
    }
    set(
        runner,
        grove,
        "grove.publishState",
        metadata.publish_state.as_str().as_bytes(),
    )
}

pub fn read(runner: &dyn GitRunner, grove: &Grove) -> Result<Metadata> {
    let version = match get(runner, grove, "grove.version")? {
        Some(value) => Some(parse_version(value.as_ref())?),
        None => None,
    };
    let default_branch = get(runner, grove, "grove.defaultBranch")?;
    let remote = get(runner, grove, "grove.remote")?;
    let publish_state = match get(runner, grove, "grove.publishState")? {
        Some(value) => PublishState::parse(value)?,
        None => PublishState::Unpublished,
    };

    Ok(Metadata {
        version,
        default_branch,
        remote,
        publish_state,
    })
}

pub fn ensure_supported(metadata: &Metadata) -> Result<()> {
    match metadata.version {
        Some(version) if version > FORMAT_VERSION => Err(GroveError::needs_decision(format!(
            "this grove uses layout version {version}, newer than this git-grove supports ({FORMAT_VERSION})"
        ))
        .with_detail("upgrade git-grove before changing this grove")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use crate::error::ExitClass;
    use crate::git::runner::{GitOutput, RecordingFake};

    fn grove() -> Grove {
        Grove { root: "/g".into() }
    }

    #[test]
    fn writes_every_present_key_through_the_absolute_config_file() {
        let fake = RecordingFake::new();
        let metadata = Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(BString::from("main")),
            remote: Some(BString::from("origin")),
            publish_state: PublishState::Published,
        };

        write(&fake, &grove(), &metadata).unwrap();

        let calls: Vec<Vec<String>> = fake
            .calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect();
        assert!(calls.iter().any(|call| {
            call == &vec!["config", "--file", "/g/.bare/config", "grove.version", "1"]
        }));
        assert!(calls.iter().any(|call| {
            call == &vec![
                "config",
                "--file",
                "/g/.bare/config",
                "grove.defaultBranch",
                "main",
            ]
        }));
        assert!(calls.iter().any(|call| {
            call == &vec![
                "config",
                "--file",
                "/g/.bare/config",
                "grove.remote",
                "origin",
            ]
        }));
        assert!(calls.iter().any(|call| {
            call == &vec![
                "config",
                "--file",
                "/g/.bare/config",
                "grove.publishState",
                "published",
            ]
        }));
    }

    #[test]
    fn writes_non_utf8_branch_bytes_without_lossy_conversion() {
        let fake = RecordingFake::new();
        let metadata = Metadata {
            version: None,
            default_branch: Some(BString::from(vec![b'm', b'a', b'i', b'n', b'-', 0xff])),
            remote: None,
            publish_state: PublishState::Unpublished,
        };

        write(&fake, &grove(), &metadata).unwrap();

        let call = fake
            .calls()
            .into_iter()
            .find(|call| call.argv_os()[3] == "grove.defaultBranch")
            .unwrap();
        assert_eq!(
            call.argv_os()[4],
            OsString::from_vec(vec![b'm', b'a', b'i', b'n', b'-', 0xff])
        );
    }

    #[test]
    fn refuses_a_newer_layout_version_before_writing() {
        let fake = RecordingFake::new();
        let metadata = Metadata {
            version: Some(FORMAT_VERSION + 1),
            default_branch: None,
            remote: None,
            publish_state: PublishState::Unpublished,
        };

        let error = write(&fake, &grove(), &metadata).unwrap_err();

        assert_eq!(error.class, ExitClass::NeedsDecision);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn treats_a_missing_version_as_legacy_and_allows_it() {
        let metadata = Metadata {
            version: None,
            default_branch: None,
            remote: None,
            publish_state: PublishState::Unpublished,
        };

        assert!(ensure_supported(&metadata).is_ok());
    }

    #[test]
    fn reads_values_back_without_losing_non_utf8_name_bytes() {
        let fake = RecordingFake::new();
        for value in [
            b"1\n".as_slice(),
            b"main-\xff\n",
            b"origin-\xfe\n",
            b"published\n",
        ] {
            fake.push_response(GitOutput {
                status: 0,
                stdout: value.to_vec(),
                stderr: Vec::new(),
            });
        }

        let metadata = read(&fake, &grove()).unwrap();

        assert_eq!(metadata.version, Some(1));
        assert_eq!(
            metadata.default_branch,
            Some(BString::from(vec![b'm', b'a', b'i', b'n', b'-', 0xff]))
        );
        assert_eq!(
            metadata.remote,
            Some(BString::from(vec![
                b'o', b'r', b'i', b'g', b'i', b'n', b'-', 0xfe
            ]))
        );
        assert_eq!(metadata.publish_state, PublishState::Published);
    }

    #[test]
    fn rejects_a_malformed_version() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 0,
            stdout: b"one\n".to_vec(),
            stderr: Vec::new(),
        });

        let error = read(&fake, &grove()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert_eq!(error.message, "grove.version must be an ASCII u32");
    }

    #[test]
    fn propagates_an_unexpected_config_read_failure_with_escaped_stderr() {
        let fake = RecordingFake::new();
        fake.push_response(GitOutput {
            status: 3,
            stdout: Vec::new(),
            stderr: b"cannot read config: \xff\n".to_vec(),
        });

        let error = read(&fake, &grove()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("grove.version"));
        assert!(error.message.contains("exit status 3"));
        assert_eq!(
            error.detail.as_deref(),
            Some(r"cannot\x20read\x20config:\x20\xFF\n")
        );
    }

    #[test]
    fn rejects_an_unknown_publish_state() {
        let fake = RecordingFake::new();
        for value in [b"1\n".as_slice(), b"main\n", b"origin\n", b"queued\n"] {
            fake.push_response(GitOutput {
                status: 0,
                stdout: value.to_vec(),
                stderr: Vec::new(),
            });
        }

        let error = read(&fake, &grove()).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("publish state"));
    }
}
