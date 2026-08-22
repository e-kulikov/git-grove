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
    /// Strictly before a remote exists at all: the hosting-side repository
    /// creation `publish --create` requested has been requested, but this
    /// grove has not yet been handed off to the classic `publishing`
    /// transaction that `publish <url>` also uses.
    Creating,
    Publishing,
    Published,
}

impl PublishState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished",
            Self::Creating => "creating",
            Self::Publishing => "publishing",
            Self::Published => "published",
        }
    }

    pub fn parse(value: impl AsRef<[u8]>) -> Result<Self> {
        match value.as_ref() {
            b"unpublished" => Ok(Self::Unpublished),
            b"creating" => Ok(Self::Creating),
            b"publishing" => Ok(Self::Publishing),
            b"published" => Ok(Self::Published),
            _ => Err(GroveError::failure(
                "grove publish state must be unpublished, creating, publishing, or published",
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
    /// The `--create` provider/owner/name a grove's remote was created
    /// through. Present for `creating`, and, once written, never cleared
    /// again for `publishing`/`published` — that persistence is what lets a
    /// later `--create` rerun recognise its own prior success without a
    /// provider round trip. Absent for a grove published by a bare
    /// `publish <url>`.
    pub publish_provider: Option<BString>,
    pub publish_owner: Option<BString>,
    pub publish_name: Option<BString>,
}

/// The durable record of a `--create` request in flight: which provider,
/// owner, name, and remote name it targets. A separate, narrower accessor
/// from [`receipt`] — a `creating` grove has no classic URL yet, which is a
/// legitimate absence, not an incomplete classic receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatingReceipt {
    pub provider: BString,
    pub owner: BString,
    pub name: BString,
    pub remote: BString,
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
        // A `creating` grove has no classic URL yet — its own receipt is read
        // through `creating_receipt`, not this one. This is a legitimate
        // absence, not the "incomplete receipt" `Failure` below.
        (PublishState::Creating, Some(_), None) => Ok(None),
        // `creating` forbids `publishUrl` outright (Decision 1): a `creating`
        // grove that already carries one — with or without `publishRemote` —
        // is torn, not a classic receipt to reinterpret. Checked ahead of the
        // wildcard arm below, which would otherwise read it as one.
        (PublishState::Creating, _, Some(_)) => Err(GroveError::failure(
            "this grove's publish state is creating but it already carries a completed publication URL",
        )
        .with_detail("grove.publishUrl must never be set while grove.publishState is creating")),
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

/// Read the `--create` creation receipt out of already-read metadata. See
/// [`CreatingReceipt`] for what it carries.
///
/// `Ok(None)` means "this grove's remote, if any, was not created through
/// `--create`" — an `unpublished` grove with none of the three keys, or a
/// `publishing`/`published` grove published by a bare `publish <url>`. Every
/// other shape either fully describes a `--create` origin or is a `Failure`:
/// no version this tool has ever shipped writes a partial three-key set
/// (`publishRemote` is shared with the classic receipt and is checked
/// alongside them, since `creating` has no other use for it and
/// `publishing`/`published` already require it for the classic receipt).
pub fn creating_receipt(metadata: &Metadata) -> Result<Option<CreatingReceipt>> {
    let incomplete = || {
        Err(GroveError::failure(format!(
            "this grove records publish state {} but its creation receipt is incomplete",
            metadata.publish_state.as_str()
        ))
        .with_detail(
            "grove.publishProvider, grove.publishOwner, grove.publishName, and grove.publishRemote must all be present, or all absent",
        ))
    };

    let all_absent = metadata.publish_provider.is_none()
        && metadata.publish_owner.is_none()
        && metadata.publish_name.is_none();
    let complete = match (
        &metadata.publish_provider,
        &metadata.publish_owner,
        &metadata.publish_name,
        &metadata.publish_remote,
    ) {
        (Some(provider), Some(owner), Some(name), Some(remote)) => Some(CreatingReceipt {
            provider: provider.clone(),
            owner: owner.clone(),
            name: name.clone(),
            remote: remote.clone(),
        }),
        _ => None,
    };

    match metadata.publish_state {
        PublishState::Unpublished => {
            if all_absent {
                Ok(None)
            } else {
                incomplete()
            }
        }
        PublishState::Creating => match complete {
            Some(_) if metadata.publish_url.is_some() => incomplete(),
            Some(receipt) => Ok(Some(receipt)),
            None => incomplete(),
        },
        PublishState::Publishing | PublishState::Published => {
            if all_absent {
                Ok(None)
            } else if let Some(receipt) = complete {
                Ok(Some(receipt))
            } else {
                incomplete()
            }
        }
    }
}

/// Write a fresh creation receipt in the order Decision 2 requires: state
/// first (`grove.publishState=creating`), then the four keys — mirroring
/// [`write_receipt`]'s own documented state-first order.
pub fn write_creating_receipt(
    runner: &dyn GitRunner,
    grove: &Grove,
    provider: &BString,
    owner: &BString,
    name: &BString,
    remote: &BString,
) -> Result<()> {
    set(
        runner,
        grove,
        "grove.publishState",
        PublishState::Creating.as_str().as_bytes(),
    )?;
    set(runner, grove, "grove.publishProvider", provider.as_ref())?;
    set(runner, grove, "grove.publishOwner", owner.as_ref())?;
    set(runner, grove, "grove.publishName", name.as_ref())?;
    set(runner, grove, "grove.publishRemote", remote.as_ref())
}

/// Roll a creation receipt back to `unpublished`, in the order Decision 2
/// requires: the four keys cleared first, `grove.publishState=unpublished`
/// written last — inverted from [`write_creating_receipt`] on purpose, so
/// `publishRemote` (shared with the classic receipt) never sits next to
/// `publishState=unpublished`, which is exactly the existing "records no
/// publication but carries a receipt" hard failure. Idempotent: clearing an
/// already-absent key is not an error, so this is safe to call on a grove
/// whose creation receipt is already partial or absent.
pub fn rollback_creating_receipt(runner: &dyn GitRunner, grove: &Grove) -> Result<()> {
    unset(runner, grove, "grove.publishProvider")?;
    unset(runner, grove, "grove.publishOwner")?;
    unset(runner, grove, "grove.publishName")?;
    unset(runner, grove, "grove.publishRemote")?;
    set(
        runner,
        grove,
        "grove.publishState",
        PublishState::Unpublished.as_str().as_bytes(),
    )
}

fn unset(runner: &dyn GitRunner, grove: &Grove, key: &str) -> Result<()> {
    let config_path = config_path(grove);
    let output = runner.run(Invocation::new().args([
        OsStr::new("config"),
        OsStr::new("--file"),
        config_path.as_os_str(),
        OsStr::new("--unset-all"),
        OsStr::new(key),
    ]))?;
    // Exit 5 is "the key does not exist", which is the state this asks for.
    if output.ok() || output.status == 5 {
        Ok(())
    } else {
        Err(
            GroveError::failure(format!("cannot clear {key} in the grove configuration"))
                .with_detail(output.stderr.as_slice().escape_bytes().to_string()),
        )
    }
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
    if let Some(publish_provider) = &metadata.publish_provider {
        set_at(
            runner,
            config_path,
            "grove.publishProvider",
            publish_provider.as_ref(),
        )?;
    }
    if let Some(publish_owner) = &metadata.publish_owner {
        set_at(
            runner,
            config_path,
            "grove.publishOwner",
            publish_owner.as_ref(),
        )?;
    }
    if let Some(publish_name) = &metadata.publish_name {
        set_at(
            runner,
            config_path,
            "grove.publishName",
            publish_name.as_ref(),
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
    let publish_provider = get(runner, grove, "grove.publishProvider")?;
    let publish_owner = get(runner, grove, "grove.publishOwner")?;
    let publish_name = get(runner, grove, "grove.publishName")?;

    Ok(Metadata {
        version,
        default_branch,
        remote,
        publish_state,
        publish_remote,
        publish_url,
        publish_provider,
        publish_owner,
        publish_name,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
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
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
        }
    }

    /// Like [`metadata_with`], with the three `--create` keys also settable,
    /// for the `creating_receipt` matrix below.
    #[allow(clippy::too_many_arguments)]
    fn creating_metadata_with(
        publish_state: PublishState,
        publish_provider: Option<&[u8]>,
        publish_owner: Option<&[u8]>,
        publish_name: Option<&[u8]>,
        publish_remote: Option<&[u8]>,
        publish_url: Option<&[u8]>,
    ) -> Metadata {
        Metadata {
            publish_provider: publish_provider.map(BString::from),
            publish_owner: publish_owner.map(BString::from),
            publish_name: publish_name.map(BString::from),
            ..metadata_with(publish_state, publish_remote, publish_url)
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

    // ---- the `creating` state --------------------------------------------

    #[test]
    fn creating_state_round_trips_through_as_str_and_parse() {
        assert_eq!(PublishState::Creating.as_str(), "creating");
        assert_eq!(
            PublishState::parse(b"creating").unwrap(),
            PublishState::Creating
        );
    }

    #[test]
    fn classic_receipt_returns_none_for_a_creating_grove_with_no_classic_url() {
        let metadata = creating_metadata_with(
            PublishState::Creating,
            Some(b"github"),
            Some(b"acme"),
            Some(b"widgets"),
            Some(b"origin"),
            None,
        );

        assert_eq!(receipt(&metadata).unwrap(), None);
    }

    /// Regression test for a real, reachable panic (found by Copilot review
    /// against PR #5, verified independently): `creating` forbids
    /// `publishUrl` outright, but before this fix `receipt()`'s wildcard arm
    /// silently read a `creating` grove that also carried one as an ordinary
    /// classic receipt, which then drove `accept_rerun` into an
    /// `unreachable!()`. This must be a `Failure`, never `Ok(Some(_))`,
    /// regardless of whether `publishRemote` is also present.
    #[test]
    fn a_creating_grove_carrying_a_classic_url_is_a_failure_not_a_silent_classic_receipt() {
        for remote in [Some(b"origin".as_slice()), None] {
            let metadata = creating_metadata_with(
                PublishState::Creating,
                Some(b"github"),
                Some(b"acme"),
                Some(b"widgets"),
                remote,
                Some(b"https://example.invalid/r.git"),
            );

            let error = receipt(&metadata).unwrap_err();

            assert_eq!(error.class, ExitClass::Failure, "remote={remote:?}");
        }
    }

    #[test]
    fn every_other_classic_receipt_case_is_unchanged_by_the_creating_arm() {
        // The existing test module above this one exercises `receipt()`
        // exhaustively for every state other than `Creating` and is run
        // unmodified as part of this same suite; this test only pins the one
        // case that could plausibly have been disturbed by an insertion
        // ahead of the wildcard receipt arm: a fully-populated classic
        // receipt on a non-`Creating` state still resolves as before.
        let metadata = metadata_with(PublishState::Published, Some(b"origin"), Some(b"u"));

        assert_eq!(
            receipt(&metadata).unwrap(),
            Some(Receipt {
                remote: BString::from("origin"),
                url: BString::from("u"),
            })
        );
    }

    // ---- `creating_receipt` ----------------------------------------------

    #[test]
    fn an_unpublished_grove_without_any_creating_key_has_no_creating_receipt() {
        let metadata = metadata_with(PublishState::Unpublished, None, None);

        assert_eq!(creating_receipt(&metadata).unwrap(), None);
    }

    #[test]
    fn an_unpublished_grove_carrying_any_creating_key_is_a_failure() {
        for (provider, owner, name) in [
            (Some(b"github".as_slice()), None, None),
            (None, Some(b"acme".as_slice()), None),
            (None, None, Some(b"widgets".as_slice())),
        ] {
            let metadata = creating_metadata_with(
                PublishState::Unpublished,
                provider,
                owner,
                name,
                None,
                None,
            );

            let error = creating_receipt(&metadata).unwrap_err();

            assert_eq!(error.class, ExitClass::Failure);
        }
    }

    #[test]
    fn a_creating_grove_with_all_four_keys_and_no_url_yields_the_creating_receipt() {
        let metadata = creating_metadata_with(
            PublishState::Creating,
            Some(b"github"),
            Some(b"acme"),
            Some(b"widgets"),
            Some(b"origin"),
            None,
        );

        assert_eq!(
            creating_receipt(&metadata).unwrap(),
            Some(CreatingReceipt {
                provider: BString::from("github"),
                owner: BString::from("acme"),
                name: BString::from("widgets"),
                remote: BString::from("origin"),
            })
        );
    }

    #[test]
    fn a_creating_grove_with_a_classic_url_is_a_failure() {
        let metadata = creating_metadata_with(
            PublishState::Creating,
            Some(b"github"),
            Some(b"acme"),
            Some(b"widgets"),
            Some(b"origin"),
            Some(b"https://example.invalid/r.git"),
        );

        let error = creating_receipt(&metadata).unwrap_err();

        assert_eq!(error.class, ExitClass::Failure);
    }

    #[test]
    fn a_creating_grove_with_an_incomplete_four_key_set_is_a_failure_in_every_position() {
        let full = (
            Some(b"github".as_slice()),
            Some(b"acme".as_slice()),
            Some(b"widgets".as_slice()),
            Some(b"origin".as_slice()),
        );
        for missing in 0..4 {
            let mut fields = full;
            match missing {
                0 => fields.0 = None,
                1 => fields.1 = None,
                2 => fields.2 = None,
                _ => fields.3 = None,
            }
            let metadata = creating_metadata_with(
                PublishState::Creating,
                fields.0,
                fields.1,
                fields.2,
                fields.3,
                None,
            );

            let error = creating_receipt(&metadata).unwrap_err();

            assert_eq!(
                error.class,
                ExitClass::Failure,
                "missing position {missing}"
            );
        }
    }

    #[test]
    fn publishing_and_published_groves_created_through_create_carry_the_creating_receipt() {
        for state in [PublishState::Publishing, PublishState::Published] {
            let metadata = creating_metadata_with(
                state,
                Some(b"github"),
                Some(b"acme"),
                Some(b"widgets"),
                Some(b"origin"),
                Some(b"https://example.invalid/r.git"),
            );

            assert_eq!(
                creating_receipt(&metadata).unwrap(),
                Some(CreatingReceipt {
                    provider: BString::from("github"),
                    owner: BString::from("acme"),
                    name: BString::from("widgets"),
                    remote: BString::from("origin"),
                })
            );
        }
    }

    #[test]
    fn publishing_and_published_groves_created_by_a_bare_publish_have_no_creating_receipt() {
        for state in [PublishState::Publishing, PublishState::Published] {
            let metadata = metadata_with(
                state,
                Some(b"origin"),
                Some(b"https://example.invalid/r.git"),
            );

            assert_eq!(creating_receipt(&metadata).unwrap(), None);
        }
    }

    #[test]
    fn publishing_and_published_groves_with_a_partial_creating_key_set_are_a_failure() {
        for state in [PublishState::Publishing, PublishState::Published] {
            let metadata = creating_metadata_with(
                state,
                Some(b"github"),
                None,
                Some(b"widgets"),
                Some(b"origin"),
                Some(b"https://example.invalid/r.git"),
            );

            let error = creating_receipt(&metadata).unwrap_err();

            assert_eq!(error.class, ExitClass::Failure);
        }
    }

    // ---- writing and rolling back the creating receipt --------------------

    #[test]
    fn write_creating_receipt_writes_state_first_then_the_four_keys_in_order() {
        let fake = RecordingFake::new();

        write_creating_receipt(
            &fake,
            &grove(),
            &BString::from("github"),
            &BString::from("acme"),
            &BString::from("widgets"),
            &BString::from("origin"),
        )
        .unwrap();

        let calls: Vec<Vec<String>> = fake
            .calls()
            .iter()
            .map(|call| call.argv_for_test())
            .collect();
        assert_eq!(
            calls,
            vec![
                vec!["config", "--file", CONFIG, "grove.publishState", "creating"],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "grove.publishProvider",
                    "github"
                ],
                vec!["config", "--file", CONFIG, "grove.publishOwner", "acme"],
                vec!["config", "--file", CONFIG, "grove.publishName", "widgets"],
                vec!["config", "--file", CONFIG, "grove.publishRemote", "origin"],
            ]
        );
    }

    #[test]
    fn rollback_creating_receipt_clears_the_four_keys_before_writing_unpublished_last() {
        let fake = RecordingFake::new();

        rollback_creating_receipt(&fake, &grove()).unwrap();

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
                    "--unset-all",
                    "grove.publishProvider",
                ],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "--unset-all",
                    "grove.publishOwner",
                ],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "--unset-all",
                    "grove.publishName",
                ],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "--unset-all",
                    "grove.publishRemote",
                ],
                vec![
                    "config",
                    "--file",
                    CONFIG,
                    "grove.publishState",
                    "unpublished",
                ],
            ]
        );
    }

    #[test]
    fn rollback_creating_receipt_is_idempotent_on_an_absent_or_partial_receipt() {
        let fake = RecordingFake::new();
        // Exit 5 from `--unset-all` is "the key does not exist".
        for _ in 0..4 {
            fake.push_response(GitOutput {
                status: 5,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }

        rollback_creating_receipt(&fake, &grove()).unwrap();
    }

    #[test]
    fn write_receipt_on_a_creating_grove_leaves_the_three_new_keys_untouched() {
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
        assert!(!calls.iter().any(|call| call
            .iter()
            .any(|arg| arg.starts_with("grove.publishProvider")
                || arg.starts_with("grove.publishOwner")
                || arg.starts_with("grove.publishName"))));
    }
}
