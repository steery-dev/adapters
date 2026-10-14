//! Steery's `macos` adapter. It provides `processes`: it starts, finds, inspects
//! and signals processes on the Mac it runs on, and keeps the runtime running there as a
//! launchd agent. It reads processes through libproc and `sysctl`.
//!
//! The work itself is macOS's alone ([`macos`]). Built for another system, the adapter
//! still says what it is, and refuses every method.

#[cfg(target_os = "macos")]
mod macos;

use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire;
use steery_sdk::{Error, PROTOCOL, Program};

/// macOS's mark, Apple's: one SVG path's `d` in a 24 by 24 box, filled with the even-odd rule.
const MARK: &str = "M12.152 6.896c-.948 0-2.415-1.078-3.96-1.04-2.04.027-3.91 1.183-4.961 3.014-2.117 3.675-.546 9.103 1.519 12.09 1.013 1.454 2.208 3.09 3.792 3.039 1.52-.065 2.09-.987 3.935-.987 1.831 0 2.35.987 3.96.948 1.637-.026 2.676-1.48 3.676-2.948 1.156-1.688 1.636-3.325 1.662-3.415-.039-.013-3.182-1.221-3.22-4.857-.026-3.04 2.48-4.494 2.597-4.559-1.429-2.09-3.623-2.324-4.39-2.376-2-.156-3.675 1.09-4.61 1.09zM15.53 3.83c.843-1.012 1.4-2.427 1.245-3.83-1.207.052-2.662.805-3.532 1.818-.78.896-1.454 2.338-1.273 3.714 1.338.104 2.715-.688 3.559-1.701";

fn main() -> io::Result<()> {
    steery_adapter_common::serve(|_notifier| Mac::default())
}

/// The adapter. It keeps only the person's home, given in `initialize`.
#[derive(Default)]
struct Mac {
    home: OnceLock<PathBuf>,
}

impl Program for Mac {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "macos".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "macOS".into(),
                short: "Mac".into(),
                tint: None,
                mark: Some(MARK.into()),
            },
            harness: None,
            machine: Some(wire::Machine::Processes(wire::ProcessFacts {
                os: wire::Os::Macos,
                service: wire::ServiceManager::Launchd,
            })),
            settings: Vec::new(),
        })
    }

    #[cfg(target_os = "macos")]
    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_adapter_common::blocking;
        use steery_sdk::{method, params as given, result};
        match method {
            method::PROCESS_START => result(macos::start(given(params)?)?),
            method::PROCESS_LIST => {
                let list = given(params)?;
                result(blocking(move || Ok(macos::list(list))).await?)
            }
            method::PROCESS_ENVIRON => {
                let environ = given(params)?;
                result(blocking(move || macos::environ(environ)).await?)
            }
            method::PROCESS_FILES => {
                let files = given(params)?;
                result(blocking(move || macos::files(files)).await?)
            }
            method::PROCESS_LISTENING => result(macos::listening(given(params)?).await?),
            method::PROCESS_SIGNAL => {
                let signal = given(params)?;
                result(blocking(move || Ok(macos::signal(signal))).await?)
            }
            method::SERVICE_PLAN => result(macos::service_plan(&self.home()?, given(params)?)?),
            method::SERVICE_STATUS => {
                result(macos::service_status(&self.home()?, given(params)?).await?)
            }
            method::SERVICE_START => {
                result(macos::service_start(&self.home()?, given(params)?, false).await?)
            }
            method::SERVICE_RESTART => {
                result(macos::service_start(&self.home()?, given(params)?, true).await?)
            }
            method::SERVICE_REMOVE => {
                result(macos::service_remove(&self.home()?, given(params)?).await?)
            }
            _ => Err(Error::Unknown),
        }
    }

    /// Off macOS there is nothing this adapter can do.
    #[cfg(not(target_os = "macos"))]
    async fn call(&self, _method: &str, _params: Value) -> Result<Value, Error> {
        Err(Error::refused(
            steery_sdk::refusal::REFUSED,
            "the macos adapter runs on macOS only",
        ))
    }
}

#[cfg(target_os = "macos")]
impl Mac {
    /// The person's home: the one `initialize` gave, else `HOME`.
    fn home(&self) -> Result<PathBuf, Error> {
        match self.home.get() {
            Some(home) => Ok(home.clone()),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::invalid("no home")),
        }
    }
}
