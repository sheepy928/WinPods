//! RTSP/HTTP request formatting, response parsing and the RTSP client
//! (workstream: control plane).
//!
//! Byte-exact port of `pyatv/support/http.py` + `pyatv/support/rtsp.py` as used by
//! WinPods: CSeq starts at 0 (GET /info takes CSeq 0); headers CSeq, DACP-ID (u64
//! uppercase hex), Active-Remote (u32), Client-Instance (= DACP-ID), then extra
//! headers; `User-Agent: AirPlay/550.10`; URI `rtsp://<local ip>/<session id>`;
//! dict bodies are binary plists with `Content-Type: application/x-apple-binary-plist`;
//! 4 s response timeout. Pairing posts use HTTP/1.1 and no CSeq. Once pairing
//! completes, every byte in both directions goes through the HAP cipher.
//! Fixture: `tests/fixtures/rtsp_requests.json`.
//!
//! Timeout note: pyatv waits up to 10 s for the HTTP-level response and then up to
//! 4 s for the matching CSeq, so a slow (waking) receiver gets 10 s. The client keeps
//! that effective behaviour: [`REQUEST_TIMEOUT`] (10 s) covers sending a request and
//! receiving the response with its CSeq. Responses carrying another CSeq (late
//! replies to a request that already timed out) are skipped.
//!
//! Status handling follows `HttpConnection.send_and_receive`: 403 is an
//! authentication error, 401 is one unless errors are allowed, 2xx succeeds, and
//! anything else is a rejection unless errors are allowed.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, trace};

use crate::hap::{CipherKeys, HapError, HapReader, HapWriter, hap_pair};

/// User agent of RTSP requests (pyatv `rtsp.USER_AGENT`).
pub const USER_AGENT: &str = "AirPlay/550.10";
/// Protocol of session requests.
pub const RTSP_PROTOCOL: &str = "RTSP/1.0";
/// Protocol of the pairing posts.
pub const HTTP_PROTOCOL: &str = "HTTP/1.1";
/// Content type of binary plist bodies.
pub const BPLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
/// Time allowed for sending a request and receiving its response.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Size of one socket read.
const READ_CHUNK: usize = 16 * 1024;
/// Largest message body accepted (receiver bodies are a few KB).
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// Errors from formatting, parsing or exchanging RTSP/HTTP messages.
#[derive(Debug, thiserror::Error)]
pub enum RtspError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no response to {0} (timeout)")]
    Timeout(String),
    #[error("the receiver closed the connection")]
    Closed,
    #[error("{method} failed: not authenticated ({code})")]
    NotAuthenticated { method: String, code: u16 },
    #[error("{method} rejected with {code} {reason}")]
    Status {
        method: String,
        code: u16,
        reason: String,
    },
    #[error("malformed message: {0}")]
    Parse(String),
    #[error("could not encode plist body: {0}")]
    Plist(#[from] plist::Error),
    #[error("encrypted transport failed: {0}")]
    Hap(#[from] HapError),
}

/// Case-insensitive, order-preserving header list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// Value of `name` (case-insensitive). Like `CaseInsensitiveDict`, a header that
    /// appears more than once yields its last value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// True when `name` is present (case-insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// All `(name, value)` pairs in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of header lines.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when there are no headers.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A parsed response (`HttpResponse` in pyatv; the body is always kept as bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub protocol: String,
    pub version: String,
    pub code: u16,
    pub message: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    /// The `CSeq` header as a number, if present and numeric.
    pub fn cseq(&self) -> Option<u32> {
        self.headers.get("CSeq")?.trim().parse().ok()
    }

    /// Decode the body as a plist (binary or XML), like `decode_bplist_from_body`.
    pub fn body_plist(&self) -> Result<plist::Value, RtspError> {
        Ok(plist::Value::from_reader(std::io::Cursor::new(&self.body))?)
    }
}

/// A parsed request (`HttpRequest` in pyatv), as received on the event channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingRequest {
    pub method: String,
    pub path: String,
    pub protocol: String,
    pub version: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

/// Byte-exact port of pyatv's `_format_message`.
///
/// Order: request line, `User-Agent` (only if `headers` has none), `Content-Type`
/// (only if `content_type` is given), `Content-Length` (only for a non-empty body),
/// then `headers` in order, an empty line and the body.
pub fn format_message(
    method: &str,
    uri: &str,
    protocol: &str,
    user_agent: &str,
    content_type: Option<&str>,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut msg = format!("{method} {uri} {protocol}");
    if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("User-Agent"))
    {
        msg.push_str("\r\nUser-Agent: ");
        msg.push_str(user_agent);
    }
    if let Some(content_type) = content_type {
        msg.push_str("\r\nContent-Type: ");
        msg.push_str(content_type);
    }
    if !body.is_empty() {
        msg.push_str(&format!("\r\nContent-Length: {}", body.len()));
    }
    for (key, value) in headers {
        msg.push_str("\r\n");
        msg.push_str(key);
        msg.push_str(": ");
        msg.push_str(value);
    }
    msg.push_str("\r\n\r\n");
    let mut out = msg.into_bytes();
    out.extend_from_slice(body);
    out
}

/// One complete message split off a receive buffer.
struct RawMessage {
    first_line: String,
    headers: Headers,
    body: Vec<u8>,
    /// Bytes of the buffer the message occupied.
    used: usize,
}

/// Split one complete message off the front of `buf` (`_parse_http_message`), or
/// `None` while the message is incomplete.
fn parse_message(buf: &[u8]) -> Result<Option<RawMessage>, RtspError> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..end])
        .map_err(|_| RtspError::Parse("header is not UTF-8".into()))?;
    let mut lines = head.split("\r\n");
    let first_line = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once(": ")
            .ok_or_else(|| RtspError::Parse(format!("bad header line {line:?}")))?;
        headers.push((key.to_string(), value.to_string()));
    }
    let headers = Headers(headers);
    let content_length = match headers.get("Content-Length") {
        Some(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|_| RtspError::Parse(format!("bad Content-Length {value:?}")))?,
        None => 0,
    };
    if content_length > MAX_BODY {
        return Err(RtspError::Parse(format!(
            "Content-Length {content_length} exceeds {MAX_BODY}"
        )));
    }
    let body_start = end + 4;
    let used = body_start + content_length;
    let Some(body) = buf.get(body_start..used) else {
        return Ok(None);
    };
    Ok(Some(RawMessage {
        first_line,
        headers,
        body: body.to_vec(),
        used,
    }))
}

/// Take the leading run of characters matching `pred`.
fn take_while(s: &str, pred: impl Fn(char) -> bool) -> (&str, &str) {
    let end = s.find(|c| !pred(c)).unwrap_or(s.len());
    s.split_at(end)
}

/// Parse one response from the front of `buf`; `Ok(None)` = need more data.
/// The status line must look like `<proto>/<version> <code> <message>`.
pub fn parse_response(buf: &[u8]) -> Result<Option<(Response, usize)>, RtspError> {
    let Some(RawMessage {
        first_line,
        headers,
        body,
        used,
    }) = parse_message(buf)?
    else {
        return Ok(None);
    };
    let bad = || RtspError::Parse(format!("bad status line {first_line:?}"));
    let (protocol, rest) = first_line.split_once('/').ok_or_else(bad)?;
    let (version, rest) = take_while(rest, |c| c.is_ascii_digit() || c == '.');
    let rest = rest.strip_prefix(' ').ok_or_else(bad)?;
    let (code, rest) = take_while(rest, |c| c.is_ascii_digit());
    if protocol.is_empty() || version.is_empty() || code.is_empty() {
        return Err(bad());
    }
    let code: u16 = code.parse().map_err(|_| bad())?;
    // pyatv requires "<code> <message>"; tolerate a missing reason phrase.
    let message = rest.strip_prefix(' ').unwrap_or(rest).to_string();
    let response = Response {
        protocol: protocol.to_string(),
        version: version.to_string(),
        code,
        message,
        headers,
        body,
    };
    Ok(Some((response, used)))
}

/// Parse one request from the front of `buf`; `Ok(None)` = need more data.
/// The request line must start with `<METHOD> <path> <proto>/<version>`.
pub fn parse_request(buf: &[u8]) -> Result<Option<(IncomingRequest, usize)>, RtspError> {
    let Some(RawMessage {
        first_line,
        headers,
        body,
        used,
    }) = parse_message(buf)?
    else {
        return Ok(None);
    };
    let bad = || RtspError::Parse(format!("bad request line {first_line:?}"));
    let (method, rest) = take_while(&first_line, |c| c.is_ascii_uppercase() || c == '_');
    let rest = rest.strip_prefix(' ').ok_or_else(bad)?;
    let (path, rest) = take_while(rest, |c| c != ' ');
    let rest = rest.strip_prefix(' ').ok_or_else(bad)?;
    let (protocol, rest) = take_while(rest, |c| c != '/');
    let rest = rest.strip_prefix('/').ok_or_else(bad)?;
    let (version, _) = take_while(rest, |c| c.is_ascii_digit() || c == '.');
    if method.is_empty() || path.is_empty() || protocol.is_empty() || version.is_empty() {
        return Err(bad());
    }
    let request = IncomingRequest {
        method: method.to_string(),
        path: path.to_string(),
        protocol: protocol.to_string(),
        version: version.to_string(),
        headers,
        body,
    };
    Ok(Some((request, used)))
}

/// Per-connection identifiers that go into every RTSP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspContext {
    /// Local address of the connection (part of the URI).
    pub local_ip: Ipv4Addr,
    /// Random u32; also `streamConnectionID` and the RTP SSRC.
    pub session_id: u32,
    /// Random u64 as uppercase hex without padding (`DACP-ID`, `Client-Instance`).
    pub dacp_id: String,
    /// Random u32 (`Active-Remote`).
    pub active_remote: u32,
}

impl RtspContext {
    /// Fresh random identifiers, like pyatv's `RtspSession.__init__`.
    pub fn random(local_ip: Ipv4Addr) -> Result<RtspContext, getrandom::Error> {
        Ok(RtspContext {
            local_ip,
            session_id: getrandom::u32()?,
            dacp_id: format!("{:X}", getrandom::u64()?),
            active_remote: getrandom::u32()?,
        })
    }

    /// Request URI for session requests: `rtsp://<local ip>/<session id>`.
    pub fn uri(&self) -> String {
        format!("rtsp://{}/{}", self.local_ip, self.session_id)
    }
}

/// Body of an outgoing RTSP request.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Body {
    #[default]
    Empty,
    /// Raw bytes (the content type is set separately).
    Bytes(Vec<u8>),
    /// A dictionary sent as a binary plist; adds the plist `Content-Type` header.
    Plist(plist::Value),
}

/// An outgoing RTSP request (the arguments of `RtspSession.exchange`).
#[derive(Debug, Clone, PartialEq)]
pub struct RtspRequest {
    pub method: String,
    /// `None` = the session URI.
    pub uri: Option<String>,
    /// Sent as `Content-Type` before `Content-Length`.
    pub content_type: Option<String>,
    /// Extra headers, sent after the four standard ones.
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// Return non-2xx responses (except 403) instead of failing.
    pub allow_error: bool,
}

impl RtspRequest {
    /// A request for `method` on the session URI, without body or extra headers.
    pub fn new(method: &str) -> RtspRequest {
        RtspRequest {
            method: method.to_string(),
            uri: None,
            content_type: None,
            headers: Vec::new(),
            body: Body::Empty,
            allow_error: false,
        }
    }

    /// Use `uri` instead of the session URI.
    pub fn uri(mut self, uri: &str) -> RtspRequest {
        self.uri = Some(uri.to_string());
        self
    }

    /// Add an extra header.
    pub fn header(mut self, key: &str, value: impl Into<String>) -> RtspRequest {
        self.headers.push((key.to_string(), value.into()));
        self
    }

    /// Raw body with its content type.
    pub fn bytes(mut self, content_type: &str, body: Vec<u8>) -> RtspRequest {
        self.content_type = Some(content_type.to_string());
        self.body = Body::Bytes(body);
        self
    }

    /// Binary plist body.
    pub fn plist(mut self, body: plist::Value) -> RtspRequest {
        self.body = Body::Plist(body);
        self
    }

    /// Accept non-2xx responses.
    pub fn allow_error(mut self) -> RtspRequest {
        self.allow_error = true;
        self
    }
}

/// Encode a plist value the way pyatv sends dict bodies (binary format).
pub fn encode_bplist(value: &plist::Value) -> Result<Vec<u8>, RtspError> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out)?;
    Ok(out)
}

/// Format an RTSP request exactly like `RtspSession.exchange` + `_format_message`.
pub fn format_rtsp_request(
    ctx: &RtspContext,
    cseq: u32,
    request: &RtspRequest,
) -> Result<Vec<u8>, RtspError> {
    let mut headers = vec![
        ("CSeq".to_string(), cseq.to_string()),
        ("DACP-ID".to_string(), ctx.dacp_id.clone()),
        ("Active-Remote".to_string(), ctx.active_remote.to_string()),
        ("Client-Instance".to_string(), ctx.dacp_id.clone()),
    ];
    headers.extend(request.headers.iter().cloned());
    let body = match &request.body {
        Body::Empty => Vec::new(),
        Body::Bytes(bytes) => bytes.clone(),
        Body::Plist(value) => {
            headers.push(("Content-Type".to_string(), BPLIST_CONTENT_TYPE.to_string()));
            encode_bplist(value)?
        }
    };
    let uri = request.uri.clone().unwrap_or_else(|| ctx.uri());
    Ok(format_message(
        &request.method,
        &uri,
        RTSP_PROTOCOL,
        USER_AGENT,
        request.content_type.as_deref(),
        &headers,
        &body,
    ))
}

/// Format an HTTP/1.1 POST like `HttpConnection.post` (no CSeq or session headers).
pub fn format_http_post(path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    format_message(
        "POST",
        path,
        HTTP_PROTOCOL,
        USER_AGENT,
        None,
        &headers,
        body,
    )
}

/// Apply pyatv's status rules to a response.
fn check_status(
    method: &str,
    response: Response,
    allow_error: bool,
) -> Result<Response, RtspError> {
    match response.code {
        403 => Err(RtspError::NotAuthenticated {
            method: method.to_string(),
            code: 403,
        }),
        401 if !allow_error => Err(RtspError::NotAuthenticated {
            method: method.to_string(),
            code: 401,
        }),
        200..=299 => Ok(response),
        _ if allow_error => Ok(response),
        code => Err(RtspError::Status {
            method: method.to_string(),
            code,
            reason: response.message,
        }),
    }
}

/// RTSP client over one connection: assigns CSeqs, optionally HAP-encrypts, and
/// matches responses to requests. One request is in flight at a time.
pub struct RtspClient<S = TcpStream> {
    stream: S,
    ctx: RtspContext,
    cseq: u32,
    hap: Option<(HapWriter, HapReader)>,
    /// Received plaintext not yet parsed into a response.
    inbound: Vec<u8>,
    scratch: Vec<u8>,
    closed: bool,
}

impl<S> std::fmt::Debug for RtspClient<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtspClient")
            .field("ctx", &self.ctx)
            .field("cseq", &self.cseq)
            .field("encrypted", &self.hap.is_some())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> RtspClient<S> {
    /// Wrap a connected stream. The first request gets CSeq 0.
    pub fn new(stream: S, ctx: RtspContext) -> RtspClient<S> {
        RtspClient {
            stream,
            ctx,
            cseq: 0,
            hap: None,
            inbound: Vec::new(),
            scratch: vec![0; READ_CHUNK],
            closed: false,
        }
    }

    /// Identifiers used in requests.
    pub fn context(&self) -> &RtspContext {
        &self.ctx
    }

    /// CSeq the next RTSP request will carry.
    pub fn next_cseq(&self) -> u32 {
        self.cseq
    }

    /// True once the peer closed the connection or the transport failed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Encrypt everything from now on (output = our write key, input = read key).
    pub fn enable_encryption(&mut self, keys: &CipherKeys) {
        self.hap = Some(hap_pair(keys));
    }

    /// Send an RTSP request and wait for the response with the same CSeq.
    pub async fn exchange(&mut self, request: &RtspRequest) -> Result<Response, RtspError> {
        let cseq = self.cseq;
        self.cseq = self.cseq.wrapping_add(1);
        let bytes = format_rtsp_request(&self.ctx, cseq, request)?;
        let label = format!(
            "{} {} (CSeq {cseq})",
            request.method,
            request.uri.as_deref().unwrap_or("<session>")
        );
        debug!(method = %request.method, cseq, "RTSP request");
        let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.send(&bytes).await?;
            loop {
                let response = self.read_response().await?;
                if response.cseq() == Some(cseq) {
                    return Ok::<Response, RtspError>(response);
                }
                debug!(
                    expected = cseq,
                    got = ?response.cseq(),
                    "skipping response for another request"
                );
            }
        })
        .await
        .map_err(|_| RtspError::Timeout(label))??;
        trace!(code = response.code, cseq, "RTSP response");
        check_status(&request.method, response, request.allow_error)
    }

    /// Send an HTTP/1.1 POST (pairing) and wait for the next response. Does not use
    /// or advance the CSeq counter.
    pub async fn post_http(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, RtspError> {
        let bytes = format_http_post(path, headers, body);
        debug!(path, len = body.len(), "HTTP POST");
        let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.send(&bytes).await?;
            self.read_response().await
        })
        .await
        .map_err(|_| RtspError::Timeout(format!("POST {path}")))??;
        check_status("POST", response, false)
    }

    /// Wait for at least one more chunk from the peer (used while idle to notice a
    /// closed connection). Cancel-safe: data is only consumed once the read returns.
    pub async fn read_some(&mut self) -> Result<(), RtspError> {
        if self.closed {
            return Err(RtspError::Closed);
        }
        let n = match self.stream.read(&mut self.scratch).await {
            Ok(n) => n,
            Err(err) => {
                self.closed = true;
                return Err(err.into());
            }
        };
        if n == 0 {
            self.closed = true;
            return Err(RtspError::Closed);
        }
        match &mut self.hap {
            Some((_, reader)) => {
                if let Err(err) = reader.decrypt(&self.scratch[..n], &mut self.inbound) {
                    self.closed = true;
                    return Err(err.into());
                }
            }
            None => self.inbound.extend_from_slice(&self.scratch[..n]),
        }
        Ok(())
    }

    /// Close the write side; the connection is unusable afterwards.
    pub async fn shutdown(&mut self) {
        self.closed = true;
        if let Err(err) = self.stream.shutdown().await {
            debug!("RTSP shutdown: {err}");
        }
    }

    async fn send(&mut self, plaintext: &[u8]) -> Result<(), RtspError> {
        if self.closed {
            return Err(RtspError::Closed);
        }
        let result = match &mut self.hap {
            Some((writer, _)) => {
                let frames = writer.encrypt(plaintext)?;
                self.stream.write_all(&frames).await
            }
            None => self.stream.write_all(plaintext).await,
        };
        if let Err(err) = result {
            self.closed = true;
            return Err(err.into());
        }
        Ok(())
    }

    async fn read_response(&mut self) -> Result<Response, RtspError> {
        loop {
            match parse_response(&self.inbound) {
                Ok(Some((response, used))) => {
                    self.inbound.drain(..used);
                    return Ok(response);
                }
                Ok(None) => self.read_some().await?,
                Err(err) => {
                    // The byte stream is out of sync; nothing after this can be trusted.
                    self.closed = true;
                    return Err(err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn fixture() -> Json {
        serde_json::from_str(include_str!("../tests/fixtures/rtsp_requests.json"))
            .expect("rtsp_requests.json")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    fn fixture_context(fx: &Json) -> RtspContext {
        let s = &fx["session"];
        RtspContext {
            local_ip: s["local_ip"].as_str().unwrap().parse().unwrap(),
            session_id: s["session_id"].as_u64().unwrap() as u32,
            dacp_id: s["dacp_id"].as_str().unwrap().to_string(),
            active_remote: s["active_remote"].as_u64().unwrap() as u32,
        }
    }

    fn request_bytes(fx: &Json, name: &str) -> Vec<u8> {
        let entry = fx["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name}"));
        unhex(&entry["bytes"])
    }

    fn split_head(bytes: &[u8]) -> (String, Vec<u8>) {
        let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (
            String::from_utf8(bytes[..end].to_vec()).unwrap(),
            bytes[end + 4..].to_vec(),
        )
    }

    #[test]
    fn context_uri_matches_fixture() {
        let fx = fixture();
        assert_eq!(
            fixture_context(&fx).uri(),
            fx["session"]["uri"].as_str().unwrap()
        );
    }

    #[test]
    fn plain_requests_match_pyatv_bytes() {
        let fx = fixture();
        let ctx = fixture_context(&fx);
        let seq = fx["session"]["rtpseq"].as_u64().unwrap();
        let rtptime = fx["session"]["rtptime"].as_u64().unwrap();
        let rtp_info = format!("seq={seq};rtptime={rtptime}");
        let volume = format!(
            "volume: {}",
            fx["session"]["volume_value"].as_str().unwrap()
        );
        let metadata = crate::bodies::dmap_metadata("Windows audio", "AirPlay", "WinPods");
        let cases: Vec<(&str, u32, RtspRequest)> = vec![
            (
                "info",
                0,
                RtspRequest::new("GET").uri("/info").allow_error(),
            ),
            (
                "set_metadata",
                3,
                RtspRequest::new("SET_PARAMETER")
                    .bytes("application/x-dmap-tagged", metadata)
                    .header("Session", "0")
                    .header("RTP-Info", rtp_info.clone()),
            ),
            ("record", 4, RtspRequest::new("RECORD")),
            ("feedback", 5, RtspRequest::new("POST").uri("/feedback")),
            (
                "flush",
                6,
                RtspRequest::new("FLUSH")
                    .header("Range", "npt=0-")
                    .header("Session", "0")
                    .header("RTP-Info", rtp_info),
            ),
            (
                "set_parameter_volume",
                7,
                RtspRequest::new("SET_PARAMETER").bytes("text/parameters", volume.into_bytes()),
            ),
            (
                "teardown",
                8,
                RtspRequest::new("TEARDOWN").header("Session", "0"),
            ),
        ];
        for (name, cseq, request) in cases {
            let ours = format_rtsp_request(&ctx, cseq, &request).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&ours),
                String::from_utf8_lossy(&request_bytes(&fx, name)),
                "{name}"
            );
            assert_eq!(ours, request_bytes(&fx, name), "{name}");
        }
    }

    #[test]
    fn plist_requests_match_pyatv_by_meaning() {
        let fx = fixture();
        let ctx = fixture_context(&fx);
        for (name, cseq) in [("setup_base", 1u32), ("setup_stream", 2)] {
            let expected = request_bytes(&fx, name);
            let (expected_head, expected_body) = split_head(&expected);
            let value = plist::Value::from_reader(std::io::Cursor::new(&expected_body)).unwrap();
            let ours =
                format_rtsp_request(&ctx, cseq, &RtspRequest::new("SETUP").plist(value.clone()))
                    .unwrap();
            let (head, body) = split_head(&ours);
            // Rust and Python lay out bplist objects differently, so the length may
            // differ; everything else in the header must be identical.
            let normalise = |h: &str| {
                h.lines()
                    .map(|l| {
                        if l.starts_with("Content-Length:") {
                            "Content-Length: N"
                        } else {
                            l
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n")
            };
            assert_eq!(normalise(&head), normalise(&expected_head), "{name}");
            assert!(
                head.contains(&format!("Content-Length: {}", body.len())),
                "{name}"
            );
            let decoded = plist::Value::from_reader(std::io::Cursor::new(&body)).unwrap();
            assert_eq!(decoded, value, "{name}");
        }
    }

    #[test]
    fn pairing_posts_match_pyatv_bytes() {
        let fx = fixture();
        let headers = crate::hap::PAIRING_HEADERS;
        assert_eq!(
            format_http_post("/pair-pin-start", &headers, &[]),
            request_bytes(&fx, "pair_pin_start")
        );
        assert_eq!(
            format_http_post("/pair-setup", &headers, &crate::hap::m1()),
            request_bytes(&fx, "pair_setup_m1")
        );
        let m3 = request_bytes(&fx, "pair_setup_m3");
        let (_, m3_body) = split_head(&m3);
        assert_eq!(format_http_post("/pair-setup", &headers, &m3_body), m3);
    }

    #[test]
    fn responses_parse_whole_and_in_pieces() {
        let fx = fixture();
        for entry in fx["responses_used"].as_array().unwrap() {
            let name = entry["name"].as_str().unwrap();
            let bytes = unhex(&entry["bytes"]);
            let (response, used) = parse_response(&bytes).unwrap().expect(name);
            assert_eq!(used, bytes.len(), "{name}");
            assert_eq!(response.code, 200, "{name}");
            assert_eq!(response.message, "OK", "{name}");
            if let Some(cseq) = entry["cseq"].as_u64() {
                assert_eq!(response.cseq(), Some(cseq as u32), "{name}");
            }
            for cut in [1, bytes.len() / 2, bytes.len() - 1] {
                assert!(
                    parse_response(&bytes[..cut]).unwrap().is_none(),
                    "{name} @ {cut}"
                );
            }
        }
        let parsed = &fx["parsed_setup_response"];
        let stream = fx["responses_used"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "setup_stream")
            .unwrap();
        let (response, _) = parse_response(&unhex(&stream["bytes"])).unwrap().unwrap();
        assert_eq!(response.protocol, parsed["protocol"].as_str().unwrap());
        assert_eq!(response.version, parsed["version"].as_str().unwrap());
        assert_eq!(response.body, unhex(&parsed["body"]));
        for (key, value) in parsed["headers"].as_object().unwrap() {
            assert_eq!(response.headers.get(key), value.as_str(), "{key}");
        }
        assert!(response.body_plist().is_ok());
    }

    #[test]
    fn two_responses_in_one_buffer() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nhiRTSP/1.0 404 Not Found\r\ncseq: 2\r\n\r\n";
        let (first, used) = parse_response(raw).unwrap().unwrap();
        assert_eq!(
            (first.code, first.body.as_slice(), first.cseq()),
            (200, &b"hi"[..], Some(1))
        );
        let (second, rest) = parse_response(&raw[used..]).unwrap().unwrap();
        assert_eq!((second.code, second.message.as_str()), (404, "Not Found"));
        assert_eq!(second.cseq(), Some(2));
        assert_eq!(used + rest, raw.len());
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert!(parse_response(b"garbage\r\n\r\n").is_err());
        assert!(parse_response(b"RTSP/1.0 200 OK\r\nNoColon\r\n\r\n").is_err());
        assert!(parse_response(b"RTSP/1.0 200 OK\r\nContent-Length: x\r\n\r\n").is_err());
        assert!(parse_request(b"post /x RTSP/1.0\r\n\r\n").is_err());
        let (request, _) = parse_request(b"POST /command RTSP/1.0\r\nCSeq: 4\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                request.method.as_str(),
                request.path.as_str(),
                request.protocol.as_str(),
                request.version.as_str()
            ),
            ("POST", "/command", "RTSP", "1.0")
        );
    }

    #[test]
    fn status_rules_follow_pyatv() {
        let response = |code| Response {
            protocol: "RTSP".into(),
            version: "1.0".into(),
            code,
            message: "X".into(),
            headers: Headers::default(),
            body: Vec::new(),
        };
        assert!(check_status("GET", response(200), false).is_ok());
        assert!(matches!(
            check_status("GET", response(403), true),
            Err(RtspError::NotAuthenticated { code: 403, .. })
        ));
        assert!(matches!(
            check_status("GET", response(401), false),
            Err(RtspError::NotAuthenticated { code: 401, .. })
        ));
        assert!(check_status("GET", response(401), true).is_ok());
        assert!(check_status("GET", response(404), true).is_ok());
        assert!(matches!(
            check_status("SETUP", response(500), false),
            Err(RtspError::Status { code: 500, .. })
        ));
    }

    #[test]
    fn user_agent_header_suppresses_default() {
        let out = format_message(
            "POST",
            "/x",
            "HTTP/1.1",
            "fallback",
            None,
            &[("user-agent".into(), "Mine".into())],
            &[],
        );
        assert_eq!(out, b"POST /x HTTP/1.1\r\nuser-agent: Mine\r\n\r\n");
    }

    fn test_context() -> RtspContext {
        RtspContext {
            local_ip: Ipv4Addr::LOCALHOST,
            session_id: 7,
            dacp_id: "ABC".into(),
            active_remote: 9,
        }
    }

    #[tokio::test]
    async fn exchange_skips_stale_responses_and_counts_cseq() {
        let (client_end, mut server_end) = tokio::io::duplex(4096);
        let mut client = RtspClient::new(client_end, test_context());
        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = server_end.read(&mut buf).await.unwrap();
            let (request, _) = parse_request(&buf[..n]).unwrap().unwrap();
            assert_eq!(request.headers.get("CSeq"), Some("0"));
            server_end
                .write_all(b"RTSP/1.0 200 OK\r\nCSeq: 99\r\n\r\nRTSP/1.0 200 OK\r\nCSeq: 0\r\nContent-Length: 1\r\n\r\nx")
                .await
                .unwrap();
            let n = server_end.read(&mut buf).await.unwrap();
            let (request, _) = parse_request(&buf[..n]).unwrap().unwrap();
            assert_eq!(request.headers.get("CSeq"), Some("1"));
            server_end
                .write_all(b"RTSP/1.0 500 Internal Server Error\r\nCSeq: 1\r\n\r\n")
                .await
                .unwrap();
            server_end
        });
        let response = client
            .exchange(&RtspRequest::new("GET").uri("/info"))
            .await
            .unwrap();
        assert_eq!(response.body, b"x");
        let err = client
            .exchange(&RtspRequest::new("RECORD"))
            .await
            .unwrap_err();
        assert!(matches!(err, RtspError::Status { code: 500, .. }), "{err}");
        assert_eq!(client.next_cseq(), 2);
        drop(server.await.unwrap());
        assert!(matches!(client.read_some().await, Err(RtspError::Closed)));
        assert!(client.is_closed());
        assert!(matches!(
            client.exchange(&RtspRequest::new("TEARDOWN")).await,
            Err(RtspError::Closed)
        ));
        assert_eq!(client.next_cseq(), 3);
    }

    #[tokio::test]
    async fn malformed_response_breaks_the_connection() {
        let (client_end, mut server_end) = tokio::io::duplex(4096);
        let mut client = RtspClient::new(client_end, test_context());
        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let _ = server_end.read(&mut buf).await.unwrap();
            server_end.write_all(b"NONSENSE\r\n\r\n").await.unwrap();
            server_end
        });
        let err = client
            .exchange(&RtspRequest::new("RECORD"))
            .await
            .unwrap_err();
        assert!(matches!(err, RtspError::Parse(_)), "{err}");
        assert!(client.is_closed());
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn encrypted_exchange_round_trips() {
        use crate::hap::HapSession;
        let keys = CipherKeys {
            output: [1; 32],
            input: [2; 32],
        };
        let (client_end, mut server_end) = tokio::io::duplex(4096);
        let mut client = RtspClient::new(client_end, test_context());
        client.enable_encryption(&keys);
        let server = tokio::spawn(async move {
            let mut hap = HapSession::new(&keys.swapped());
            let mut plain = Vec::new();
            let mut buf = vec![0u8; 4096];
            let request = loop {
                let n = server_end.read(&mut buf).await.unwrap();
                hap.decrypt(&buf[..n], &mut plain).unwrap();
                if let Some((request, _)) = parse_request(&plain).unwrap() {
                    break request;
                }
            };
            assert_eq!(request.method, "SETUP");
            let reply = hap
                .encrypt(b"RTSP/1.0 200 OK\r\nCSeq: 0\r\nContent-Length: 3\r\n\r\nabc")
                .unwrap();
            server_end.write_all(&reply).await.unwrap();
        });
        let response = client
            .exchange(
                &RtspRequest::new("SETUP").plist(plist::Value::Dictionary(Default::default())),
            )
            .await
            .unwrap();
        assert_eq!(response.body, b"abc");
        server.await.unwrap();
    }
}
