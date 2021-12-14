use futures_util::StreamExt;
use http::{header::HeaderName, HeaderMap, HeaderValue, Uri};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    str::FromStr,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::codec::Decoder;

use crate::{upgrade, Connector, Error, MaybeTlsStream, Role, WebsocketStream};

pub(crate) fn make_key(key: Option<[u8; 16]>, key_base64: &mut [u8; 24]) {
    let key_bytes = key.unwrap_or_else(rand::random);

    base64::encode_config_slice(&key_bytes, base64::STANDARD, key_base64);
}

fn default_port(uri: &Uri) -> Option<u16> {
    if let Some(port) = uri.port_u16() {
        return Some(port);
    }

    let scheme = uri.scheme_str();

    match scheme {
        Some("https" | "wss") => Some(443),
        Some("http" | "ws") => Some(80),
        _ => None,
    }
}

fn build_request(uri: &Uri, key: &[u8], headers: &HeaderMap) -> Vec<u8> {
    let mut buf = Vec::new();

    buf.extend_from_slice(b"GET ");
    buf.extend_from_slice(uri.path().as_bytes());

    if let Some(query) = uri.query() {
        buf.extend_from_slice(b"?");
        buf.extend_from_slice(query.as_bytes());
    }

    buf.extend_from_slice(b" HTTP/1.1\r\n");

    if let Some(host) = uri.host() {
        buf.extend_from_slice(b"Host: ");
        buf.extend_from_slice(host.as_bytes());

        if let Some(port) = default_port(uri) {
            buf.extend_from_slice(b":");
            buf.extend_from_slice(port.to_string().as_bytes());
        }

        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ");
    buf.extend_from_slice(key);
    buf.extend_from_slice(b"\r\nSec-WebSocket-Version: 13\r\n");

    for (name, value) in headers {
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"\r\n");

    buf
}

async fn resolve(host: String, port: u16) -> Result<SocketAddr, Error> {
    let task = tokio::task::spawn_blocking(move || {
        (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or(Error::CannotResolveHost)
    });

    task.await.expect("Tokio threadpool failed")
}

/// Builder for websocket client connections.
pub struct Builder {
    uri: Uri,
    connector: Option<Connector>,
    headers: HeaderMap,
}

impl Builder {
    /// Creates a [`Builder`] that connects to a given URI.
    ///
    /// # Errors
    ///
    /// This method returns an [`Err`] result if URL parsing fails.
    pub fn new(uri: &str) -> Result<Self, http::uri::InvalidUri> {
        Ok(Self::from_uri(Uri::from_str(uri)?))
    }

    /// Creates a [`Builder`] that connects to a given URI.
    ///
    /// This method never fails as the URI has already been parsed.
    #[must_use]
    pub fn from_uri(uri: Uri) -> Self {
        Self {
            uri,
            connector: None,
            headers: HeaderMap::new(),
        }
    }

    /// Sets the SSL connector for the client.
    /// By default, the client will create a new one for each connection instead of reusing one.
    pub fn set_connector(&mut self, connector: Connector) {
        self.connector = Some(connector);
    }

    /// Adds an extra HTTP header to the handshake request.
    pub fn add_header(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.insert(name, value);
    }

    /// Establishes a connection to the websocket server.
    ///
    /// # Errors
    ///
    /// This method returns an [`Error`] if connecting to the server fails.
    pub async fn connect(mut self) -> Result<WebsocketStream<MaybeTlsStream<TcpStream>>, Error> {
        let host = self.uri.host().ok_or(Error::CannotResolveHost)?;
        let port = default_port(&self.uri).unwrap_or(80);
        let addr = resolve(host.to_string(), port).await?;

        let stream = TcpStream::connect(&addr).await?;

        let connector = if let Some(connector) = self.connector.take() {
            connector
        } else if self.uri.scheme_str() == Some("wss") {
            Connector::new()?
        } else {
            Connector::Plain
        };

        let stream = connector.wrap(host, stream).await?;

        self.connect_on(stream).await
    }

    /// Takes over an already established stream and uses it to send and receive websocket messages.
    ///
    /// This method assumes that the TLS connection has already been established, if needed. It sends an HTTP
    /// upgrade request and waits for an HTTP OK response before proceeding.
    ///
    /// # Errors
    ///
    /// This method returns an [`Error`] if writing or reading from the stream fails.
    pub async fn connect_on<S: AsyncRead + AsyncWrite + Unpin>(
        self,
        mut stream: S,
    ) -> Result<WebsocketStream<S>, Error> {
        let mut key_base64 = [0; 24];
        make_key(None, &mut key_base64);

        let upgrade_codec = upgrade::Codec::new(&key_base64);
        let request = build_request(&self.uri, &key_base64, &self.headers);
        AsyncWriteExt::write_all(&mut stream, &request).await?;

        let (opt, framed) = upgrade_codec.framed(stream).into_future().await;
        opt.ok_or(Error::NoUpgradeResponse)??;

        Ok(WebsocketStream::from_framed(framed, Role::Client))
    }
}
