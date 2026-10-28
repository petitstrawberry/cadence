//! Synthetic end-to-end mutual TLS checks. Never access Keychain.
use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
};

#[test]
fn client_identity_authenticates_both_https_and_websocket() {
    const ROOT: &[u8] = include_bytes!("../tests/fixtures/mtls/root.der");
    const CLIENT: &[u8] = include_bytes!("../tests/fixtures/mtls/client.p12");
    const PASSWORD: &str = "test-fixture-only";
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ROOT.to_vec())).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots.clone()),
        provider.clone(),
    )
    .build()
    .unwrap();
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("../tests/fixtures/mtls/server.der").to_vec(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("../tests/fixtures/mtls/server-key.der").to_vec(),
            )),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let server = Arc::new(server);
        for websocket in [false, true] {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let connection = rustls::ServerConnection::new(server.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, socket);
            if websocket {
                let mut socket = tungstenite::accept(tls).unwrap();
                assert!(
                    socket
                        .get_ref()
                        .conn
                        .peer_certificates()
                        .is_some_and(|certs| !certs.is_empty())
                );
                assert_eq!(socket.read().unwrap().into_text().unwrap(), "fixture");
                socket
                    .send(tungstenite::Message::text("authenticated"))
                    .unwrap();
            } else {
                let mut headers = Vec::new();
                let mut byte = [0u8; 1];
                while !headers.ends_with(b"\r\n\r\n") {
                    tls.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 8192);
                }
                assert!(
                    tls.conn
                        .peer_certificates()
                        .is_some_and(|certs| !certs.is_empty())
                );
                tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .unwrap();
                tls.flush().unwrap();
            }
        }
    });
    assert!(decode_identity(CLIENT, "wrong-fixture-password").is_err());
    let identity = decode_identity(CLIENT, PASSWORD).unwrap();
    let tls = tls_config(&identity, roots).unwrap();
    let transport = Transport {
        tls: Some(Arc::new(tls)),
        identity_origin: Some(Url::parse(&format!("https://{address}")).unwrap()),
    };
    let response = transport
        .http_agent()
        .get(&format!("https://{address}/media/tracks/test"))
        .call()
        .unwrap();
    assert_eq!(response.into_string().unwrap(), "ok");
    let stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (mut websocket, _) = tungstenite::client_tls_with_config(
        format!("wss://{address}/ws"),
        stream,
        None,
        transport.ws_connector(),
    )
    .unwrap();
    websocket
        .send(tungstenite::Message::text("fixture"))
        .unwrap();
    assert_eq!(
        websocket.read().unwrap().into_text().unwrap(),
        "authenticated"
    );
    handle.join().unwrap();
}
