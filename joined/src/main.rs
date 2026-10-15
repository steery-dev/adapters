//! Steery's adapter for hosts that join their machine (specs/JOINED-HOSTS.md §5 in
//! steery-dev/server; PROTOCOL.md §3.2, §5.2, §5.3).
//!
//! A joined machine's hosts are made, started and stopped by a system of their owner's; each
//! dials Steery's server with the machine's join token and keeps that socket. This adapter
//! makes no host: it lists the hosts that joined (`GET /v1/join/hosts`) and carries a
//! stream to one of them over a WebSocket the server splices to that host's
//! (`/v1/join/control`). Both are called as the machine, whose URL and key the server hands
//! the adapter as its settings; a person types none. `hosts/ensure`, `start`, `stop` and
//! `delete` are methods it does not have: the runtime reads that as an adapter whose hosts
//! come only from `hosts/list`.

mod channel;
mod server;

use std::io;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use steery_sdk::wire::hosts::{self, Named};
use steery_sdk::wire::{self, Empty};
use steery_sdk::{Error, PROTOCOL, Program, method, params as given, refusal, result};

use crate::channel::Channels;
use crate::server::Server;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut shared = None;
    let served = steery_sdk::serve(|notifier| {
        let channels = Arc::new(Channels::new(notifier));
        shared = Some(channels.clone());
        Joined {
            server: RwLock::new(Err("initialize comes first".into())),
            channels,
        }
    })
    .await;
    if let Some(channels) = shared {
        channels.shut().await;
    }
    served
}

/// The adapter: the server its settings name, and its open channels.
struct Joined {
    server: RwLock<Result<Arc<Server>, String>>,
    channels: Arc<Channels>,
}

impl Joined {
    /// The server, or the refusal its settings are.
    fn server(&self) -> Result<Arc<Server>, Error> {
        match &*self.server.read().unwrap_or_else(|e| e.into_inner()) {
            Ok(server) => Ok(server.clone()),
            Err(why) => Err(Error::refused(refusal::REFUSED, format!("Steery: {why}"))),
        }
    }
}

impl Program for Joined {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        *self.server.write().unwrap_or_else(|e| e.into_inner()) =
            Server::new(&params.settings).map(Arc::new);
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "joined".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Your own hosts".into(),
                short: "J".into(),
                tint: None,
                mark: None,
            },
            harness: None,
            machine: Some(wire::Machine::Hosts(wire::HostFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::None,
                optional: vec![wire::Optional::Keepalive],
                builds: false,
                channel: wire::Channel::Stream,
                os: wire::Os::Linux,
                unavailable: None,
            })),
            // `STEERY_URL` and `STEERY_KEY` are handed by Steery's server, never typed: a
            // declared setting is one a person is asked for.
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::HOSTS_LIST => result(list(|| self.server(), given(params)?).await?),
            method::HOSTS_KEEPALIVE => result(keepalive(given(params)?)?),
            method::HOSTS_CHANNEL => {
                let asked = given(params)?;
                result(self.channels.channel(self.server()?, asked).await?)
            }
            method::STREAM_WRITE => result(self.channels.write(given(params)?).await?),
            method::STREAM_CLOSE => result(self.channels.close(given(params)?).await?),
            _ => Err(Error::Unknown),
        }
    }
}

/// The label a joined host carries: the name it joined with.
const NAME_LABEL: &str = "name";

/// Every host that joined, `started` while its socket is open, else `stopped`; only those
/// carrying every label asked. A host carries no label but its name, so a filter on any
/// other, the runtime's `steery-machine` among them, finds none without asking the server:
/// no host here is the runtime's to delete, and a machine's removal, which lists with the
/// person's settings alone, is never refused for the server's.
async fn list(
    server: impl FnOnce() -> Result<Arc<Server>, Error>,
    asked: hosts::List,
) -> Result<hosts::Listed, Error> {
    let wanted = asked.labels.unwrap_or_default();
    if wanted.keys().any(|name| name != NAME_LABEL) {
        return Ok(hosts::Listed { hosts: Vec::new() });
    }
    let joined = server()?.hosts().await?;
    let hosts = joined
        .into_iter()
        .filter(|joined| {
            wanted
                .get(NAME_LABEL)
                .is_none_or(|name| *name == joined.name)
        })
        .map(|joined| hosts::Host {
            created_at: u64::try_from(joined.joined_at.timestamp_millis()).unwrap_or(0),
            state: if joined.online { "started" } else { "stopped" }.into(),
            labels: [(NAME_LABEL.to_owned(), joined.name.clone())].into(),
            host: joined.name,
        })
        .collect();
    Ok(hosts::Listed { hosts })
}

/// Nothing to keep alive: a joined host's own system decides when it stops.
fn keepalive(asked: Named) -> Result<Empty, Error> {
    if asked.host.is_empty() {
        return Err(Error::invalid("host must be a non-empty string"));
    }
    Ok(Empty {})
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use steery_sdk::Error;
    use steery_sdk::wire::hosts;

    #[tokio::test]
    async fn the_runtimes_label_lists_none_without_the_server() {
        let asked = hosts::List {
            labels: Some(BTreeMap::from([(
                "steery-machine".to_owned(),
                "0199".to_owned(),
            )])),
        };
        let listed = super::list(|| Err(Error::invalid("not asked")), asked)
            .await
            .unwrap();
        assert!(listed.hosts.is_empty());
    }

    #[tokio::test]
    async fn without_settings_a_list_is_refused() {
        let listed = super::list(
            || Err(Error::invalid("no server")),
            hosts::List { labels: None },
        )
        .await;
        assert!(listed.is_err());
    }
}
