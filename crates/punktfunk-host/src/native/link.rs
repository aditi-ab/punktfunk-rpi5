//! The session's control connection, whichever transport carries it.
//!
//! The native plane rides quinn; a browser rides WebTransport (`design/web-client.md`). Video was
//! always portable — `Session` holds a `Box<dyn Transport>` — but audio, cursor, rumble and HID
//! out call `send_datagram` on the connection directly, and the handshake and control plane are
//! streams on it. Those call sites named `quinn::Connection`, which is what kept a browser to the
//! video half.
//!
//! **An enum, not a trait.** Two carriers, both known at compile time, so no `dyn` — and, the
//! reason that matters, `async fn` stays an ordinary `async fn`. A `dyn`-compatible trait would
//! box a future on `closed()`, which is on the per-session path.
//!
//! Most of this delegates rather than branches: WebTransport *is* QUIC, and `wtransport` hands
//! out the `quinn::Connection` underneath, so path and lifecycle questions have one answer for
//! both. Only the two genuinely carrier-shaped questions branch.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The control stream's write half, whichever carrier. Delegates every poll, because both
/// halves already implement tokio's traits — this exists so the session code can name one type.
pub(crate) enum CtlSend {
    Quic(quinn::SendStream),
    Web(wtransport::SendStream),
}

/// The control stream's read half. See [`CtlSend`].
pub(crate) enum CtlRecv {
    Quic(quinn::RecvStream),
    Web(wtransport::RecvStream),
}

impl AsyncWrite for CtlSend {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
            CtlSend::Web(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
            CtlSend::Web(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
            CtlSend::Web(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for CtlRecv {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlRecv::Quic(s) => AsyncRead::poll_read(Pin::new(s), cx, buf),
            CtlRecv::Web(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

/// What accepting the control stream produced.
pub(crate) enum Accepted {
    Stream(CtlSend, CtlRecv),
    /// A clean application close before any stream: a reachability probe, not a client.
    ProbeClose,
}

/// One client's control connection. Cheap to clone — both variants are handles.
#[derive(Clone)]
pub(crate) enum SessionLink {
    /// The native plane: `punktfunk/1` over quinn.
    Quic(quinn::Connection),
    /// The browser plane: the same protocol over one WebTransport session, and the device
    /// fingerprint its key signature proved. `None` before admission and under `serve --open`.
    Web(wtransport::Connection, Option<[u8; 32]>),
}

impl SessionLink {
    /// The QUIC connection underneath, which both carriers have.
    fn quic(&self) -> &quinn::Connection {
        match self {
            SessionLink::Quic(c) => c,
            SessionLink::Web(c, _) => c.quic_connection(),
        }
    }

    /// Smoothed round trip of the connection underneath.
    pub(crate) fn rtt(&self) -> std::time::Duration {
        self.quic().rtt()
    }

    /// Unreliable datagram: audio, cursor, rumble, HID out.
    ///
    /// The three outcomes callers actually act on, because they act differently: a frame too big
    /// for this path is dropped and the plane continues, while datagrams being unavailable at all
    /// ends the plane rather than pacing a wire that cannot take it.
    pub(crate) fn send_datagram(&self, payload: Vec<u8>) -> DatagramSend {
        match self {
            SessionLink::Quic(c) => match c.send_datagram(payload.into()) {
                Ok(()) => DatagramSend::Sent,
                Err(quinn::SendDatagramError::TooLarge) => DatagramSend::TooLarge,
                Err(_) => DatagramSend::Unavailable,
            },
            // Not the quinn connection: a WebTransport datagram carries a session-id prefix, so
            // it has to go through the layer that writes one.
            SessionLink::Web(c, _) => match c.send_datagram(&payload) {
                Ok(()) => DatagramSend::Sent,
                Err(wtransport::error::SendDatagramError::TooLarge) => DatagramSend::TooLarge,
                Err(_) => DatagramSend::Unavailable,
            },
        }
    }

    /// The next datagram from the peer — mic, rich input, pen. `Err` once the peer is gone.
    pub(crate) async fn read_datagram(&self) -> Result<Vec<u8>, LinkClosed> {
        match self {
            SessionLink::Quic(c) => c
                .read_datagram()
                .await
                .map(|b| b.to_vec())
                .map_err(LinkClosed::from),
            SessionLink::Web(c, _) => c
                .receive_datagram()
                .await
                .map(|d| d.payload().to_vec())
                .map_err(|e| LinkClosed::Other(format!("{e:?}"))),
        }
    }

    /// Largest datagram this path will carry. Lower on the browser plane — HTTP/3 framing comes
    /// out of the same budget — which is why callers must ask rather than assume 1500-MTU maths.
    pub(crate) fn max_datagram_size(&self) -> Option<usize> {
        match self {
            SessionLink::Quic(c) => c.max_datagram_size(),
            SessionLink::Web(c, _) => c.max_datagram_size(),
        }
    }

    pub(crate) fn remote_address(&self) -> SocketAddr {
        self.quic().remote_address()
    }

    /// Local address the connection arrived on, for binding a data socket on the same NIC.
    pub(crate) fn local_ip(&self) -> Option<IpAddr> {
        self.quic().local_ip()
    }

    /// Path MTU as the stack currently believes it.
    pub(crate) fn current_mtu(&self) -> u16 {
        self.quic().stats().path.current_mtu
    }

    /// `Some` once the connection has ended, without awaiting.
    pub(crate) fn close_reason(&self) -> Option<LinkClosed> {
        self.quic().close_reason().map(LinkClosed::from)
    }

    pub(crate) fn close(&self, code: u32, reason: &[u8]) {
        self.quic().close(code.into(), reason);
    }

    /// Close with a typed code and its reason. A browser is never shown a close reason, so it
    /// gets the same code and text on a stream first.
    pub(crate) async fn refuse(&self, code: u32, reason: &str) {
        if let SessionLink::Web(c, _) = self {
            crate::webtransport::refuse(c, code, reason).await;
        }
        self.close(code, reason.as_bytes());
    }

    /// Resolves when the peer is gone.
    pub(crate) async fn closed(&self) -> LinkClosed {
        LinkClosed::from(self.quic().closed().await)
    }

    /// The peer's first bidirectional stream — the control stream on both carriers.
    ///
    /// A clean close before any stream is a reachability probe, and is reported rather than
    /// failed. Anything else that is not a stream is the error it was.
    pub(crate) async fn accept_bi(&self) -> anyhow::Result<Accepted> {
        match self {
            SessionLink::Quic(c) => match c.accept_bi().await {
                Ok((send, recv)) => Ok(Accepted::Stream(CtlSend::Quic(send), CtlRecv::Quic(recv))),
                Err(quinn::ConnectionError::ApplicationClosed(ref ac))
                    if ac.error_code == quinn::VarInt::from_u32(0) =>
                {
                    Ok(Accepted::ProbeClose)
                }
                Err(e) => Err(anyhow::Error::new(e).context("accept control stream")),
            },
            SessionLink::Web(c, _) => {
                let (send, recv) = c
                    .accept_bi()
                    .await
                    .map_err(|e| anyhow::anyhow!("accept control stream: {e:?}"))?;
                Ok(Accepted::Stream(CtlSend::Web(send), CtlRecv::Web(recv)))
            }
        }
    }

    /// The quinn connection, for clipboard, whose transfers are quinn streams.
    pub(crate) fn as_quic(&self) -> Option<&quinn::Connection> {
        match self {
            SessionLink::Quic(c) => Some(c),
            SessionLink::Web(..) => None,
        }
    }

    /// The device this session is keyed by: the client certificate's fingerprint on the native
    /// plane, the admitted device key's on the browser plane. `None` for an anonymous client.
    pub(crate) fn peer_fingerprint(&self) -> Option<[u8; 32]> {
        match self {
            SessionLink::Quic(c) => punktfunk_core::quic::endpoint::peer_fingerprint(c),
            SessionLink::Web(_, fp) => *fp,
        }
    }

    /// The plane events and session rows name this session by.
    pub(crate) fn plane(&self) -> crate::events::Plane {
        match self {
            SessionLink::Quic(_) => crate::events::Plane::Native,
            SessionLink::Web(..) => crate::events::Plane::Web,
        }
    }

    /// Whether a browser is on the other end. For the few decisions that really are about the
    /// carrier: there is no second UDP plane to punch, and the capabilities that ride quinn
    /// streams are not on offer.
    pub(crate) fn is_web(&self) -> bool {
        matches!(self, SessionLink::Web(..))
    }
}

/// What became of one datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatagramSend {
    Sent,
    /// Over this path's datagram ceiling. The frame is lost; the plane carries on, and a caller
    /// that sees these should be resizing what it sends rather than retrying.
    TooLarge,
    /// The peer will take no more datagrams for the rest of the connection.
    Unavailable,
}

impl DatagramSend {
    pub(crate) fn is_sent(self) -> bool {
        self == DatagramSend::Sent
    }
}

/// Why a connection ended, in the terms callers actually branch on: our own close codes ride an
/// application close, and a timeout is the one transport failure worth telling apart from the
/// rest. Everything else is noise for a log line.
#[derive(Clone, Debug)]
pub(crate) enum LinkClosed {
    /// The peer closed with an application code — where `QUIT_CODE` and the reject codes live.
    App {
        code: u64,
        reason: String,
    },
    /// The path went quiet. Distinguished because a client that timed out did not choose to leave.
    TimedOut,
    Other(String),
}

impl LinkClosed {
    /// The application close code, if that is how it ended.
    pub(crate) fn app_code(&self) -> Option<u64> {
        match self {
            LinkClosed::App { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Did the peer close with this application code?
    pub(crate) fn closed_with(&self, code: u32) -> bool {
        self.app_code() == Some(u64::from(code))
    }
}

impl From<quinn::ConnectionError> for LinkClosed {
    fn from(e: quinn::ConnectionError) -> LinkClosed {
        match e {
            quinn::ConnectionError::ApplicationClosed(ref ac) => LinkClosed::App {
                code: ac.error_code.into_inner(),
                reason: String::from_utf8_lossy(&ac.reason).into_owned(),
            },
            quinn::ConnectionError::TimedOut => LinkClosed::TimedOut,
            other => LinkClosed::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for LinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkClosed::App { code, reason } if reason.is_empty() => {
                write!(f, "closed by peer (code {code})")
            }
            LinkClosed::App { code, reason } => write!(f, "closed by peer (code {code}): {reason}"),
            LinkClosed::TimedOut => f.write_str("timed out"),
            LinkClosed::Other(s) => f.write_str(s),
        }
    }
}

impl From<quinn::Connection> for SessionLink {
    fn from(c: quinn::Connection) -> SessionLink {
        SessionLink::Quic(c)
    }
}
