//! Shared in-process QUIC loopback plumbing for the quic submodule tests.
use super::endpoint;

/// Stand up two loopback quinn endpoints, connect, and return
/// `(server_ep, client_ep, host_conn, client_conn)`. Both endpoints are returned so the caller
/// keeps them in scope — dropping a `quinn::Endpoint` tears down its connections.
pub(crate) async fn connect_pair() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = endpoint::client_insecure().unwrap();
    let accept = tokio::spawn(async move {
        let incoming = server.accept().await.expect("incoming connection");
        let conn = incoming.await.expect("host side connects");
        (server, conn)
    });
    let client_conn = client
        .connect(addr, "punktfunk")
        .unwrap()
        .await
        .expect("client side connects");
    let (server, host_conn) = accept.await.unwrap();
    (server, client, host_conn, client_conn)
}
