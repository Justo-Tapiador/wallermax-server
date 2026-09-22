//! The manager's endpoint client — a tiny blocking `GET` behind the
//! dashboard's health, metrics and stats numbers, fluent in both of the
//! local server's dialects: plain `http://` and TLS-serving `https://`.
//!
//! The client is hand-rolled on purpose (no async runtime, no reqwest):
//! the manager only ever talks to the **local** server. Three behaviours
//! the dashboard depends on live here:
//!
//! - **`https://` origins talk TLS** — a server with `[tls]` enabled
//!   serves its real endpoints over TLS, and the manager reads them
//!   like any browser would.
//! - **Redirects are followed** — that same server answers its optional
//!   plain-HTTP listener with `308 Permanent Redirect` to the HTTPS
//!   equivalent, so an `http://` origin pointing there still reaches
//!   the real endpoints. Redirects are only followed on the **same
//!   host** (a redirect naming another host is refused, not followed),
//!   and never more than [`MAX_REDIRECTS`] times.
//! - **The certificate is accepted without validation** — the server's
//!   TLS certificate is a local development certificate no list of
//!   trusted roots knows, and the manager is local tooling reading
//!   public endpoints: the session is a pipe, not a proof. It carries
//!   no secrets and authenticates nothing.
//!
//! Complaints are the dashboard's banners: every one names the URL it
//! failed on and, where a guess is possible, the way out — a port that
//! answers TLS to an `http://` origin, a handshake against a port that
//! is not the TLS listener, a redirect that leads elsewhere.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

/// How many redirects [`http_get_text`] follows before giving up. The
/// plain-to-TLS hop costs one; anything past three is a loop.
const MAX_REDIRECTS: usize = 3;

/// Performs a blocking `GET` against `url` with a hard `timeout`, and
/// returns the body of a 2xx answer.
///
/// `http://` and `https://` are both understood (port-less authorities
/// default to 80 and 443), and up to [`MAX_REDIRECTS`] same-host
/// redirects are followed — including the server's plain-to-TLS `308`.
///
/// # Errors
///
/// A human-readable message for URL problems, connection failures,
/// timeouts, TLS handshake trouble, redirect trouble, non-2xx statuses
/// and truncated answers.
pub fn http_get_text(url: &str, timeout: Duration) -> Result<String, String> {
    let mut target = parse_url(url)?;
    let mut trail = target.url.clone();
    for _ in 0..=MAX_REDIRECTS {
        match fetch_once(&target, timeout)? {
            Answer::Body(body) => return Ok(body),
            Answer::Redirect { status, location } => {
                let next = resolve_location(&target, &location)?;
                if !same_host(&target, &next) {
                    return Err(format!(
                        "GET {url} answered {status} pointing at another host \
                         (`{location}`) — only same-host redirects are followed"
                    ));
                }
                trail.push_str(" -> ");
                trail.push_str(&next.url);
                target = next;
            }
        }
    }
    Err(format!(
        "GET {url} redirected more than {MAX_REDIRECTS} times: {trail}"
    ))
}

/// One parsed endpoint URL: scheme, authority and path.
#[derive(Debug)]
struct Target {
    /// The URL this target came from, quoted verbatim in complaints.
    url: String,
    /// Whether the scheme was `https://`.
    tls: bool,
    /// `host` or `host:port`, possibly a bracketed IPv6 literal.
    authority: String,
    /// Request path with query, always starting with `/`.
    path: String,
}

/// What one round trip produced.
enum Answer {
    /// A 2xx answer's body.
    Body(String),
    /// A 3xx answer naming where to look next.
    Redirect { status: u16, location: String },
}

/// Parses `http://host[:port][/path]` or `https://host[:port][/path]`.
fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!(
            "the manager speaks `http://` and `https://` to the local server; \
             `{url}` matches neither"
        ));
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::from("/")),
    };
    if authority.is_empty() {
        return Err(format!("no host in `{url}`"));
    }
    Ok(Target {
        url: url.to_owned(),
        tls,
        authority: authority.to_owned(),
        path,
    })
}

/// Resolves a redirect `Location` against the target it came from:
/// absolute URLs as themselves, scheme-relative (`//host/path`) and
/// path-relative forms against the current scheme, host and directory.
fn resolve_location(current: &Target, location: &str) -> Result<Target, String> {
    let location = location.trim();
    if location.starts_with("http://") || location.starts_with("https://") {
        return parse_url(location);
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = if current.tls { "https://" } else { "http://" };
        return parse_url(&format!("{scheme}{rest}"));
    }
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        // Relative to the current path's directory (`/api/one` + `two`
        // -> `/api/two`), the RFC 3986 resolution for no-scheme forms.
        let directory = current
            .path
            .rsplit_once('/')
            .map(|(directory, _)| directory)
            .unwrap_or("");
        format!("{directory}/{location}")
    };
    let scheme = if current.tls { "https:" } else { "http:" };
    Ok(Target {
        url: format!("{scheme}//{}{path}", current.authority),
        tls: current.tls,
        authority: current.authority.clone(),
        path,
    })
}

/// Whether two targets name the same host (the port may differ — the
/// plain-to-TLS redirect lands on another port of the same machine).
fn same_host(one: &Target, other: &Target) -> bool {
    host_of(&one.authority).eq_ignore_ascii_case(host_of(&other.authority))
}

/// The host part of an authority — `127.0.0.1` from `127.0.0.1:8080`,
/// `::1` from `[::1]:8080`, bare IPv6 addresses unchanged.
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(authority);
    }
    if authority.matches(':').count() > 1 {
        return authority;
    }
    authority.split(':').next().unwrap_or(authority)
}

/// One round trip: connect (TLS when the target says so), send the
/// `GET`, read the answer, and classify it as a body or a redirect.
fn fetch_once(target: &Target, timeout: Duration) -> Result<Answer, String> {
    let default_port: u16 = if target.tls { 443 } else { 80 };
    let mut stream = connect(&target.authority, default_port)
        .map_err(|error| format!("could not reach {}: {error}", target.url))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("could not arm the read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not arm the write timeout: {error}"))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: wallermax-manager\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        target.path, target.authority
    );

    let mut reader: Box<dyn Read> = if target.tls {
        let mut session = tls_wrap(host_of(&target.authority), stream)
            .map_err(|error| format!("could not reach {}: {error}", target.url))?;
        session.write_all(request.as_bytes()).map_err(|error| {
            format!(
                "could not talk TLS to {}: {error} — an `https://` origin needs \
                     the server's TLS listener",
                target.url
            )
        })?;
        Box::new(session)
    } else {
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("could not write to {}: {error}", target.url))?;
        Box::new(stream)
    };

    let mut response = Vec::new();
    read_until(&mut reader, &mut response, contains_head, &target.url)?;
    if find_head(&response).is_none() {
        if looks_like_tls(&response) {
            return Err(format!(
                "GET {} did not answer HTTP — that port is speaking TLS; \
                 an `https://` origin is needed",
                target.url
            ));
        }
        return Err(format!("truncated answer from {}", target.url));
    }
    let head_end = find_head(&response).expect("checked a moment ago");
    let head = String::from_utf8_lossy(&response[..head_end]).into_owned();
    let parsed = parse_head(&head, &target.url)?;

    // The body: exactly Content-Length bytes when the header says so
    // (which never leans on a well-behaved shutdown), otherwise
    // whatever arrives until the connection ends.
    if let Some(length) = parsed.content_length {
        let want = head_end + 4 + length;
        read_until(
            &mut reader,
            &mut response,
            |buf| buf.len() >= want,
            &target.url,
        )?;
    } else {
        read_until(&mut reader, &mut response, |_| false, &target.url)?;
    }

    let body = &response[(head_end + 4).min(response.len())..];
    let body = if parsed.chunked {
        dechunk(&String::from_utf8_lossy(body))
    } else {
        String::from_utf8_lossy(body).into_owned()
    };
    match parsed.status {
        200..300 => Ok(Answer::Body(body)),
        300..400 => parsed
            .location
            .map(|location| Answer::Redirect {
                status: parsed.status,
                location,
            })
            .ok_or_else(|| {
                format!(
                    "GET {} answered {} without a Location to follow",
                    target.url, parsed.status
                )
            }),
        status => Err(format!("GET {} answered {status}", target.url)),
    }
}

/// The parts of a response head the client cares about.
struct Head {
    status: u16,
    location: Option<String>,
    content_length: Option<usize>,
    chunked: bool,
}

/// Parses the status line and the Location / Content-Length /
/// Transfer-Encoding headers out of a response head.
fn parse_head(head: &str, url: &str) -> Result<Head, String> {
    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line from {url}: {status_line:?}"))?;
    let mut parsed = Head {
        status,
        location: None,
        content_length: None,
        chunked: false,
    };
    for header in head.lines().skip(1) {
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("location") {
            parsed.location = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            parsed.content_length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            parsed.chunked = true;
        }
    }
    Ok(parsed)
}

/// Reads into `response` until `done` is satisfied (checked before
/// every read), the peer closes the connection, or the transport
/// complains.
///
/// A transport error **after** a complete head arrived is treated as
/// the end of the answer: servers that slam the socket shut without
/// the TLS `close_notify` courtesy still delivered a parseable
/// response, and the Content-Length / chunk decoder below — not the
/// socket — is the judge of what is usable.
fn read_until(
    reader: &mut dyn Read,
    response: &mut Vec<u8>,
    done: impl Fn(&[u8]) -> bool,
    url: &str,
) -> Result<(), String> {
    while !done(response) {
        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => response.extend_from_slice(&chunk[..read]),
            Err(error) => {
                if contains_head(response) {
                    return Ok(());
                }
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    return Err(format!("{url} did not answer within the timeout"));
                }
                return Err(format!("could not read from {url}: {error}"));
            }
        }
    }
    Ok(())
}

/// Whether the buffer holds a complete response head.
fn contains_head(buf: &[u8]) -> bool {
    find_head(buf).is_some()
}

/// The offset of the head/body boundary (`\r\n\r\n`), when complete.
fn find_head(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Whether the answer starts like a TLS record instead of an HTTP
/// response (`H`) — the shape a TLS port answers a plain `GET` with,
/// usually a fatal alert: a record type byte (20-23) followed by a
/// version byte of 3.
fn looks_like_tls(response: &[u8]) -> bool {
    matches!(response.first(), Some(20..=23)) && response.get(1) == Some(&3)
}

/// Wraps `stream` in a TLS session with `host`.
///
/// The certificate is deliberately **not** validated — see the module
/// header for why that is the right default for local tooling.
fn tls_wrap(
    host: &str,
    stream: TcpStream,
) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
    let name = ServerName::try_from(host.to_owned())
        .map_err(|error| format!("`{host}` is not a name TLS can talk to: {error}"))?;
    let connection = ClientConnection::new(endpoint_client_config(), name)
        .map_err(|error| format!("could not start the TLS session with `{host}`: {error}"))?;
    Ok(StreamOwned::new(connection, stream))
}

/// The shared client configuration: any certificate answers. Built
/// once — it is stateless per connection.
fn endpoint_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider carries its own safe versions");
            Arc::new(
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(TrustTheLocalServer))
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// A [`ServerCertVerifier`] that accepts whatever the local server
/// presents — the certificate of a development TLS setup no root store
/// knows, read over a connection that carries no secrets.
#[derive(Debug)]
struct TrustTheLocalServer;

impl ServerCertVerifier for TrustTheLocalServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

/// Splits an authority into its host and port, defaulting the port to
/// `default_port` (80 for `http://`, 443 for `https://`).
///
/// Understands every shape a URL can carry: `host`, `host:port`,
/// bracketed IPv6 (`[::1]:8080`) and bare IPv6 (`::1` — the colons
/// belong to the address, they are not separators).
fn host_port(authority: &str, default_port: u16) -> Result<(&str, u16), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[::1]` or `[::1]:8080`.
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated IPv6 address in `{authority}`"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port,
            None if tail.is_empty() => "",
            None => {
                return Err(format!(
                    "garbage after the IPv6 address in `{authority}`: `{tail}`"
                ))
            }
        };
        (host, port)
    } else if authority.matches(':').count() > 1 {
        // Bare IPv6: every colon belongs to the address itself.
        (authority, "")
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, port),
            None => (authority, ""),
        }
    };
    let port = if port.is_empty() {
        default_port
    } else {
        port.parse::<u16>()
            .map_err(|_| format!("invalid port in `{authority}`"))?
    };
    Ok((host, port))
}

/// Resolves `host:port` to every address it names.
///
/// The host and the port are resolved **as a pair**: resolving the bare
/// string only works when the port is spelled out, so an origin like
/// `http://localhost` used to die with `invalid socket address` no
/// matter whether the server was running — the bug behind a dashboard
/// banner that no amount of Refresh ever cleared.
fn resolve_all(authority: &str, default_port: u16) -> Result<Vec<SocketAddr>, String> {
    let (host, port) = host_port(authority, default_port)?;
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve `{authority}`: {error}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("no address answered for `{authority}`"));
    }
    Ok(addresses)
}

/// Connects to the first address `authority` names that accepts the
/// connection. `localhost` can name both `::1` and `127.0.0.1`, and a
/// server that binds only one of them still answers.
fn connect(authority: &str, default_port: u16) -> Result<TcpStream, String> {
    let addresses = resolve_all(authority, default_port)?;
    let mut last = String::new();
    for address in &addresses {
        match TcpStream::connect_timeout(address, Duration::from_secs(2)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = format!("{address}: {error}"),
        }
    }
    Err(format!(
        "every address of `{authority}` refused the connection — last tried {last}"
    ))
}

/// Reassembles a chunked body. Tolerates a trailing truncated chunk
/// (the connection closed early) by returning what was decoded so far.
fn dechunk(body: &str) -> String {
    let mut decoded = String::new();
    let mut rest = body;
    while let Some((size_line, remainder)) = rest.split_once('\n') {
        let Ok(size) = usize::from_str_radix(size_line.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let cut = remainder.len().min(size);
        decoded.push_str(&remainder[..cut]);
        // Skip the chunk terminator: CRLF per the standard, a bare LF
        // tolerated; anything else means a truncated answer — keep what
        // we have.
        let tail = &remainder[cut..];
        rest = match tail
            .strip_prefix("\r\n")
            .or_else(|| tail.strip_prefix('\n'))
        {
            Some(tail) => tail,
            None => break,
        };
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Reads one HTTP request head (or, against a plain port, whatever
    /// the TLS handshake sent first) — bounded, so a test server never
    /// hangs on unexpected bytes.
    fn read_request<S: Read>(stream: &mut S) -> String {
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while seen.len() < 512 && !seen.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => seen.push(byte[0]),
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    /// Serves exactly one plain-HTTP connection: reads the request,
    /// writes `response`, closes.
    fn serve_one_plain(response: &'static str) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = read_request(&mut stream);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(address)
    }

    /// Serves exactly one TLS connection answering `response` — the
    /// exact shape of the server's development TLS (rustls with a
    /// freshly generated self-signed certificate).
    fn serve_one_tls(response: &'static str) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let Ok(session) = tls_server_session() else {
                return;
            };
            let mut session = StreamOwned::new(session, stream);
            let _ = read_request(&mut session);
            let _ = session.write_all(response.as_bytes());
            let _ = session.flush();
        });
        Ok(address)
    }

    /// A server session with a fresh self-signed certificate.
    fn tls_server_session() -> Result<rustls::ServerConnection, Box<dyn std::error::Error>> {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
        let certificate = CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::try_from(
            certified.signing_key.serialize_der().to_vec(),
        )?;
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)?;
        Ok(rustls::ServerConnection::new(Arc::new(config))?)
    }

    #[test]
    fn plain_get_reads_the_body() {
        let address = serve_one_plain(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .expect("bind");
        let body = http_get_text(&format!("http://{address}/x"), Duration::from_secs(5))
            .expect("the body answers");
        assert_eq!(body, "hello");
    }

    #[test]
    fn https_talks_tls_and_accepts_a_self_signed_certificate() {
        let address =
            serve_one_tls("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                .expect("bind");
        let body = http_get_text(&format!("https://{address}/health"), Duration::from_secs(5))
            .expect("TLS with any certificate");
        assert_eq!(body, "hello");
    }

    #[test]
    fn the_servers_plain_to_tls_redirect_is_followed() {
        // The exact production shape: an `http://` origin whose
        // listener answers 308 -> `https://<same host>:<tls port><path>`
        // — the server's optional plain-HTTP redirect listener.
        let tls_address =
            serve_one_tls("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("bind");
        let redirect = format!(
            "HTTP/1.1 308 Permanent Redirect\r\nLocation: https://{tls_address}/health\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let plain_address = serve_one_plain(Box::leak(redirect.into_boxed_str())).expect("bind");
        let body = http_get_text(
            &format!("http://{plain_address}/health"),
            Duration::from_secs(5),
        )
        .expect("the redirect leads to the TLS endpoint");
        assert_eq!(body, "ok");
    }

    #[test]
    fn redirects_to_another_host_are_refused() {
        let address = serve_one_plain(
            "HTTP/1.1 308 Permanent Redirect\r\nLocation: https://elsewhere.example/health\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("bind");
        let error = http_get_text(&format!("http://{address}/health"), Duration::from_secs(5))
            .expect_err("another host is not followed");
        assert!(error.contains("another host"), "names the refusal: {error}");
    }

    #[test]
    fn a_redirect_without_location_names_what_is_missing() {
        let address = serve_one_plain(
            "HTTP/1.1 308 Permanent Redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("bind");
        let error = http_get_text(&format!("http://{address}/health"), Duration::from_secs(5))
            .expect_err("nowhere to go");
        assert!(error.contains("308"), "names the status: {error}");
        assert!(error.contains("Location"), "names what is missing: {error}");
    }

    #[test]
    fn redirect_loops_are_cut_short() {
        // A listener that always redirects to itself: the hop budget
        // must run out, not the patience.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            for _ in 0..10 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = read_request(&mut stream);
                let answer = format!(
                    "HTTP/1.1 308 Permanent Redirect\r\nLocation: http://{address}/health\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(answer.as_bytes());
                let _ = stream.flush();
            }
        });
        let error = http_get_text(&format!("http://{address}/health"), Duration::from_secs(5))
            .expect_err("the loop must be cut");
        assert!(error.contains("redirected"), "names the loop: {error}");
    }

    #[test]
    fn a_tls_port_answering_a_plain_get_gets_the_hint() {
        // The exact shape of a TLS port's answer to a plain GET: a
        // fatal alert record — no HTTP head anywhere in sight.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = read_request(&mut stream);
            // A fatal alert record: type 21 (alert), version 3, then
            // the alert itself — what a TLS listener answers when a
            // plain `GET` arrives.
            let _ = stream.write_all(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]);
            let _ = stream.flush();
        });
        let error = http_get_text(&format!("http://{address}/health"), Duration::from_secs(5))
            .expect_err("plain HTTP against TLS is explained");
        assert!(error.contains("TLS"), "points at TLS: {error}");
    }

    #[test]
    fn an_https_origin_against_a_plain_port_explains_itself() {
        let address =
            serve_one_plain("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("bind");
        let error = http_get_text(&format!("https://{address}/health"), Duration::from_secs(5))
            .expect_err("TLS against a plain port fails");
        assert!(error.contains("TLS"), "points at TLS: {error}");
    }

    #[test]
    fn chunked_answers_are_reassembled() {
        let address = serve_one_plain(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
        )
        .expect("bind");
        let body = http_get_text(&format!("http://{address}/metrics"), Duration::from_secs(5))
            .expect("chunked is fine");
        assert_eq!(body, "Wikipedia");
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        assert_eq!(dechunk("4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n"), "Wikipedia");
    }

    #[test]
    fn port_less_authorities_default_to_their_schemes_port() {
        let (host, port) = host_port("localhost", 443).expect("a port-less authority");
        assert_eq!(host, "localhost");
        assert_eq!(port, 443);
        let addresses = resolve_all("localhost", 443).expect("localhost resolves");
        assert!(
            addresses.iter().all(|address| address.port() == 443),
            "every candidate carries the default port: {addresses:?}"
        );
    }

    #[test]
    fn ipv6_authorities_keep_their_colons() {
        assert_eq!(
            host_port("[::1]:9000", 80).expect("bracketed"),
            ("::1", 9000)
        );
        assert_eq!(
            host_port("[::1]", 443).expect("bracketed, port-less"),
            ("::1", 443)
        );
        assert_eq!(host_port("::1", 80).expect("bare ipv6"), ("::1", 80));
        assert_eq!(
            host_port("127.0.0.1:8123", 80).expect("host and port"),
            ("127.0.0.1", 8123)
        );
        assert!(resolve_all("[::1]:9000", 80).is_ok());
    }

    #[test]
    fn malformed_authorities_are_rejected_by_name() {
        for authority in ["localhost:http", "[::1", "[::1]junk"] {
            let error = resolve_all(authority, 80).expect_err("must be rejected");
            assert!(
                error.contains(authority),
                "the complaint names `{authority}`: {error}"
            );
        }
    }

    #[test]
    fn urls_without_a_known_scheme_are_refused_by_name() {
        let error = parse_url("ftp://127.0.0.1:8080/metrics").expect_err("must be rejected");
        assert!(
            error.contains("ftp://127.0.0.1:8080/metrics"),
            "names the URL: {error}"
        );
    }

    #[test]
    fn relative_locations_resolve_against_the_current_target() {
        let current = parse_url("http://127.0.0.1:8080/api/one").expect("parses");
        assert_eq!(
            resolve_location(&current, "/health")
                .expect("an absolute path")
                .path,
            "/health"
        );
        assert_eq!(
            resolve_location(&current, "two")
                .expect("a relative path")
                .path,
            "/api/two"
        );
        let scheme_relative =
            resolve_location(&current, "//127.0.0.1:8080/health").expect("scheme-relative");
        assert!(!scheme_relative.tls);
        assert_eq!(scheme_relative.path, "/health");
        let absolute = resolve_location(&current, "https://127.0.0.1:9090/x").expect("absolute");
        assert!(absolute.tls);
        assert_eq!(absolute.authority, "127.0.0.1:9090");
    }

    #[test]
    fn same_host_ignores_the_port_but_not_the_name() {
        let plain = parse_url("http://127.0.0.1:8080/health").expect("parses");
        let tls = parse_url("https://127.0.0.1/health").expect("parses");
        assert!(same_host(&plain, &tls), "the redirect hop keeps the host");
        let elsewhere = parse_url("https://elsewhere.example/health").expect("parses");
        assert!(
            !same_host(&plain, &elsewhere),
            "another host is another host"
        );
    }
}
