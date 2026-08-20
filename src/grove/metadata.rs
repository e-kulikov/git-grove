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
    pub publish_remote: Option<BString>,
    pub publish_url: Option<BString>,
}

/// The durable record of a publication: which remote name and which URL the
/// transaction committed to. Both halves are raw bytes and are never parsed,
/// normalised, or compared as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub remote: BString,
    pub url: BString,
}

/// Read the publication receipt out of already-read metadata.
///
/// `Ok(None)` means **no receipt is recorded**, not "this grove is not
/// published": a caller that wants publication status must read
/// `metadata.publish_state` alongside this. The two cases that carry no receipt
/// are an `unpublished` grove and a grove created by a pre-0.3 `clone`.
///
/// The shapes that are a `Failure` are the ones no writer that has ever existed
/// could have produced, so they are a bug rather than a decision a user can
/// resolve:
///
/// - either receipt key set while the state is `unpublished`;
/// - exactly one of the two keys set — no version wrote one without the other;
/// - `publishing` with neither key — the `publishing` state is invented by this
///   release, and this release never writes it without a receipt.
///
/// `published` with neither key is **not** in that list. `git grove clone` has
/// written exactly that shape since 0.1, it is correct per the specification's
/// `## adopt` step 8 (`publishState=unpublished` only when there is no remote),
/// and shipped v0.2.0 groves of that shape exist in users' filesystems. Since
/// `FORMAT_VERSION` stays `1` here they are indistinguishable from groves this
/// release creates, so the allowance is mandatory. It may be dropped at a
/// `grove.version` bump with a migration, not before.
pub fn receipt(metadata: &Metadata) -> Result<Option<Receipt>> {
    match (
        metadata.publish_state,
        &metadata.publish_remote,
        &metadata.publish_url,
    ) {
        (PublishState::Unpublished, None, None) => Ok(None),
        (PublishState::Unpublished, _, _) => Err(GroveError::failure(
            "this grove records no publication but carries a publication receipt",
        )
        .with_detail(
            "grove.publishRemote or grove.publishUrl is set while grove.publishState is unpublished",
        )),
        (_, Some(remote), Some(url)) => Ok(Some(Receipt {
            remote: remote.clone(),
            url: url.clone(),
        })),
        (PublishState::Published, None, None) => Ok(None),
        (state, _, _) => Err(GroveError::failure(format!(
            "this grove records publish state {} but its publication receipt is incomplete",
            state.as_str()
        ))
        .with_detail("both grove.publishRemote and grove.publishUrl must be present")),
    }
}

/// Write the publication receipt in one step: state first, then the remote
/// name, then the URL.
pub fn write_receipt(
    runner: &dyn GitRunner,
    grove: &Grove,
    state: PublishState,
    receipt: &Receipt,
) -> Result<()> {
    set(
        runner,
        grove,
        "grove.publishState",
        state.as_str().as_bytes(),
    )?;
    set(
        runner,
        grove,
        "grove.publishRemote",
        receipt.remote.as_ref(),
    )?;
    set(runner, grove, "grove.publishUrl", receipt.url.as_ref())
}

fn config_path(grove: &Grove) -> PathBuf {
    grove.bare_dir().join("config")
}

fn set(runner: &dyn GitRunner, grove: &Grove, key: &str, value: &[u8]) -> Result<()> {
    let config_path = config_path(grove);
    set_at(runner, &config_path, key, value)
}

fn set_at(
    runner: &dyn GitRunner,
    config_path: &std::path::Path,
    key: &str,
    value: &[u8],
) -> Result<()> {
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
    write_to_config(runner, &config_path(grove), metadata)
}

pub fn write_to_config(
    runner: &dyn GitRunner,
    config_path: &std::path::Path,
    metadata: &Metadata,
) -> Result<()> {
    ensure_supported(metadata)?;

    if let Some(version) = metadata.version {
        set_at(
            runner,
            config_path,
            "grove.version",
            version.to_string().as_bytes(),
        )?;
    }
    if let Some(default_branch) = &metadata.default_branch {
        set_at(
            runner,
            config_path,
            "grove.defaultBranch",
            default_branch.as_ref(),
        )?;
    }
    if let Some(remote) = &metadata.remote {
        set_at(runner, config_path, "grove.remote", remote.as_ref())?;
    }
    set_at(
        runner,
        config_path,
        "grove.publishState",
        metadata.publish_state.as_str().as_bytes(),
    )?;
    if let Some(publish_remote) = &metadata.publish_remote {
        set_at(
            runner,
            config_path,
            "grove.publishRemote",
            publish_remote.as_ref(),
        )?;
    }
    if let Some(publish_url) = &metadata.publish_url {
        set_at(
            runner,
            config_path,
            "grove.publishUrl",
            publish_url.as_ref(),
        )?;
    }
    Ok(())
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
    let publish_remote = get(runner, grove, "grove.publishRemote")?;
    let publish_url = get(runner, grove, "grove.publishUrl")?;

    Ok(Metadata {
        version,
        default_branch,
        remote,
        publish_state,
        publish_remote,
        publish_url,
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

    /// The pinned absolute configuration file of the grove above.
    const CONFIG: &str = "/g/.bare/config";

    #[test]
    fn writes_every_present_key_through_the_absolute_config_file() {
        let fake = RecordingFake::new();
        let metadata = Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(BString::from("main")),
            remote: Some(BString::from("origin")),
            publish_state: PublishState::Published,
            publish_remote: None,
            publish_url: None,
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
            publish_remote: None,
            publish_url: None,
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
            publish_remote: None,
            publish_url: None,
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
            publish_remote: None,
            publish_url: None,
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

    #[test]
    fn reads_the_receipt_keys_back_without_losing_non_utf8_bytes() {
        let fake = RecordingFake::new();
        for value in [
            b"1\n".as_slice(),
            b"main\n",
            b"origin\n",
            b"publishing\n",
            b"origin-\xfd\n",
            b"https://example.invalid/r-\xfc.git\n",
        ] {
            fake.push_response(GitOutput {
                status: 0,
                stdout: value.to_vec(),
                stderr: Vec::new(),
            });
        }

        let metadata = read(&fake, &grove()).unwrap();

        assert_eq!(
            metadata.publish_remote,
            Some(BString::from(vec![
                b'o', b'r', b'i', b'g', b'i', b'n', b'-', 0xfd
            ]))
        );
        let mut url = b"https://example.invalid/r-".to_vec();
        url.push(0xfc);
        url.extend_from_slice(b".git");
        assert_eq!(metadata.publish_url, Some(BString::from(url)));
    }

    #[test]
    fn reports_absent_receipt_keys_as_none() {
        let fake = RecordingFake::new();
        for value in [b"1\n".as_slice(), b"main\n", b"origin\n", b"unpublished\n"] {
            fake.push_response(GitOutput {
                status: 0,
                stdout: value.to_vec(),
                stderr: Vec::new(),
            });
        }
        for _ in 0..2 {
            fake.push_response(GitOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }

        let metadata = read(&fake, &grove()).unwrap();

        assert_eq!(metadata.publish_remote, None);
        assert_eq!(metadata.publish_url, None);
    }

    #[test]
    fn writes_the_receipt_keys_when_present() {
        let fake = RecordingFake::new();
        let metadata = Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(BString::from("main")),
            remote: Some(BString::from("origin")),
            publish_state: PublishState::Published,
            publish_remote: Some(BString::from("origin")),
            publish_url: Some(BString::from("https://example.invalid/r.git")),
        };

        write(&fake, &grove(), &metadata).unwrap();

        let calls: Vec<Vec<String>> = fake
            .calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect();
        assert!(calls.iter().any(|call| {
            call == &vec!["config", "--file", CONFIG, "grove.publishRemote", "origin"]
        }));
        assert!(calls.iter().any(|call| {
            call == &vec![
                "config",
                "--file",
                CONFIG,
                "grove.publishUrl",
                "https://example.invalid/r.git",
            ]
        }));
    }

    #[test]
    fn write_receipt_emits_exactly_three_writes_with_the_state_first() {
        let fake = RecordingFake::new();
        let receipt = Receipt {
            remote: BString::from("origin"),
            url: BString::from("https://example.invalid/r.git"),
        };

        write_receipt(&fake, &grove(), PublishState::Publishing, &receipt).unwrap();

        let calls: Vec<Vec<String>> = fake
            .calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect();
        assert_eq!(
            calls,
            vec![
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "grove.publishState",
                    "publishing",
                ],
                vec!["config", "--file", CONFIG, "grove.publishRemote", "origin",],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "grove.publishUrl",
                    "https://example.invalid/r.git",
                ],
            ]
        );
    }

    #[test]
    fn write_receipt_preserves_raw_url_and_remote_bytes() {
        let fake = RecordingFake::new();
        let receipt = Receipt {
            remote: BString::from(vec![b'o', 0xfd]),
            url: BString::from(vec![b'u', 0xfc]),
        };

        write_receipt(&fake, &grove(), PublishState::Published, &receipt).unwrap();

        let calls = fake.calls();
        assert_eq!(calls[1].argv_os()[4], OsString::from_vec(vec![b'o', 0xfd]));
        assert_eq!(calls[2].argv_os()[4], OsString::from_vec(vec![b'u', 0xfc]));
    }

    fn metadata_with(
        publish_state: PublishState,
        publish_remote: Option<&[u8]>,
        publish_url: Option<&[u8]>,
    ) -> Metadata {
        Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(BString::from("main")),
            remote: None,
            publish_state,
            publish_remote: publish_remote.map(BString::from),
            publish_url: publish_url.map(BString::from),
        }
    }

    #[test]
    fn an_unpublished_grove_without_receipt_keys_has_no_receipt() {
        let metadata = metadata_with(PublishState::Unpublished, None, None);

        assert_eq!(receipt(&metadata).unwrap(), None);
    }

    #[test]
    fn an_unpublished_grove_carrying_receipt_keys_is_a_failure() {
        for (remote, url) in [
            (Some(b"origin".as_slice()), Some(b"u".as_slice())),
            (Some(b"origin".as_slice()), None),
            (None, Some(b"u".as_slice())),
        ] {
            let metadata = metadata_with(PublishState::Unpublished, remote, url);

            let error = receipt(&metadata).unwrap_err();

            assert_eq!(error.class, ExitClass::Failure);
            assert!(error.message.contains("receipt"));
        }
    }

    #[test]
    fn an_in_flight_grove_with_both_keys_yields_the_receipt() {
        for state in [PublishState::Publishing, PublishState::Published] {
            let metadata = metadata_with(
                state,
                Some(b"origin"),
                Some(b"https://example.invalid/r.git"),
            );

            assert_eq!(
                receipt(&metadata).unwrap(),
                Some(Receipt {
                    remote: BString::from("origin"),
                    url: BString::from("https://example.invalid/r.git"),
                })
            );
        }
    }

    #[test]
    fn a_half_written_receipt_is_a_failure_not_a_decision() {
        for state in [PublishState::Publishing, PublishState::Published] {
            for (remote, url) in [
                (Some(b"origin".as_slice()), None),
                (None, Some(b"u".as_slice())),
            ] {
                let metadata = metadata_with(state, remote, url);

                let error = receipt(&metadata).unwrap_err();

                assert_eq!(error.class, ExitClass::Failure);
                assert!(error.message.contains("receipt"));
            }
        }
    }

    /// `publishing` is invented by 0.3, and 0.3 never writes it without a
    /// receipt, so this shape is unreachable by any writer that has existed.
    #[test]
    fn publishing_without_a_receipt_is_a_failure() {
        let metadata = metadata_with(PublishState::Publishing, None, None);

        let error = receipt(&metadata).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
        assert!(error.message.contains("receipt"));
    }

    /// `git grove clone` has written `published` with no receipt keys since
    /// 0.1 (`src/commands/clone.rs`), v0.2.0 shipped that shape, and
    /// `FORMAT_VERSION` stays `1` here — so such groves are indistinguishable
    /// from ones this release creates and must not be read as torn.
    #[test]
    fn a_pre_0_3_cloned_grove_has_no_receipt_and_that_is_not_torn() {
        let metadata = metadata_with(PublishState::Published, None, None);

        assert_eq!(receipt(&metadata).unwrap(), None);
    }
}
