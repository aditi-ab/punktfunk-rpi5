//! A plugin's settings page without a listener. A Windows plugin in its AppContainer can bind
//! nothing, so it dials its own pipe a second time, asks for an upgrade, and the raw connection
//! is parked here. A request for that page goes down one parked connection as plain HTTP with
//! the plugin's own secret added by the host; the plugin dials again as each one is used, so a
//! slow page never holds the next. A registration with `ui.port` 0 says the plugin works this way.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use http_body_util::BodyExt;
use hyper::upgrade::Upgraded;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, Notify};

/// Parked connections per plugin, oldest first. Each serves one request.
static PARKED: LazyLock<Mutex<HashMap<String, VecDeque<Parked>>>> = LazyLock::new(Default::default);
/// Woken when a connection is parked, for a request that found none.
static ARRIVED: Notify = Notify::const_new();
/// The management runtime, for a hold or an install made from a thread of its own.
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// More parked than this and the oldest goes: a plugin that dials in a loop holds no more.
const MAX_PARKED: usize = 8;
/// How long a request waits for the plugin to park the next connection.
const ATTACH_WAIT: Duration = Duration::from_secs(3);

/// Headers that describe the hop, not the request, plus the two the host sets itself.
const NOT_FORWARDED: [HeaderName; 8] = [
    header::CONNECTION,
    header::HOST,
    header::AUTHORIZATION,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::TE,
    header::TRAILER,
    header::PROXY_AUTHORIZATION,
];

/// Called once the management API serves: the blocking callers need this runtime.
pub(crate) fn remember_runtime() {
    let _ = RUNTIME.set(tokio::runtime::Handle::current());
}

/// Why a request never reached the plugin.
#[derive(Debug)]
pub(crate) enum Unreached {
    NotAttached,
    Io(String),
}

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreached::NotAttached => f.write_str("the plugin UI is not attached"),
            Unreached::Io(e) => f.write_str(e),
        }
    }
}

/// One request down one parked connection, with the plugin's secret as the bearer.
pub(crate) async fn send(
    id: &str,
    secret: &str,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<hyper::Response<hyper::body::Incoming>, Unreached> {
    let Some(upgraded) = take(id).await else {
        return Err(Unreached::NotAttached);
    };
    // `Upgraded` already speaks hyper's io traits; it needs no tokio adapter.
    let (mut sender, conn) = hyper::client::conn::http1::handshake(upgraded)
        .await
        .map_err(|e| Unreached::Io(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut req = hyper::Request::builder().method(method).uri(path_and_query);
    for (name, value) in headers {
        if !NOT_FORWARDED.contains(name) {
            req = req.header(name, value);
        }
    }
    let req = req
        .header(header::HOST, "punktfunk.plugin")
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .body(body)
        .map_err(|e| Unreached::Io(e.to_string()))?;
    sender
        .send_request(req)
        .await
        .map_err(|e| Unreached::Io(e.to_string()))
}

/// The host's own call to a plugin page — a hold, an install — from a thread of its own:
/// the status and the body as text, or why not.
pub(crate) fn request_blocking(
    id: &str,
    path: &str,
    json: &str,
    timeout: Duration,
) -> Result<(u16, String), Unreached> {
    let Some(cred) = super::registry::ui_credential(id).filter(|c| c.port == 0) else {
        return Err(Unreached::NotAttached);
    };
    let call = async {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Body::from(json.to_string());
        let resp = send(id, &cred.secret, Method::POST, path, &headers, body).await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| Unreached::Io(e.to_string()))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    };
    let timed = async {
        tokio::time::timeout(timeout, call)
            .await
            .unwrap_or_else(|_| Err(Unreached::Io("timed out".into())))
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(timed)),
        Err(_) => RUNTIME
            .get()
            .ok_or_else(|| Unreached::Io("no management runtime".into()))?
            .block_on(timed),
    }
}

/// One parked connection, held by a task until a request claims it or the plugin hangs up.
struct Parked {
    claim: oneshot::Sender<()>,
    back: oneshot::Receiver<Upgraded>,
}

/// Keep a plugin's upgraded connection for the next request to its page.
pub(crate) fn park(id: &str, upgraded: Upgraded) {
    let (claim, claimed) = oneshot::channel();
    let (hand, back) = oneshot::channel();
    // The plugin speaks only after a request, so a read that ends here means it is gone: a
    // restarted plugin's connections stay parked otherwise, and each would answer one 502.
    tokio::spawn(async move {
        let mut io = hyper_util::rt::TokioIo::new(upgraded);
        let mut probe = [0u8; 1];
        tokio::select! {
            biased;
            _ = io.read(&mut probe) => {}
            _ = claimed => { let _ = hand.send(io.into_inner()); }
        }
    });
    let mut all = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    let queue = all.entry(id.to_string()).or_default();
    queue.retain(|p| !p.claim.is_closed());
    if queue.len() >= MAX_PARKED {
        queue.pop_front();
    }
    queue.push_back(Parked { claim, back });
    drop(all);
    ARRIVED.notify_waiters();
}

/// The oldest live parked connection for `id`, waiting [`ATTACH_WAIT`] for one to arrive.
async fn take(id: &str) -> Option<Upgraded> {
    let deadline = tokio::time::Instant::now() + ATTACH_WAIT;
    loop {
        let notified = ARRIVED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let taken = PARKED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
            .and_then(|q| q.pop_front());
        if let Some(Parked { claim, back }) = taken {
            // A connection whose plugin hung up has no task left to hand it back.
            if claim.send(()).is_ok() {
                if let Ok(upgraded) = back.await {
                    return Some(upgraded);
                }
            }
            continue;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return None;
        }
    }
}
