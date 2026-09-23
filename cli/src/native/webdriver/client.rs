use serde_json::{json, Value};
use std::io::Read;
use std::time::{Duration, Instant};

pub struct WebDriverClient {
    base_url: String,
    session_id: Option<String>,
}

impl WebDriverClient {
    pub fn new(port: u16) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
            session_id: None,
        }
    }

    pub async fn create_session(&mut self, capabilities: Value) -> Result<Value, String> {
        let body = json!({
            "capabilities": {
                "alwaysMatch": capabilities,
            }
        });

        let response = self.post("/session", &body).await?;

        let session_id = response
            .get("value")
            .and_then(|v| v.get("sessionId"))
            .and_then(|v| v.as_str())
            .ok_or("No sessionId in response")?
            .to_string();

        self.session_id = Some(session_id);
        Ok(response)
    }

    pub async fn delete_session(&mut self) -> Result<(), String> {
        if let Some(ref sid) = self.session_id.clone() {
            let _ = self.delete(&format!("/session/{}", sid)).await;
            self.session_id = None;
        }
        Ok(())
    }

    pub async fn navigate(&self, url: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/url", sid), &json!({ "url": url }))
            .await?;
        Ok(())
    }

    pub async fn get_url(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/url", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Completion-only URL read: one request, a 64 KiB whole-response cap, and
    /// one deadline covering transport, HTTP decoding, and JSON parsing. The
    /// socket closes before JSON parsing; failures never recover the session.
    pub async fn get_url_before(&self, deadline: Instant) -> Result<String, String> {
        check_completion_deadline(deadline)?;
        let sid = self.session_id()?;
        let url = format!("{}/session/{}/url", self.base_url, sid);
        let (response, head) = completion_http_response(&url, deadline).await?;
        parse_completion_url(&response, head, deadline)
    }

    pub async fn get_title(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/title", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn find_element(&self, using: &str, value: &str) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self
            .post(
                &format!("/session/{}/element", sid),
                &json!({ "using": using, "value": value }),
            )
            .await?;

        let element_value = response.get("value").ok_or("No element in response")?;
        element_id_from_value(element_value, using, value)
    }

    pub async fn click_element(&self, element_id: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/click", sid, element_id),
            &json!({}),
        )
        .await?;
        Ok(())
    }

    pub async fn send_keys(&self, element_id: &str, text: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/value", sid, element_id),
            &json!({ "text": text }),
        )
        .await?;
        Ok(())
    }

    pub async fn clear_element(&self, element_id: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/clear", sid, element_id),
            &json!({}),
        )
        .await?;
        Ok(())
    }

    pub async fn execute_script(&self, script: &str, args: Vec<Value>) -> Result<Value, String> {
        let sid = self.session_id()?.to_string();
        let response = self
            .post(
                &format!("/session/{}/execute/sync", sid),
                &json!({ "script": script, "args": args }),
            )
            .await?;
        Ok(response.get("value").cloned().unwrap_or(Value::Null))
    }

    pub async fn screenshot(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/screenshot", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn get_cookies(&self) -> Result<Value, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/cookie", sid)).await?;
        Ok(response.get("value").cloned().unwrap_or(Value::Null))
    }

    pub async fn get_page_source(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/source", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn back(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/back", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub async fn forward(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/forward", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub async fn refresh(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/refresh", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub fn session_id_pub(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn new_with_session(port: u16, session_id: String) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
            session_id: Some(session_id),
        }
    }

    pub async fn execute_actions(&self, session_id: &str, actions: &Value) -> Result<(), String> {
        self.post(&format!("/session/{}/actions", session_id), actions)
            .await?;
        Ok(())
    }

    fn session_id(&self) -> Result<&str, String> {
        self.session_id
            .as_deref()
            .ok_or("No active WebDriver session".to_string())
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        http_request("GET", &format!("{}{}", self.base_url, path), None).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        http_request("POST", &format!("{}{}", self.base_url, path), Some(body)).await
    }

    async fn delete(&self, path: &str) -> Result<Value, String> {
        http_request("DELETE", &format!("{}{}", self.base_url, path), None).await
    }
}

const MAX_COMPLETION_RESPONSE_BYTES: usize = 64 * 1024;
const COMPLETION_PARSE_RESERVE: Duration = Duration::from_millis(10);
const COMPLETION_DEADLINE_ERROR: &str = "Completion URL read timed out";

fn check_completion_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err(COMPLETION_DEADLINE_ERROR.to_string())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CompletionBody {
    Length(usize),
    Chunked,
    UntilEof,
}

#[derive(Clone, Copy)]
struct CompletionHead {
    body_start: usize,
    body: CompletionBody,
}

// Searches resume at the previous incomplete boundary, so fragmented headers
// and chunk size lines do not cause repeated scans of already-read bytes.
fn find_completion_boundary(
    bytes: &[u8],
    start: usize,
    boundary: &[u8],
    deadline: Instant,
) -> Result<Option<usize>, String> {
    for (offset, window) in bytes[start..].windows(boundary.len()).enumerate() {
        if offset % 1024 == 0 {
            check_completion_deadline(deadline)?;
        }
        if window == boundary {
            return Ok(Some(start + offset));
        }
    }
    check_completion_deadline(deadline)?;
    Ok(None)
}

fn completion_head(
    bytes: &[u8],
    head_end: usize,
    deadline: Instant,
) -> Result<CompletionHead, String> {
    check_completion_deadline(deadline)?;
    let header = std::str::from_utf8(&bytes[..head_end])
        .map_err(|_| "Invalid WebDriver HTTP headers".to_string())?;
    let mut lines = header.split("\r\n");
    let mut status = lines.next().unwrap_or_default().split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err("Invalid WebDriver HTTP status".to_string());
    }
    let code = status
        .next()
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("Invalid WebDriver HTTP status")?;
    if !(200..300).contains(&code) {
        return Err(format!("WebDriver URL HTTP status {}", code));
    }
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        check_completion_deadline(deadline)?;
        let (name, value) = line
            .split_once(':')
            .ok_or("Invalid WebDriver HTTP header")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err("Invalid WebDriver HTTP header".to_string());
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err("Invalid WebDriver Content-Length".to_string());
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| "Invalid WebDriver Content-Length")?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err("Unsupported WebDriver Transfer-Encoding".to_string());
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err("Unsupported WebDriver Content-Encoding".to_string());
        }
    }
    if chunked && length.is_some() {
        return Err("Ambiguous WebDriver HTTP framing".to_string());
    }
    let body_start = head_end + 4;
    let body = if let Some(length) = length {
        if length > MAX_COMPLETION_RESPONSE_BYTES - body_start {
            return Err("WebDriver URL response exceeds 64 KiB limit".to_string());
        }
        CompletionBody::Length(length)
    } else if chunked {
        CompletionBody::Chunked
    } else {
        CompletionBody::UntilEof
    };
    check_completion_deadline(deadline)?;
    Ok(CompletionHead { body_start, body })
}

#[derive(Default)]
struct CompletionChunks {
    position: usize,
    scan_from: usize,
    data_length: Option<usize>,
    trailers: bool,
}

fn completion_chunk_length(line: &[u8]) -> Result<usize, String> {
    let size = line.split(|byte| *byte == b';').next().unwrap_or_default();
    if size.is_empty() || !size.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid WebDriver chunk size".to_string());
    }
    let size = std::str::from_utf8(size).map_err(|_| "Invalid WebDriver chunk size")?;
    usize::from_str_radix(size, 16).map_err(|_| "Invalid WebDriver chunk size".to_string())
}

impl CompletionChunks {
    fn complete_before(
        &mut self,
        bytes: &[u8],
        deadline: Instant,
    ) -> Result<Option<usize>, String> {
        loop {
            check_completion_deadline(deadline)?;
            if let Some(length) = self.data_length {
                let end = self
                    .position
                    .checked_add(length)
                    .and_then(|end| end.checked_add(2))
                    .filter(|end| *end <= MAX_COMPLETION_RESPONSE_BYTES)
                    .ok_or("WebDriver URL response exceeds 64 KiB limit")?;
                if bytes.len() < end {
                    return Ok(None);
                }
                if &bytes[end - 2..end] != b"\r\n" {
                    return Err("Invalid WebDriver chunk terminator".to_string());
                }
                self.position = end;
                self.scan_from = end;
                self.data_length = None;
            } else {
                let Some(end) = find_completion_boundary(bytes, self.scan_from, b"\r\n", deadline)?
                else {
                    self.scan_from = bytes.len().saturating_sub(1).max(self.position);
                    return Ok(None);
                };
                let line = &bytes[self.position..end];
                self.position = end + 2;
                self.scan_from = self.position;
                if self.trailers {
                    if line.is_empty() {
                        return Ok(Some(self.position));
                    }
                    if !line.contains(&b':')
                        || line
                            .iter()
                            .any(|byte| byte.is_ascii_control() && *byte != b'\t')
                    {
                        return Err("Invalid WebDriver chunk trailer".to_string());
                    }
                } else {
                    let length = completion_chunk_length(line)?;
                    if length == 0 {
                        self.trailers = true;
                    } else {
                        self.data_length = Some(length);
                    }
                }
            }
        }
    }
}

async fn completion_http_response(
    url: &str,
    deadline: Instant,
) -> Result<(Vec<u8>, CompletionHead), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    check_completion_deadline(deadline)?;
    let parsed = url::Url::parse(url).map_err(|_| "Invalid WebDriver URL")?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed.port().unwrap_or(80);
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        parsed.path(),
        host,
        port
    );
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        check_completion_deadline(deadline)?;
        let mut stream = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|_| "WebDriver URL connection failed")?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| "WebDriver URL write failed")?;
        // The extra byte detects overflow, including oversized HTTP headers
        // and chunk framing. No response read can grow this allocation.
        let mut bytes = vec![0; MAX_COMPLETION_RESPONSE_BYTES + 1];
        let mut used = 0;
        let mut head = None;
        let mut scan_from = 0;
        let mut chunks = CompletionChunks::default();
        loop {
            check_completion_deadline(deadline)?;
            let count = stream
                .read(&mut bytes[used..])
                .await
                .map_err(|_| "WebDriver URL read failed")?;
            used += count;
            if used > MAX_COMPLETION_RESPONSE_BYTES {
                return Err("WebDriver URL response exceeds 64 KiB limit".to_string());
            }
            let received = &bytes[..used];
            if head.is_none() {
                if let Some(end) =
                    find_completion_boundary(received, scan_from, b"\r\n\r\n", deadline)?
                {
                    head = Some(completion_head(received, end, deadline)?);
                } else {
                    scan_from = used.saturating_sub(3);
                }
            }
            if let Some(head) = head {
                let complete = match head.body {
                    CompletionBody::Length(length) => {
                        (used >= head.body_start + length).then_some(head.body_start + length)
                    }
                    CompletionBody::Chunked => chunks
                        .complete_before(&received[head.body_start..], deadline)?
                        .map(|end| head.body_start + end),
                    CompletionBody::UntilEof => (count == 0).then_some(used),
                };
                if let Some(end) = complete {
                    if end != used {
                        return Err("Unexpected bytes after WebDriver URL response".to_string());
                    }
                    bytes.truncate(used);
                    // Release the backend connection before any JSON decoding.
                    drop(stream);
                    return Ok((bytes, head));
                }
            }
            if count == 0 {
                return Err("Incomplete WebDriver URL response".to_string());
            }
        }
    })
    .await
    .map_err(|_| COMPLETION_DEADLINE_ERROR.to_string())?
}

struct CompletionJsonReader<'a> {
    bytes: &'a [u8],
    deadline: Instant,
}

impl Read for CompletionJsonReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                COMPLETION_DEADLINE_ERROR,
            ));
        }
        self.bytes.read(buffer)
    }
}

fn parse_completion_url(
    bytes: &[u8],
    head: CompletionHead,
    deadline: Instant,
) -> Result<String, String> {
    // Tokio cannot interrupt synchronous parsing. Reserve time, cap its input,
    // and check the same deadline throughout HTTP decoding and every JSON read.
    if deadline.saturating_duration_since(Instant::now()) < COMPLETION_PARSE_RESERVE {
        return Err(COMPLETION_DEADLINE_ERROR.to_string());
    }
    let raw_body = &bytes[head.body_start..];
    let mut decoded = Vec::new();
    let body = if matches!(head.body, CompletionBody::Chunked) {
        let mut position = 0;
        // The total decoded body is no larger than the already capped wire
        // input; this allocation cannot grow while stripping chunk framing.
        decoded.reserve_exact(raw_body.len());
        loop {
            check_completion_deadline(deadline)?;
            let end = find_completion_boundary(raw_body, position, b"\r\n", deadline)?
                .ok_or("Invalid WebDriver chunk size")?;
            let length = completion_chunk_length(&raw_body[position..end])?;
            if length == 0 {
                break;
            }
            position = end + 2;
            let end = position
                .checked_add(length)
                .filter(|end| *end <= raw_body.len())
                .ok_or("Invalid WebDriver chunk body")?;
            decoded.extend_from_slice(&raw_body[position..end]);
            position = end + 2;
        }
        decoded.as_slice()
    } else {
        raw_body
    };
    #[derive(serde::Deserialize)]
    struct UrlResponse {
        value: String,
    }
    let response = serde_json::from_reader::<_, UrlResponse>(CompletionJsonReader {
        bytes: body,
        deadline,
    });
    check_completion_deadline(deadline)?;
    let response = response.map_err(|_| "Invalid WebDriver URL JSON response")?;
    if response.value.trim().is_empty() {
        return Err("Empty WebDriver URL response".to_string());
    }
    check_completion_deadline(deadline)?;
    Ok(response.value)
}

/// Extract the element id from a WebDriver find-element `value` payload.
///
/// A genuine locator miss arrives as a WebDriver error payload
/// ("no such element"), not as a malformed response. It is translated to the
/// anchored locator-miss shape the rest of the CLI produces, so it keeps the
/// selector detail and receives the AI-friendly guidance that
/// `to_ai_friendly_error` reserves for locator misses. A payload with
/// neither an error nor an element id is genuinely malformed and keeps the
/// protocol-shaped message.
fn element_id_from_value(
    element_value: &Value,
    using: &str,
    value: &str,
) -> Result<String, String> {
    if element_value
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|e| e == "no such element")
    {
        return Err(format!("No element found by {} '{}'", using, value));
    }

    element_value
        .get("element-6066-11e4-a52e-4f735466cecf")
        .or_else(|| element_value.get("ELEMENT"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or("No element ID in response".to_string())
}

async fn http_request(method: &str, url: &str, body: Option<&Value>) -> Result<Value, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{}:{}", host, port);
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| format!("Connection timeout: {}", addr))?
    .map_err(|e| format!("Connection failed: {}", e))?;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body_str = body
        .map(|b| serde_json::to_string(b).unwrap_or_default())
        .unwrap_or_default();

    let request = if body.is_some() {
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            method, path, addr, body_str.len(), body_str
        )
    } else {
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            method, path, addr
        )
    };

    let mut stream = stream;
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("Write failed: {}", e))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| format!("Read failed: {}", e))?;

    let response_str = String::from_utf8_lossy(&response);
    let body_part = response_str.split("\r\n\r\n").nth(1).unwrap_or("").trim();

    // Handle chunked encoding
    let json_body = if body_part.contains('\n')
        && body_part
            .chars()
            .next()
            .map(|c| c.is_ascii_hexdigit())
            .unwrap_or(false)
    {
        // Chunked: skip chunk size lines
        body_part
            .lines()
            .filter(|l| !l.chars().all(|c| c.is_ascii_hexdigit() || c == '\r'))
            .collect::<Vec<&str>>()
            .join("")
    } else {
        body_part.to_string()
    };

    if json_body.is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&json_body).map_err(|e| {
        format!(
            "Invalid JSON response: {} (body: {})",
            e,
            json_body.chars().take(100).collect::<String>()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_reply(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    // Keep a complete length/chunk-framed response open until the client closes
    // it. This proves success and fallback release the backend connection, not
    // merely that dropping a server happens to terminate an unbounded read.
    async fn serve_completion_reply(
        reply: Vec<u8>,
        close_after_write: bool,
        release_at: Option<Instant>,
    ) -> (WebDriverClient, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebDriverClient::new_with_session(
            listener.local_addr().unwrap().port(),
            "existing-session".to_string(),
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = vec![0; 4096];
            let mut used = 0;
            while !request[..used]
                .windows(4)
                .any(|window| window == b"\r\n\r\n")
            {
                let count =
                    tokio::time::timeout(Duration::from_secs(2), stream.read(&mut request[used..]))
                        .await
                        .unwrap()
                        .unwrap();
                assert!(count > 0);
                used += count;
            }
            assert!(std::str::from_utf8(&request[..used])
                .unwrap()
                .starts_with("GET /session/existing-session/url HTTP/1.1\r\n"));
            if let Some(release_at) = release_at {
                tokio::time::sleep_until(tokio::time::Instant::from_std(release_at)).await;
            }
            // Early rejection may close while a large response is being sent.
            // The resulting write error varies by platform; the result and
            // bounded peer-close assertions below establish the behavior.
            let _ = stream.write_all(&reply).await;
            if close_after_write {
                // The client may already have closed after detecting malformed
                // or oversized framing; shutdown errors are platform-specific.
                let _ = stream.shutdown().await;
            }
            assert_completion_backend_closed(&mut stream, &listener).await;
        });
        (client, server)
    }

    async fn assert_completion_backend_closed(
        stream: &mut tokio::net::TcpStream,
        listener: &tokio::net::TcpListener,
    ) {
        use tokio::io::AsyncReadExt;
        let closed = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut [0]))
            .await
            .expect("completion read must promptly close its backend socket");
        match closed {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) => {}
            other => panic!("backend socket not closed: {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "completion must not retry"
        );
    }

    // Drive the client only after each fragment is sent. Its dedicated read
    // waker lets the reactor report new bytes, then polling consumes those bytes
    // and suspends on the next read before the server releases another fragment.
    // Separate TCP writes alone would allow the client to coalesce the fragments.
    async fn completion_reply_in_fragments(fragments: &[&[u8]]) -> Result<String, String> {
        use std::future::Future;
        use std::sync::Arc;
        use std::task::{Context, Wake, Waker};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct ReadWake(tokio::sync::Notify);
        impl Wake for ReadWake {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.notify_one();
            }
        }

        assert!(!fragments.is_empty());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebDriverClient::new_with_session(
            listener.local_addr().unwrap().port(),
            "existing-session".to_string(),
        );
        let start = Instant::now();
        let result = client.get_url_before(start + Duration::from_secs(2));
        tokio::pin!(result);
        let (mut stream, _) = tokio::select! {
            result = &mut result => panic!("client completed before accepting request: {result:?}"),
            accepted = tokio::time::timeout(Duration::from_millis(300), listener.accept()) => {
                accepted.unwrap().unwrap()
            }
        };
        // Tiny framing fragments must not wait on platform-specific delayed ACKs.
        stream.set_nodelay(true).unwrap();
        let mut request = [0; 4096];
        let mut used = 0;
        while !request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = tokio::select! {
                result = &mut result => panic!("client completed before response: {result:?}"),
                count = tokio::time::timeout(
                    Duration::from_millis(300), stream.read(&mut request[used..]),
                ) => count.unwrap().unwrap(),
            };
            assert!(count > 0);
            used += count;
        }
        assert!(std::str::from_utf8(&request[..used])
            .unwrap()
            .starts_with("GET /session/existing-session/url HTTP/1.1\r\n"));

        // Register a fresh waker after the request has been sent, so only the
        // pending response read (or the much later deadline) can wake it.
        let read_wake = Arc::new(ReadWake(tokio::sync::Notify::new()));
        let waker = Waker::from(read_wake.clone());
        let mut context = Context::from_waker(&waker);
        assert!(result.as_mut().poll(&mut context).is_pending());
        for (index, fragment) in fragments.iter().enumerate() {
            stream.write_all(fragment).await.unwrap();
            if index + 1 != fragments.len() {
                tokio::time::timeout(Duration::from_millis(300), read_wake.0.notified())
                    .await
                    .expect("client must wake to consume each response fragment");
                assert!(
                    result.as_mut().poll(&mut context).is_pending(),
                    "client completed on incomplete response fragment {index}"
                );
            }
        }
        let result = tokio::time::timeout(Duration::from_millis(300), result)
            .await
            .expect("complete fragmented response must finish without waiting for EOF or deadline");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_completion_backend_closed(&mut stream, &listener).await;
        result
    }

    #[tokio::test]
    async fn completion_url_resumes_fragmented_header_terminators() {
        let reply = completion_reply(r#"{"value":"https://example.test/fragmented"}"#);
        let head_end = reply
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        // Every nonempty prefix of CRLFCRLF can end a read. In particular the
        // three-byte prefix requires resuming the scan three bytes back.
        for prefix in 1..=3 {
            let split = head_end + prefix;
            assert_eq!(
                completion_reply_in_fragments(&[&reply[..split], &reply[split..]])
                    .await
                    .unwrap(),
                "https://example.test/fragmented"
            );
        }
    }

    #[tokio::test]
    async fn completion_url_resumes_fragmented_chunk_framing() {
        let body = br#"{"value":"https://example.test/fragmented"}"#;
        let size = format!("{:x};extension=present\r", body.len());
        let split = body.len() / 2;
        let fragments: &[&[u8]] = &[
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            size.as_bytes(), // CRLF after the size crosses a read boundary.
            b"\n",
            &body[..split],
            &body[split..],
            b"\r",    // CRLF after chunk data crosses a read boundary.
            b"\n0\r", // So does CRLF after the final chunk's size.
            b"\nX-Trailer: fragmented\r",
            b"\n\r", // Split both the trailer line and the empty final line.
            b"\n",
        ];
        assert_eq!(
            completion_reply_in_fragments(fragments).await.unwrap(),
            "https://example.test/fragmented"
        );
    }

    #[tokio::test]
    async fn completion_url_rejects_chunked_trailing_bytes() {
        let body = r#"{"value":"https://example.test/fragmented"}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\nX-Trailer: valid\r\n\r\nunexpected",
            body.len(), body
        );
        assert_eq!(
            completion_reply_in_fragments(&[reply.as_bytes()])
                .await
                .unwrap_err(),
            "Unexpected bytes after WebDriver URL response"
        );
    }

    #[tokio::test]
    async fn completion_url_accepts_content_length_chunked_and_eof_responses() {
        let body = r#"{"value":"https://example.test/live"}"#;
        let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x};test=yes\r\n{}\r\n0\r\nX-Trailer: accepted\r\n\r\n", body.len(), body);
        let eof = format!(
            "HTTP/1.0 200 OK\r\nX_Custom.Header: accepted\r\n\r\n{}",
            body
        );
        for (reply, eof) in [
            (completion_reply(body), false),
            (chunked.into_bytes(), false),
            (eof.into_bytes(), true),
        ] {
            let (client, server) = serve_completion_reply(reply, eof, None).await;
            assert_eq!(
                client
                    .get_url_before(Instant::now() + Duration::from_secs(1))
                    .await
                    .unwrap(),
                "https://example.test/live"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completion_url_accepts_exact_wire_limit() {
        let prefix = b"HTTP/1.0 200 OK\r\n\r\n";
        let template = r#"{"value":"https://example.test/live","padding":""}"#;
        let padding = "x".repeat(MAX_COMPLETION_RESPONSE_BYTES - prefix.len() - template.len());
        let body = format!(
            r#"{{"value":"https://example.test/live","padding":"{}"}}"#,
            padding
        );
        let mut reply = prefix.to_vec();
        reply.extend_from_slice(body.as_bytes());
        assert_eq!(reply.len(), MAX_COMPLETION_RESPONSE_BYTES);
        let (client, server) = serve_completion_reply(reply, true, None).await;
        assert_eq!(
            client
                .get_url_before(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap(),
            "https://example.test/live"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn completion_url_rejects_complete_oversized_body_headers_and_chunks() {
        let body = format!(
            r#"{{"value":"https://example.test/live","padding":[{}0]}}"#,
            "0,".repeat(MAX_COMPLETION_RESPONSE_BYTES)
        );
        let eof = format!("HTTP/1.0 200 OK\r\n\r\n{}", body);
        let header = format!(
            "HTTP/1.1 200 OK\r\nX-Padding: {}\r\nContent-Length: 2\r\n\r\n{{}}",
            "x".repeat(MAX_COMPLETION_RESPONSE_BYTES)
        );
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            body
        );
        // The body is tiny, but its many legal chunk extensions consume the
        // wire budget; limiting only decoded JSON bytes would miss this case.
        let framed = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;padding={}\r\n{{}}\r\n0\r\n\r\n", "x".repeat(MAX_COMPLETION_RESPONSE_BYTES));
        for reply in [
            completion_reply(&body),
            eof.into_bytes(),
            header.into_bytes(),
            chunked.into_bytes(),
            framed.into_bytes(),
        ] {
            let (client, server) = serve_completion_reply(reply, false, None).await;
            let start = Instant::now();
            let error = client
                .get_url_before(start + Duration::from_secs(1))
                .await
                .unwrap_err();
            assert!(error.contains("exceeds 64 KiB"), "{error}");
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "oversized response waited for its deadline"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completion_url_rejects_oversized_declared_body_without_waiting_for_data() {
        for reply in [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_COMPLETION_RESPONSE_BYTES
            ),
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                MAX_COMPLETION_RESPONSE_BYTES
            ),
        ] {
            let (client, server) = serve_completion_reply(reply.into_bytes(), false, None).await;
            let start = Instant::now();
            let error = client
                .get_url_before(start + Duration::from_secs(1))
                .await
                .unwrap_err();
            assert!(error.contains("exceeds 64 KiB"), "{error}");
            assert!(start.elapsed() < Duration::from_millis(500));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completion_url_rejects_unusable_http_and_json() {
        let valid = r#"{"value":"https://example.test/live"}"#;
        let cases = [
            b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nInvalid Header\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nxyz\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}xx".to_vec(),
            completion_reply(""),
            completion_reply("not json"),
            completion_reply(r#"{"value":" "}"#),
            completion_reply(r#"{"value":{"error":"invalid session id"}}"#),
            [completion_reply(valid), b"unexpected".to_vec()].concat(),
        ];
        for reply in cases {
            let (client, server) = serve_completion_reply(reply, false, None).await;
            let start = Instant::now();
            assert!(client
                .get_url_before(start + Duration::from_secs(1))
                .await
                .is_err());
            assert!(start.elapsed() < Duration::from_millis(500));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completion_url_rejects_truncated_http_at_eof() {
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nIncomplete".to_vec(),
        ] {
            let (client, server) = serve_completion_reply(reply, true, None).await;
            assert!(client
                .get_url_before(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap_err()
                .contains("Incomplete"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completion_url_near_deadline_complete_response_closes_and_falls_back() {
        let deadline = Instant::now() + Duration::from_millis(150);
        let (client, server) = serve_completion_reply(
            completion_reply(r#"{"value":"https://example.test/late"}"#),
            false,
            Some(deadline - COMPLETION_PARSE_RESERVE / 2),
        )
        .await;
        assert_eq!(
            client.get_url_before(deadline).await.unwrap_err(),
            COMPLETION_DEADLINE_ERROR
        );
        server.await.unwrap();
    }

    #[test]
    fn completion_url_parse_reserve_rejects_complete_json_before_decoding() {
        let bytes = completion_reply(r#"{"value":"https://example.test/late"}"#);
        let end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let head = completion_head(&bytes, end, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(
            parse_completion_url(&bytes, head, Instant::now() + COMPLETION_PARSE_RESERVE / 2)
                .unwrap_err(),
            COMPLETION_DEADLINE_ERROR
        );
    }

    #[test]
    fn completion_url_json_reader_checks_deadline_while_consuming_bytes() {
        let mut reader = CompletionJsonReader {
            bytes: b"1234",
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let mut byte = [0];
        assert_eq!(reader.read(&mut byte).unwrap(), 1);
        reader.deadline = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(reader.bytes, b"234");
    }

    #[tokio::test]
    async fn completion_url_expired_deadline_never_dispatches() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebDriverClient::new_with_session(
            listener.local_addr().unwrap().port(),
            "existing-session".to_string(),
        );
        assert_eq!(
            client
                .get_url_before(Instant::now() - Duration::from_millis(1))
                .await
                .unwrap_err(),
            COMPLETION_DEADLINE_ERROR
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn completion_url_leaves_ordinary_large_get_url_unchanged() {
        let body = format!(
            r#"{{"value":"https://example.test/ordinary","padding":[{}0]}}"#,
            "0,".repeat(MAX_COMPLETION_RESPONSE_BYTES)
        );
        let (client, server) = serve_completion_reply(completion_reply(&body), true, None).await;
        assert_eq!(
            client.get_url().await.unwrap(),
            "https://example.test/ordinary"
        );
        server.await.unwrap();
    }

    #[test]
    fn test_client_new() {
        let client = WebDriverClient::new(4444);
        assert_eq!(client.base_url, "http://127.0.0.1:4444");
        assert!(client.session_id.is_none());
    }

    #[test]
    fn test_session_id_none() {
        let client = WebDriverClient::new(4444);
        let result = client.session_id();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No active WebDriver session"));
    }

    #[test]
    fn test_client_custom_port() {
        let client = WebDriverClient::new(9515);
        assert_eq!(client.base_url, "http://127.0.0.1:9515");
    }

    /// In the WebDriver engine a genuine locator miss surfaces as a
    /// "no such element" error payload; it must translate to the anchored
    /// locator-miss shape (selector included) so `to_ai_friendly_error`
    /// appends its guidance, exactly as the CDP engine's misses do.
    #[test]
    fn test_find_element_miss_translates_to_locator_miss() {
        let payload = json!({
            "error": "no such element",
            "message": "An element could not be located on the page using the given search parameters.",
        });
        let err = element_id_from_value(&payload, "css selector", ".missing")
            .expect_err("an error payload is a miss, not an element");
        assert_eq!(err, "No element found by css selector '.missing'");
    }

    #[test]
    fn test_find_element_id_extracted_from_w3c_payload() {
        let payload = json!({ "element-6066-11e4-a52e-4f735466cecf": "abc123" });
        assert_eq!(
            element_id_from_value(&payload, "css selector", "#x").unwrap(),
            "abc123"
        );
    }

    /// A payload with neither an error nor an element id is genuinely
    /// malformed; it keeps the protocol-shaped message, which
    /// `to_ai_friendly_error` deliberately passes through unchanged.
    #[test]
    fn test_find_element_malformed_payload_keeps_protocol_message() {
        let payload = json!({ "unexpected": true });
        let err = element_id_from_value(&payload, "css selector", "#x")
            .expect_err("no id and no error is malformed");
        assert_eq!(err, "No element ID in response");
    }
}
