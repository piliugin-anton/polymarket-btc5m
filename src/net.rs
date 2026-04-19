//! Proxy-aware networking primitives shared by the REST and WS clients.
//!
//! Polymarket geo-blocks many IPs at the edge, so every outbound connection
//! needs to be able to tunnel through a user-supplied proxy.
//!
//! The `POLYMARKET_PROXY` env var, if set, accepts:
//!   * `http://[user:pass@]host:port`
//!   * `https://[user:pass@]host:port`
//!   * `socks5://[user:pass@]host:port`
//!
//! REST goes through `reqwest::Proxy::all(...)` unchanged.
//!
//! WebSockets don't have built-in proxy support, so `ws_connect`:
//!   1. Opens a plain TCP socket to the proxy.
//!   2. For HTTP proxies: writes a CONNECT request, reads the 200 response,
//!      and the tunnel is now a transparent pipe to the origin.
//!      For SOCKS5 proxies: uses `tokio_socks::Socks5Stream` to do the
//!      handshake; the returned stream is likewise transparent.
//!   3. Layers TLS + WebSocket on top of that stream via
//!      `tokio_tungstenite::client_async_tls`.

use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::*;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::handshake::client::Response,
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

/// OS-level keepalive so idle TLS/WebSocket tunnels are probed before the peer times us out.
fn configure_tcp_keepalive(tcp: &TcpStream) -> Result<()> {
    use std::time::Duration;
    let sock = SockRef::from(tcp);
    let ka = TcpKeepalive::new().with_time(Duration::from_secs(30));
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::debug!(error = %e, "set_tcp_keepalive");
    }
    Ok(())
}

/// Resolve `wss`/`ws` URL to `host:port` and connect a plain TCP socket (then TLS happens in `client_async_tls`).
async fn ws_tcp_connect(url: &str) -> Result<TcpStream> {
    let target = Url::parse(url).context("parse ws URL")?;
    let host = target.host_str().context("ws url has no host")?;
    let port = target.port().unwrap_or_else(|| match target.scheme() {
        "wss" | "https" => 443,
        _ => 80,
    });
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("tcp connect to {addr}"))?;
    configure_tcp_keepalive(&tcp)?;
    Ok(tcp)
}

/// Read the proxy URL from env if present.
pub fn proxy_env() -> Option<String> {
    std::env::var("POLYMARKET_PROXY").ok().filter(|s| !s.is_empty())
}

/// Build a shared `reqwest::Client` with proxy support if configured.
///
/// * **HTTP/2 only:** TLS ALPN is set to `h2` so all requests use one multiplexed connection per
///   host where the server supports HTTP/2 (Polymarket APIs do). If TLS fails with “no protocol”,
///   the endpoint may be HTTP/1-only — remove `http2_prior_knowledge` in that case.
/// * **Pool:** `pool_idle_timeout(None)` keeps idle sockets in the pool; TCP keepalive below
///   catches dead peers.
/// * **HTTP/2 PING:** periodic pings while idle keep the HTTP/2 connection from going stale.
pub fn reqwest_client() -> Result<reqwest::Client> {
    use std::time::Duration;
    let mut b = reqwest::Client::builder()
        .user_agent("btc5m-bot/0.1")
        .timeout(Duration::from_secs(15))
        .http2_prior_knowledge()
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .pool_idle_timeout(None)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_keepalive_interval(Duration::from_secs(15))
        .tcp_keepalive_retries(3u32);
    if let Some(p) = proxy_env() {
        let proxy = reqwest::Proxy::all(&p).context("invalid POLYMARKET_PROXY for reqwest")?;
        b = b.proxy(proxy);
    }
    b.build().context("build reqwest client")
}

/// Open a WebSocket connection, routing through `POLYMARKET_PROXY` if set.
///
/// Returns the same tuple as `tokio_tungstenite::connect_async` so callers
/// don't need to change their pattern: `let (mut ws, _) = ws_connect(url).await?;`
pub async fn ws_connect(
    url: &str,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response)> {
    match proxy_env() {
        None => {
            let tcp = ws_tcp_connect(url).await?;
            tokio_tungstenite::client_async_tls(url, tcp)
                .await
                .map_err(|e| anyhow!("ws connect (tls handshake): {e}"))
        }
        Some(p) => {
            let proxy = Url::parse(&p).context("parse POLYMARKET_PROXY as URL")?;
            let target = Url::parse(url).context("parse ws URL")?;
            let host = target.host_str().context("ws url has no host")?;
            let port = target.port().unwrap_or_else(|| match target.scheme() {
                "wss" | "https" => 443,
                _ => 80,
            });
            let tcp = match proxy.scheme() {
                "http" | "https" => http_connect(&proxy, host, port).await?,
                "socks5" | "socks5h" => socks5_connect(&proxy, host, port).await?,
                other => bail!("unsupported proxy scheme '{other}' — use http, https, or socks5"),
            };
            configure_tcp_keepalive(&tcp)?;
            tokio_tungstenite::client_async_tls(url, tcp)
                .await
                .map_err(|e| anyhow!("ws handshake over proxy: {e}"))
        }
    }
}

// ── HTTP CONNECT tunnel ─────────────────────────────────────────────

async fn http_connect(proxy: &Url, host: &str, port: u16) -> Result<TcpStream> {
    let phost = proxy.host_str().context("proxy host")?;
    let pport = proxy.port_or_known_default().unwrap_or(8080);
    let mut tcp = TcpStream::connect((phost, pport))
        .await
        .with_context(|| format!("tcp connect to proxy {phost}:{pport}"))?;
    configure_tcp_keepalive(&tcp)?;

    // Build CONNECT request with optional basic auth.
    let auth = if proxy.username().is_empty() {
        String::new()
    } else {
        let cred = format!(
            "{}:{}",
            urldecode(proxy.username()),
            urldecode(proxy.password().unwrap_or("")),
        );
        format!("Proxy-Authorization: Basic {}\r\n", BASE64_STANDARD.encode(cred))
    };
    let req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}\r\n",
    );
    tcp.write_all(req.as_bytes()).await.context("write CONNECT")?;

    // Read response byte-by-byte so we stop *exactly* at \r\n\r\n and don't
    // accidentally consume bytes that belong to the upcoming TLS ClientHello.
    // (In practice the proxy won't send anything after the 200, but this
    // guarantees correctness regardless.)
    let mut response = Vec::with_capacity(256);
    let mut state = 0u8; // counts trailing \r\n\r\n bytes matched
    loop {
        let mut b = [0u8; 1];
        if tcp.read_exact(&mut b).await.is_err() {
            bail!("proxy closed during CONNECT (got {} bytes)", response.len());
        }
        response.push(b[0]);
        state = match (state, b[0]) {
            (0, b'\r') | (2, b'\r') => state + 1,
            (1, b'\n') | (3, b'\n') => state + 1,
            (_, b'\r') => 1,
            _ => 0,
        };
        if state == 4 { break; }
        if response.len() > 8192 { bail!("proxy response too large"); }
    }

    let text = String::from_utf8_lossy(&response);
    let first = text.lines().next().unwrap_or("");
    if !(first.starts_with("HTTP/1.1 200") || first.starts_with("HTTP/1.0 200")) {
        bail!("CONNECT rejected by proxy: {first}");
    }
    // `tcp` is now a transparent tunnel to origin.
    Ok(tcp)
}

// ── SOCKS5 handshake ────────────────────────────────────────────────

async fn socks5_connect(proxy: &Url, host: &str, port: u16) -> Result<TcpStream> {
    let phost = proxy.host_str().context("proxy host")?;
    let pport = proxy.port().unwrap_or(1080);
    let psock = format!("{phost}:{pport}");

    let s5 = if proxy.username().is_empty() {
        tokio_socks::tcp::Socks5Stream::connect(psock.as_str(), (host, port))
            .await
            .context("SOCKS5 connect")?
    } else {
        tokio_socks::tcp::Socks5Stream::connect_with_password(
            psock.as_str(),
            (host, port),
            &urldecode(proxy.username()),
            &urldecode(proxy.password().unwrap_or("")),
        )
        .await
        .context("SOCKS5 connect (auth)")?
    };
    Ok(s5.into_inner())
}

// Small url-decoder so creds in the proxy URL can contain escaped chars
// (e.g. passwords containing `@` or `:`). Kept inline to avoid another dep.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    if !bytes.contains(&b'%') { return s.to_string(); }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nib(bytes[i+1]), hex_nib(bytes[i+2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nib(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
