//! The `stream` channel (PROTOCOL.md §5.3): a WebSocket to the server's
//! `/v1/join/control?host=`, which the server splices to that host's own socket (the
//! server's `joins.rs`). The host connects the splice to its `serve` afresh, as
//! `steery host attach` would: whatever command is asked, the channel reaches `serve`, the
//! only thing a joined host carries. Binary frames are the channel's bytes each way; the
//! socket closing, from either end, is `stream/closed`, with no exit code.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use steery_sdk::wire::Empty;
use steery_sdk::wire::hosts::{Channel, Reach, StreamClose, StreamClosed, StreamData, StreamWrite};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::server::{CHANGE, Fail, Server, chain, refused};

/// How long `stream/close` waits for the server to close the socket after asking it to.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One open channel.
struct Open {
    stream: String,
    sink: tokio::sync::Mutex<SplitSink<Socket, Message>>,
    /// Set once `stream/closed` is sent.
    finished: AtomicBool,
    ended: watch::Sender<bool>,
    reader: Mutex<Option<AbortHandle>>,
}

impl Open {
    fn is_ended(&self) -> bool {
        *self.ended.borrow()
    }

    /// Wait until the channel ends, or `grace` has passed.
    async fn grace(&self, grace: Duration) {
        let mut ended = self.ended.subscribe();
        let _ = timeout(grace, ended.wait_for(|ended| *ended)).await;
    }

    /// Send `message` on the socket, within the runtime's bound.
    async fn send(&self, message: Message) -> Result<(), String> {
        let mut sink = self.sink.lock().await;
        match timeout(CHANGE, sink.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(chain(&e)),
            Err(_) => Err("the socket took nothing in time".into()),
        }
    }
}

/// Every open channel, by its stream id.
pub struct Channels {
    notifier: Notifier,
    open: Mutex<HashMap<String, Arc<Open>>>,
    next: AtomicU64,
}

impl Channels {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            open: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    fn opened(&self, stream: &str) -> Result<Arc<Open>, Error> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(stream)
            .cloned()
            .ok_or_else(|| {
                Error::refused(
                    refusal::NOT_FOUND,
                    format!("Steery: no open channel {stream}"),
                )
            })
    }

    /// `hosts/channel`: a socket spliced to the host's.
    pub async fn channel(
        self: &Arc<Self>,
        server: Arc<Server>,
        asked: Channel,
    ) -> Result<Reach, Error> {
        if asked.host.is_empty() {
            return Err(Error::invalid("host must be a non-empty string"));
        }
        let socket = join(&server, &asked.host)
            .await
            .map_err(|fail| server.refuse(fail))?;
        let stream = format!(
            "{}#{}",
            asked.host,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let (sink, source) = socket.split();
        let open = Arc::new(Open {
            stream: stream.clone(),
            sink: tokio::sync::Mutex::new(sink),
            finished: AtomicBool::new(false),
            ended: watch::channel(false).0,
            reader: Mutex::new(None),
        });
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(stream.clone(), open.clone());
        // The runtime runs on one thread, and the SDK writes this call's answer before this
        // task yields again: the reader, spawned here, sends nothing before the runtime
        // has the stream's id.
        let reader = tokio::spawn(self.clone().read(open.clone(), source));
        *open.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader.abort_handle());
        Ok(Reach::Stream { stream })
    }

    /// Read the socket until it closes: the host's bytes to the runtime.
    async fn read(self: Arc<Self>, open: Arc<Open>, mut source: SplitStream<Socket>) {
        while let Some(message) = source.next().await {
            match message {
                Ok(Message::Binary(bytes)) if !bytes.is_empty() => self.notifier.notify(
                    method::STREAM_DATA,
                    json(StreamData {
                        stream: open.stream.clone(),
                        bytes: STANDARD.encode(&bytes),
                    }),
                ),
                Ok(Message::Close(frame)) => {
                    if let Some(frame) = frame.filter(|frame| !frame.reason.is_empty()) {
                        eprintln!(
                            "[{}] the server closed the channel: {}",
                            open.stream, frame.reason
                        );
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[{}] socket: {}", open.stream, chain(&e));
                    break;
                }
            }
        }
        self.finish(&open);
    }

    /// The channel ended: forget it and say so, once.
    fn finish(&self, open: &Open) {
        if open.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&open.stream);
        self.notifier.notify(
            method::STREAM_CLOSED,
            json(StreamClosed {
                stream: open.stream.clone(),
                exit: None,
            }),
        );
        open.ended.send_replace(true);
    }

    /// `stream/write`: hand the bytes to the host.
    pub async fn write(&self, asked: StreamWrite) -> Result<Empty, Error> {
        let open = self.opened(&asked.stream)?;
        let bytes = STANDARD
            .decode(asked.bytes.as_bytes())
            .map_err(|_| Error::invalid("bytes must be base64"))?;
        if open.is_ended() {
            return Err(closing());
        }
        open.send(Message::Binary(bytes.into()))
            .await
            .map_err(|_| closing())?;
        Ok(Empty {})
    }

    /// `stream/close`: close the socket, which the server tells the host as `close`; one
    /// the server never answers is dropped with its reader.
    pub async fn close(&self, asked: StreamClose) -> Result<Empty, Error> {
        let open = self.opened(&asked.stream)?;
        if !open.is_ended() {
            let frame = CloseFrame {
                code: CloseCode::Normal,
                reason: "closed by steery".into(),
            };
            let _ = open.send(Message::Close(Some(frame))).await;
            open.grace(CLOSE_GRACE).await;
        }
        if !open.is_ended() {
            if let Some(reader) = open.reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
                reader.abort();
            }
            self.finish(&open);
        }
        Ok(Empty {})
    }

    /// The runtime said shutdown: close every socket still open.
    pub async fn shut(&self) {
        let open: Vec<Arc<Open>> = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for open in open {
            let frame = CloseFrame {
                code: CloseCode::Away,
                reason: "steery adapter shut down".into(),
            };
            let mut sink = open.sink.lock().await;
            let _ = timeout(
                Duration::from_secs(1),
                sink.send(Message::Close(Some(frame))),
            )
            .await;
        }
    }
}

/// Open `/v1/join/control?host=<host>` as the machine. The server refuses `409` with why
/// for a host whose socket is not open now: its system has not started it.
async fn join(server: &Server, host: &str) -> Result<Socket, Fail> {
    let what = "reach the host";
    let mut url = server.url(&["v1", "join", "control"], &[("host", host)]);
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| Fail::Other(format!("{what}: no WebSocket URL for {url}")))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| Fail::Other(format!("{what}: {}", chain(&e))))?;
    let bearer = HeaderValue::from_str(&server.bearer())
        .map_err(|_| Fail::Other(format!("{what}: STEERY_KEY is no header value")))?;
    request
        .headers_mut()
        .insert(tungstenite::http::header::AUTHORIZATION, bearer);
    match timeout(CHANGE, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(e)) => Err(socket_fail(what, &e)),
        Err(_) => Err(Fail::Offline(format!(
            "{what}: the server said nothing in time"
        ))),
    }
}

fn socket_fail(what: &str, error: &tungstenite::Error) -> Fail {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let line = refused(what, status, response.body().as_deref().unwrap_or_default());
            match status {
                // The host is not joined now: out of reach until its system starts it.
                409 => Fail::Offline(line),
                _ => Fail::Http { status, line },
            }
        }
        tungstenite::Error::Io(_) | tungstenite::Error::ConnectionClosed => {
            Fail::Offline(format!("{what}: {}", chain(error)))
        }
        _ => Fail::Other(format!("{what}: {}", chain(error))),
    }
}

fn closing() -> Error {
    Error::refused(refusal::OFFLINE, "Steery: the channel is closing")
}

fn json<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_default()
}
