//! Steery's `daytona` adapter. It provides `hosts` (PROTOCOL.md §3.2, §5.2) on
//! Daytona, through Daytona's published REST API. Daytona's own words for a host (its API
//! paths) stay inside [`api`], which names the items around them by Steery's.
//!
//! Its client is built from the settings `initialize` passes, and from nothing else: the
//! host, its image, credentials and network are the company's and Daytona's. The one
//! environment a host is given is the person's own `DAYTONA_ENV`, passed to Daytona as it
//! is; the adapter adds no image, environment, secret or network of its own, and Daytona's
//! auto-stop stays the host's idle timer.

mod api;

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use steery_sdk::wire::{self, hosts};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal};

use api::{BuildInfo, Create, Daytona, Deadline, Env, Fault};

/// The label every host this adapter makes carries, and every list is filtered by.
const MINE: (&str, &str) = ("steery", "host");
/// The label that finds a host again by the runtime's key.
const KEY_LABEL: &str = "steery-key";
/// The host Daytona's SSH access docs name, where its answer names none.
const SSH_HOST: &str = "ssh.app.daytona.io";
/// How long a channel's SSH access token lives, in minutes.
const SSH_MINUTES: u32 = 60;
/// Seconds the adapter waits for Daytona, inside the runtime's bounds (§9: 300 s, 60 s).
const LONG_SECONDS: u64 = 280;
const CHANGE_SECONDS: u64 = 55;
/// Seconds for one file call in the host after an upload: a size check, a chmod, a removal.
const FILE_SECONDS: Duration = Duration::from_secs(5);
/// How often a host changing state is read again; an archive takes minutes.
const POLL: Duration = Duration::from_secs(1);
const ARCHIVE_POLL: Duration = Duration::from_secs(2);
/// The states a host that is being changed fails in.
const FAILED: &[&str] = &["error", "build_failed"];
/// The states of a host whose image Daytona still builds from a Dockerfile.
const BUILDING: &[&str] = &["pending_build", "building_snapshot"];
/// The longest line a refusal carries.
const LINE_CHARS: usize = 300;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    steery_sdk::serve(|_notifier| Adapter::default()).await
}

/// The adapter: a Daytona client once `initialize` gave an API key, and none before; and
/// whether this computer has the OpenSSH client its channels run.
#[derive(Default)]
struct Adapter {
    daytona: OnceLock<Option<Daytona>>,
    ssh: OnceLock<bool>,
}

/// Why no channel can be had here: Daytona's runs `ssh` on the controlling computer, and
/// waiting does not install one. Said by `initialize` as `unavailable` (PROTOCOL.md §3.2),
/// so the machine is never added where it cannot work, and by `hosts/channel` and
/// `hosts/list` as the guard where it is used.
const NO_SSH: &str = "Daytona's channel runs `ssh` on this computer, and none is installed: install an OpenSSH client (e.g. apt install openssh-client)";

impl Adapter {
    /// `ssh` is on `PATH`, as found when the adapter was initialized.
    fn reach(&self) -> Result<(), Error> {
        let found = *self
            .ssh
            .get_or_init(|| std::env::var_os("PATH").is_some_and(|path| ssh_within(&path)));
        found
            .then_some(())
            .ok_or_else(|| Error::refused(refusal::REFUSED, NO_SSH))
    }
}

/// Whether a folder of `path` (as `PATH` spells it) holds an executable `ssh`.
fn ssh_within(path: &std::ffi::OsStr) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).any(|dir| {
        std::fs::metadata(dir.join("ssh"))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

impl Program for Adapter {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let given = |name: &str| params.settings.get(name).filter(|value| !value.is_empty());
        let env = match given("DAYTONA_ENV") {
            Some(lines) => env_of(lines).map_err(|why| Error::refused(refusal::REFUSED, why))?,
            None => Env::default(),
        };
        let daytona = match given("DAYTONA_API_KEY") {
            Some(key) => Some(
                Daytona::new(
                    key.clone(),
                    given("DAYTONA_API_URL").map(String::as_str),
                    given("DAYTONA_TARGET").cloned(),
                    env,
                )
                .map_err(|why| Error::refused(refusal::REFUSED, format!("Daytona: {why}")))?,
            ),
            None => None,
        };
        let _ = self.daytona.set(daytona);
        // Said up front (PROTOCOL.md §3.2), so no machine is added where its channel cannot
        // run; the channel and the list still refuse, as the guard where it is used.
        let unavailable = self.reach().err().map(|_| NO_SSH.to_owned());
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "daytona".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Daytona".into(),
                short: "Dt".into(),
                tint: None,
                mark: None,
            },
            harness: None,
            machine: Some(wire::Machine::Hosts(wire::HostFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::Activity,
                optional: vec![
                    wire::Optional::Archive,
                    wire::Optional::Keepalive,
                    wire::Optional::Expose,
                ],
                builds: true,
                channel: wire::Channel::Argv,
                os: wire::Os::Linux,
                unavailable,
            })),
            settings: vec![
                setting(
                    "DAYTONA_API_KEY",
                    true,
                    true,
                    "An API key from app.daytona.io/dashboard/keys",
                ),
                setting(
                    "DAYTONA_API_URL",
                    false,
                    false,
                    "Your Daytona API URL, for a self-hosted Daytona; absent: https://app.daytona.io/api",
                ),
                setting(
                    "DAYTONA_TARGET",
                    false,
                    false,
                    "The Daytona region hosts are made in (us, eu); absent: your organization's default",
                ),
                setting(
                    "DAYTONA_ENV",
                    true,
                    false,
                    "Environment for every sandbox, one NAME=VALUE per line (# comments); passed as Daytona's env",
                ),
            ],
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::{params as given, result};
        let daytona = || {
            self.daytona.get().and_then(Option::as_ref).ok_or_else(|| {
                Error::refused(
                    refusal::REFUSED,
                    "Daytona: no API key was given: give DAYTONA_API_KEY",
                )
            })
        };
        match method {
            method::HOSTS_LIST => {
                self.reach()?;
                result(list(daytona()?, given(params)?).await?)
            }
            method::HOSTS_ENSURE => result(ensure(daytona()?, given(params)?).await?),
            method::HOSTS_START => result(start(daytona()?, given(params)?).await?),
            method::HOSTS_STOP => result(stop(daytona()?, given(params)?).await?),
            method::HOSTS_DELETE => result(delete(daytona()?, given(params)?).await?),
            method::HOSTS_ARCHIVE => result(archive(daytona()?, given(params)?).await?),
            method::HOSTS_KEEPALIVE => result(keepalive(daytona()?, given(params)?).await?),
            method::HOSTS_EXPOSE => result(expose(daytona()?, given(params)?).await?),
            method::HOSTS_PUT => result(put(daytona()?, given(params)?).await?),
            method::HOSTS_CHANNEL => {
                self.reach()?;
                result(channel(daytona()?, given(params)?).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}

fn setting(name: &str, secret: bool, required: bool, about: &str) -> wire::Setting {
    wire::Setting {
        name: name.into(),
        secret,
        required,
        about: about.into(),
    }
}

/// The environment `DAYTONA_ENV` names: one `NAME=VALUE` per line, blank lines and `#`
/// lines skipped; the value is everything after the first `=`, as it is. A malformed line
/// is refused by its number, never its text, which may hold a secret.
fn env_of(lines: &str) -> Result<Env, String> {
    let mut env = BTreeMap::new();
    for (at, line) in lines.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = at + 1;
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| format!("DAYTONA_ENV line {number} has no `=`: write NAME=VALUE"))?;
        let named = name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !named {
            return Err(format!(
                "DAYTONA_ENV line {number}: a name is letters, digits and _, not starting with a digit"
            ));
        }
        env.insert(name.to_owned(), value.to_owned());
    }
    Ok(Env(env))
}

/// The refusal a fault is; `otherwise` where it is none of not found, key, down or time.
/// A refused key is `refused`, never `offline`: it does not heal by waiting, and its line
/// names the setting to give again. Its line never holds the API key.
fn refuse(daytona: &Daytona, fault: Fault, otherwise: &'static str) -> Error {
    let (word, line) = match fault {
        Fault::NotFound(line) => (refusal::NOT_FOUND, format!("Daytona: {line}")),
        Fault::KeyRefused { status, said } => (
            refusal::REFUSED,
            format!(
                "Daytona refused the API key (HTTP {status}: {said}): give DAYTONA_API_KEY again"
            ),
        ),
        Fault::Offline(line) => (refusal::OFFLINE, format!("Daytona: {line}")),
        // Daytona answered, and the host was not there in time.
        Fault::TimedOut(line) => (refusal::REFUSED, format!("Daytona: {line} (timed out)")),
        Fault::Conflict(line) | Fault::Failed(line) => (otherwise, format!("Daytona: {line}")),
    };
    Error::refused(word, one_line(daytona.key(), &line))
}

/// `text` on one line, without `key`, at most [`LINE_CHARS`] characters.
fn one_line(key: &str, text: &str) -> String {
    let text = text.replace(key, "[redacted]");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(LINE_CHARS - 1) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// A host's id, never empty.
fn named(id: &str) -> Result<&str, Error> {
    if id.is_empty() {
        Err(Error::invalid("host must be a non-empty string"))
    } else {
        Ok(id)
    }
}

/// Milliseconds since 1970 of a time Daytona writes (RFC 3339); 0 where it is none.
fn millis(at: Option<&str>) -> u64 {
    at.and_then(|at| {
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
    })
    .and_then(|at| u64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok())
    .unwrap_or(0)
}

/// One argument as a POSIX shell reads it back.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_/.:=@%+,-".contains(&byte));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

async fn list(daytona: &Daytona, params: hosts::List) -> Result<hosts::Listed, Error> {
    let mut labels = params.labels.unwrap_or_default();
    labels.insert(MINE.0.into(), MINE.1.into());
    let found = daytona
        .list(&labels, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    let hosts = found
        .into_iter()
        .map(|host| hosts::Host {
            state: host.state().to_owned(),
            created_at: millis(host.created_at.as_deref()),
            labels: host.labels.unwrap_or_default(),
            host: host.id,
        })
        .collect();
    Ok(hosts::Listed { hosts })
}

/// The host made for `key`, or one made for it now from the source asked, or from
/// Daytona's default snapshot, and started.
///
/// It is found by its name, which Daytona keeps unique and reads back at once; its listing
/// lags a new host by a second or more, so finding it by its labels would make a second
/// one for an ensure sent again in that time. Two ensures for one key make one host: the
/// later one's create is refused as a conflict and finds the other's.
async fn ensure(daytona: &Daytona, params: hosts::Ensure) -> Result<hosts::Named, Error> {
    if params.key.is_empty() {
        return Err(Error::invalid("key must be a non-empty string"));
    }
    let deadline = Deadline::after(LONG_SECONDS);
    let name = name_of(&params.key);
    let mut labels = params.labels;
    labels.insert(MINE.0.to_owned(), MINE.1.to_owned());
    labels.insert(KEY_LABEL.to_owned(), params.key.clone());
    let (snapshot, build_info) = match &params.source {
        Some(hosts::Source::Snapshot(snapshot)) => (Some(snapshot.as_str()), None),
        Some(hosts::Source::Image(reference)) => (None, Some(BuildInfo::image(reference))),
        Some(hosts::Source::Dockerfile(text)) => (
            None,
            Some(BuildInfo {
                dockerfile_content: text.clone(),
            }),
        ),
        None => (None, None),
    };
    let create = Create {
        name: &name,
        labels: &labels,
        snapshot,
        build_info,
        target: daytona.target(),
        env: daytona.env(),
    };
    let ensured = async {
        loop {
            match daytona.get(&name, deadline).await {
                Ok(found) if !owned(&found, &params.key) => {
                    return Err(Fault::Failed(format!(
                        "a host named {name} is not the one Steery made for this key: its labels differ"
                    )));
                }
                // A host being deleted keeps its name until it is gone.
                Ok(found) if matches!(found.state(), "destroying" | "destroyed") => {}
                // A host found while its image builds is answered once it has started.
                Ok(found) if BUILDING.contains(&found.state()) => {
                    daytona
                        .wait(&found.id, &["started"], FAILED, "start", POLL, deadline)
                        .await?;
                    return Ok(found.id);
                }
                Ok(found) => return Ok(found.id),
                Err(Fault::NotFound(_)) => match daytona.create(&create, deadline).await {
                    Ok(made) => {
                        if made.state() != "started" {
                            daytona
                                .wait(&made.id, &["started"], FAILED, "start", POLL, deadline)
                                .await?;
                        }
                        return Ok(made.id);
                    }
                    Err(Fault::Conflict(_)) => {}
                    Err(fault) => return Err(fault),
                },
                Err(fault) => return Err(fault),
            }
            let Some(left) = deadline.left() else {
                return Err(Fault::TimedOut(format!(
                    "the host named {name} was still being deleted after {LONG_SECONDS} s"
                )));
            };
            tokio::time::sleep(POLL.min(left)).await;
        }
    };
    let host = ensured
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::START_FAILED))?;
    Ok(hosts::Named { host })
}

/// The name of the host for `key`: `steery-` and the first 128 bits of its SHA-256, in
/// hex, so any key makes a name Daytona takes.
fn name_of(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("steery-{hex}")
}

/// Whether `host` carries this adapter's labels for `key`.
fn owned(host: &api::Host, key: &str) -> bool {
    host.labels.as_ref().is_some_and(|labels| {
        labels.get(MINE.0).map(String::as_str) == Some(MINE.1)
            && labels.get(KEY_LABEL).map(String::as_str) == Some(key)
    })
}

async fn start(daytona: &Daytona, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(LONG_SECONDS);
    let started = async {
        daytona.act(id, "start", deadline).await?;
        daytona
            .wait(id, &["started"], FAILED, "start", POLL, deadline)
            .await
    };
    let state = started
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::START_FAILED))?;
    Ok(hosts::State { state })
}

async fn stop(daytona: &Daytona, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let state = stopped(daytona, id, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(hosts::State { state })
}

/// Stops `id` and waits until it is stopped; an ephemeral host may be destroyed instead.
async fn stopped(daytona: &Daytona, id: &str, deadline: Deadline) -> Result<String, Fault> {
    daytona.act(id, "stop", deadline).await?;
    daytona
        .wait(
            id,
            &["stopped", "destroyed"],
            FAILED,
            "stop",
            POLL,
            deadline,
        )
        .await
}

async fn delete(daytona: &Daytona, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(CHANGE_SECONDS);
    let deleted = async {
        daytona.delete(id, deadline).await?;
        daytona
            .wait(id, &["destroyed"], FAILED, "delete", POLL, deadline)
            .await
    };
    let state = deleted
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(hosts::State { state })
}

/// Archive: Daytona archives only a stopped host, so a started one is stopped first, and
/// moving its filesystem to object storage takes minutes.
async fn archive(daytona: &Daytona, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(LONG_SECONDS);
    let archived = async {
        let was = daytona.get(id, deadline).await?;
        if was.state() == "archived" {
            return Ok("archived".to_owned());
        }
        if was.state() != "stopped" {
            stopped(daytona, id, Deadline::after(CHANGE_SECONDS / 2)).await?;
        }
        daytona.act(id, "archive", deadline).await?;
        daytona
            .wait(id, &["archived"], FAILED, "archive", ARCHIVE_POLL, deadline)
            .await
    };
    let state = archived
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(hosts::State { state })
}

/// Refreshes the host's last activity, which Daytona's auto-stop counts from.
async fn keepalive(daytona: &Daytona, params: hosts::Target) -> Result<wire::Empty, Error> {
    let id = named(&params.host)?;
    daytona
        .act(id, "last-activity", Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(wire::Empty {})
}

/// Makes the host public, so Daytona's proxy lets any caller through without its own
/// token, and answers the port's preview URL, which is made from the host's id and port
/// and so stays the same while the host lives. The service behind it checks its own key.
async fn expose(daytona: &Daytona, params: hosts::Expose) -> Result<hosts::Exposed, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(CHANGE_SECONDS);
    let exposed = async {
        daytona.act(id, "public/true", deadline).await?;
        daytona.preview(id, params.port, deadline).await
    };
    let url = exposed
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(hosts::Exposed {
        url,
        headers: BTreeMap::new(),
    })
}

/// Uploads the file, proves its size on the host, and sets its mode. An upload can answer
/// before the file is whole, or time out half-written: a partial file is removed rather
/// than left to be run.
async fn put(daytona: &Daytona, params: hosts::Put) -> Result<wire::Empty, Error> {
    let id = named(&params.host)?;
    let path = params
        .path
        .to_str()
        .filter(|path| Path::new(path).is_absolute())
        .ok_or_else(|| Error::invalid("path must be an absolute UTF-8 path"))?
        .to_owned();
    if params.mode > 0o7777 {
        return Err(Error::invalid("mode must be Unix mode bits"));
    }
    let content = base64::engine::general_purpose::STANDARD
        .decode(&params.content)
        .map_err(|e| Error::invalid(format!("content must be base64: {e}")))?;
    let length = content.len() as u64;
    let deadline = Deadline::after(LONG_SECONDS);
    let fail = |fault| refuse(daytona, fault, refusal::REFUSED);
    let host = daytona.get(id, deadline).await.map_err(fail)?;
    let toolbox = daytona.toolbox(&host).map_err(fail)?;
    let within = deadline
        .left()
        .ok_or_else(|| fail(Fault::TimedOut(format!("no time left for {path}"))))?;
    let timed_out = match daytona.upload(&toolbox, &path, content, within).await {
        Ok(()) => false,
        // No answer by the method's own bound: the file is too large to carry in it, a
        // partial file may be there, and waiting for Daytona would not help.
        Err(Fault::Offline(_)) if deadline.left().is_none() => true,
        Err(fault) => return Err(fail(fault)),
    };
    let written = daytona
        .size(&toolbox, &path, FILE_SECONDS)
        .await
        .unwrap_or(0);
    if timed_out || written != length {
        let why = if timed_out {
            format!("the upload got no answer in {LONG_SECONDS} s")
        } else {
            "the upload ended short".to_owned()
        };
        let removed = match daytona.remove(&toolbox, &path, FILE_SECONDS).await {
            Ok(()) => "removed it".to_owned(),
            Err(Fault::NotFound(_)) => "nothing was left".to_owned(),
            Err(fault) => format!("could not remove it: {fault:?}"),
        };
        return Err(Error::refused(
            refusal::REFUSED,
            one_line(
                daytona.key(),
                &format!(
                    "Daytona: {why}: {path} had {written} of {length} bytes written; {removed} (fetch a large file from inside the host)"
                ),
            ),
        ));
    }
    daytona
        .chmod(&toolbox, &path, params.mode, FILE_SECONDS)
        .await
        .map_err(fail)?;
    Ok(wire::Empty {})
}

/// An argv channel: `ssh -T` to Daytona's gateway with a 60-minute access token, running
/// `command` in the host.
async fn channel(daytona: &Daytona, params: hosts::Channel) -> Result<hosts::Reach, Error> {
    let id = named(&params.host)?;
    if params.command.is_empty() {
        return Err(Error::invalid(
            "command must be a non-empty array of strings",
        ));
    }
    let access = daytona
        .ssh_access(id, SSH_MINUTES, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    let (host, port) = gateway(access.ssh_command.as_deref().unwrap_or(""));
    let mut argv: Vec<String> = ["ssh", "-T", "-o", "StrictHostKeyChecking=accept-new"]
        .map(String::from)
        .into();
    if let Some(port) = port {
        argv.extend(["-p".to_owned(), port]);
    }
    argv.push(format!("{}@{host}", access.token));
    argv.push(
        params
            .command
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(hosts::Reach::Argv {
        argv,
        env: BTreeMap::new(),
    })
}

/// The host and port of the command Daytona means (`ssh <token>@<host>`, with any
/// `-p <port>`); the docs' host where it names none.
fn gateway(command: &str) -> (String, Option<String>) {
    let words: Vec<&str> = command.split_whitespace().collect();
    let host = words
        .iter()
        .find_map(|word| word.rsplit_once('@').map(|(_, host)| host))
        .filter(|host| !host.is_empty())
        .unwrap_or(SSH_HOST)
        .to_owned();
    let port = words
        .iter()
        .position(|word| *word == "-p")
        .and_then(|at| words.get(at + 1))
        .map(|port| (*port).to_owned());
    (host, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_what_a_shell_would_split() {
        assert_eq!(quote("/usr/bin/steery"), "/usr/bin/steery");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn reads_the_gateway_from_daytonas_command() {
        assert_eq!(
            gateway("ssh tok@ssh.app.daytona.io"),
            ("ssh.app.daytona.io".into(), None)
        );
        assert_eq!(
            gateway("ssh -p 2222 tok@gw.example"),
            ("gw.example".into(), Some("2222".into()))
        );
        assert_eq!(gateway(""), (SSH_HOST.into(), None));
    }

    #[test]
    fn reads_daytona_times_as_milliseconds() {
        assert_eq!(millis(Some("2024-10-01T12:00:00.000Z")), 1_727_784_000_000);
        assert_eq!(millis(Some("not a time")), 0);
        assert_eq!(millis(None), 0);
    }

    #[test]
    fn keeps_the_key_out_of_a_line() {
        assert_eq!(
            one_line("sk", "Daytona: bad  key sk\nhere"),
            "Daytona: bad key [redacted] here"
        );
    }

    /// The refusal word and line `hosts/list` answers against a Daytona answering
    /// `status` with `body`.
    async fn listed(status: u16, body: &'static str) -> (String, String) {
        let url = api::tests::daytona_answering(Some(status), body).await;
        let daytona = Daytona::new("sk-secret".into(), Some(&url), None, Env::default()).unwrap();
        match list(&daytona, hosts::List { labels: None }).await {
            Err(Error::Refused { word, line }) => (word, line.unwrap_or_default()),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refused_key_is_refused_and_names_the_setting_to_give_again() {
        assert_eq!(
            listed(401, r#"{"message":"Invalid API key sk-secret"}"#).await,
            (
                refusal::REFUSED.to_owned(),
                "Daytona refused the API key (HTTP 401: Invalid API key [redacted]): give DAYTONA_API_KEY again"
                    .to_owned()
            )
        );
        let (word, _) = listed(403, r#"{"message":"Forbidden resource"}"#).await;
        assert_eq!(word, refusal::REFUSED);
    }

    #[tokio::test]
    async fn daytona_down_is_offline() {
        for status in [502, 503, 504] {
            let (word, line) = listed(status, "").await;
            assert_eq!(word, refusal::OFFLINE, "{line}");
        }
    }

    /// Without `ssh` on this computer no channel can be had, so `hosts/channel` refuses
    /// `refused`, saying what to install, before it asks Daytona anything; and so does
    /// `hosts/list`, the first call adding a machine makes.
    #[tokio::test]
    async fn without_an_ssh_client_a_channel_is_refused_saying_what_to_install() {
        let without = Adapter {
            daytona: OnceLock::from(None),
            ssh: OnceLock::from(false),
        };
        let refused = |answer: Result<Value, Error>| match answer {
            Err(Error::Refused { word, line }) => (word, line.unwrap_or_default()),
            other => panic!("{other:?}"),
        };
        let asked = serde_json::json!({"host": "s", "command": ["steery", "serve"]});
        for method in [method::HOSTS_CHANNEL, method::HOSTS_LIST] {
            let params = if method == method::HOSTS_LIST {
                serde_json::json!({})
            } else {
                asked.clone()
            };
            assert_eq!(
                refused(without.call(method, params).await),
                (refusal::REFUSED.to_owned(), NO_SSH.to_owned()),
                "{method}"
            );
        }
        let with = Adapter {
            daytona: OnceLock::from(None),
            ssh: OnceLock::from(true),
        };
        let (_, line) = refused(with.call(method::HOSTS_CHANNEL, asked).await);
        assert!(line.contains("DAYTONA_API_KEY"), "{line}");
    }

    /// `initialize` says up front that no host can be had from a computer without
    /// `ssh` (PROTOCOL.md §3.2), so adding the machine refuses before it writes anything.
    #[test]
    fn without_an_ssh_client_initialize_says_daytona_is_unavailable_here() {
        let params: wire::InitializeParams = serde_json::from_value(serde_json::json!({
            "protocol": [PROTOCOL],
            "runtime": {"release": "6.10.3", "os": "linux", "arch": "x86_64"},
            "home": "/home/p", "config": "/home/p/.config/steery",
        }))
        .unwrap();
        let unavailable = |ssh: bool| {
            let adapter = Adapter {
                daytona: OnceLock::new(),
                ssh: OnceLock::from(ssh),
            };
            match adapter.initialize(&params).unwrap().machine {
                Some(wire::Machine::Hosts(facts)) => facts.unavailable,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(unavailable(false).as_deref(), Some(NO_SSH));
        assert_eq!(unavailable(true), None);
    }

    #[test]
    fn an_ssh_client_is_found_on_path_only_where_it_runs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("steery-daytona-ssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (empty, held) = (dir.join("empty"), dir.join("held"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&held).unwrap();
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        assert!(!ssh_within(&path(&[&empty])));
        std::fs::write(held.join("ssh"), "#!/bin/sh\n").unwrap();
        assert!(!ssh_within(&path(&[&empty, &held])), "not executable");
        std::fs::set_permissions(held.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ssh_within(&path(&[&empty, &held])));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_daytona_env_line_by_line_and_never_says_a_value() {
        let env = env_of("# sign-ins\n\nA=1\r\n  B_2=x=y; z \nEMPTY=\n").unwrap();
        assert_eq!(
            env.0,
            BTreeMap::from([
                ("A".into(), "1".into()),
                ("B_2".into(), "x=y; z ".into()),
                ("EMPTY".into(), String::new()),
            ])
        );
        assert_eq!(format!("{env:?}"), r#"{"A", "B_2", "EMPTY"}"#);
        let why = env_of("A=1\nsecret-value\n").unwrap_err();
        assert!(why.contains("line 2") && !why.contains("secret"), "{why}");
        let why = env_of("A=1\n\n9X=secret\n").unwrap_err();
        assert!(why.contains("line 3") && !why.contains("secret"), "{why}");
    }

    /// The create body carries `env` only when the person gave one.
    #[test]
    fn the_create_body_carries_env_only_when_given() {
        let labels = BTreeMap::new();
        let body = |env: &Env| {
            serde_json::to_value(Create {
                name: "n",
                labels: &labels,
                snapshot: None,
                build_info: None,
                target: None,
                env,
            })
            .unwrap()
        };
        assert_eq!(body(&Env::default()).get("env"), None);
        let env = env_of("K=v").unwrap();
        assert_eq!(body(&env)["env"], serde_json::json!({"K": "v"}));
    }
}
