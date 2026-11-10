//! Read-only view of the campus login page.
//!
//! This probe deliberately does not participate in authentication or UDP
//! keepalive. It only fetches `/` from the configured authentication server and
//! recognizes the deployment's page markers. Anything ambiguous is `Unknown`.

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    time::{Duration, Instant},
};

const HTTP_PORT: u16 = 80;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_HEADER_BYTES: usize = 4 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CampusPage {
    LoginForm,
    CurrentSession,
    LoginRejected,
    LoginSucceeded,
    Unknown,
}

#[derive(Debug)]
pub enum ProbeError {
    Io(io::ErrorKind),
    Timeout,
    Redirect,
    InvalidResponse,
    TooLarge,
}

/// Requests the login page directly from one configured IPv4 address.
///
/// The request never follows redirects, sends no credentials, uses a bounded
/// response size, and has a two-second overall budget after the TCP connection.
pub fn probe(server: Ipv4Addr) -> Result<CampusPage, ProbeError> {
    let address = SocketAddrV4::new(server, HTTP_PORT);
    let connect_budget = Duration::from_millis(700);
    let mut stream =
        TcpStream::connect_timeout(&address.into(), connect_budget).map_err(classify_io_error)?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(classify_io_error)?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(classify_io_error)?;

    let request = format!(
        "GET / HTTP/1.0\r\nHost: {server}\r\nAccept: text/html\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(classify_io_error)?;

    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut response = Vec::with_capacity(4096);
    let mut buffer = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProbeError::Timeout);
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(classify_io_error)?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) if response.len() + count <= MAX_RESPONSE_BYTES => {
                response.extend_from_slice(&buffer[..count]);
            }
            Ok(_) => return Err(ProbeError::TooLarge),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(ProbeError::Timeout);
            }
            Err(error) => return Err(classify_io_error(error)),
        }
    }

    parse_http_response(&response)
}

fn classify_io_error(error: io::Error) -> ProbeError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        ProbeError::Timeout
    } else {
        ProbeError::Io(error.kind())
    }
}

fn parse_http_response(response: &[u8]) -> Result<CampusPage, ProbeError> {
    let Some(separator) = response.windows(4).position(|part| part == b"\r\n\r\n") else {
        return Err(ProbeError::InvalidResponse);
    };
    if separator > MAX_HEADER_BYTES {
        return Err(ProbeError::TooLarge);
    }
    let headers =
        std::str::from_utf8(&response[..separator]).map_err(|_| ProbeError::InvalidResponse)?;
    let mut lines = headers.split("\r\n");
    let status = lines.next().ok_or(ProbeError::InvalidResponse)?;
    let mut status_parts = status.split_ascii_whitespace();
    let version = status_parts.next().ok_or(ProbeError::InvalidResponse)?;
    let code = status_parts
        .next()
        .ok_or(ProbeError::InvalidResponse)?
        .parse::<u16>()
        .map_err(|_| ProbeError::InvalidResponse)?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(ProbeError::InvalidResponse);
    }
    if (300..400).contains(&code) {
        return Err(ProbeError::Redirect);
    }
    if code != 200 {
        return Err(ProbeError::InvalidResponse);
    }

    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(ProbeError::InvalidResponse);
        };
        if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            return Err(ProbeError::InvalidResponse);
        }
        if name.eq_ignore_ascii_case("content-length") {
            let length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| ProbeError::InvalidResponse)?;
            if length > MAX_RESPONSE_BYTES {
                return Err(ProbeError::TooLarge);
            }
            if content_length.replace(length).is_some() {
                return Err(ProbeError::InvalidResponse);
            }
        }
    }

    let body = &response[separator + 4..];
    if let Some(length) = content_length
        && body.len() != length
    {
        return Err(ProbeError::InvalidResponse);
    }
    Ok(classify_html(body))
}

/// Recognizes exactly one known marker in an HTML comment outside `<script>`.
/// The page itself is GBK, but the protocol marker is ASCII.
pub fn classify_html(html: &[u8]) -> CampusPage {
    const PREFIX: &[u8] = b"dr.comwebloginid_";
    let lower = html.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    let mut index = 0;
    let mut in_script = false;
    let mut found = None;
    let mut marker_count = 0;

    while index < lower.len() {
        if in_script {
            if let Some(tag_start) = find_script_end(&lower[index..]).map(|offset| index + offset)
                && let Some(tag_end) = lower[tag_start..].iter().position(|byte| *byte == b'>')
            {
                index = tag_start + tag_end + 1;
                in_script = false;
                continue;
            }
            break;
        }

        if starts_tag(&lower[index..], b"<script") {
            if let Some(tag_end) = lower[index..].iter().position(|byte| *byte == b'>') {
                index += tag_end + 1;
                in_script = true;
                continue;
            }
            break;
        }

        if lower[index..].starts_with(b"<!--") {
            let content_start = index + 4;
            let Some(end) = find_bytes(&lower[content_start..], b"-->") else {
                break;
            };
            let content_end = content_start + end;
            let comment = trim_ascii(&lower[content_start..content_end]);
            let occurrences = comment
                .windows(PREFIX.len())
                .filter(|part| *part == PREFIX)
                .count();
            if occurrences > 0 {
                marker_count += occurrences;
                let page = if occurrences == 1 {
                    match comment {
                        b"dr.comwebloginid_0.htm" => Some(CampusPage::LoginForm),
                        b"dr.comwebloginid_1.htm" => Some(CampusPage::CurrentSession),
                        b"dr.comwebloginid_2.htm" => Some(CampusPage::LoginRejected),
                        b"dr.comwebloginid_3.htm" => Some(CampusPage::LoginSucceeded),
                        _ => None,
                    }
                } else {
                    None
                };
                found = match (found, page) {
                    (None, Some(page)) if marker_count == 1 => Some(page),
                    _ => return CampusPage::Unknown,
                };
            }
            index = content_end + 3;
            continue;
        }
        index += 1;
    }

    found.unwrap_or(CampusPage::Unknown)
}

fn starts_tag(haystack: &[u8], tag: &[u8]) -> bool {
    haystack.starts_with(tag)
        && haystack
            .get(tag.len())
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>')
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|part| part == needle)
}

fn find_script_end(haystack: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while let Some(found) = find_bytes(&haystack[offset..], b"</script") {
        let start = offset + found;
        let boundary = haystack.get(start + b"</script".len());
        if boundary.is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/')
        {
            return Some(start);
        }
        offset = start + b"</script".len();
    }
    None
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_a_single_known_comment_outside_scripts() {
        assert_eq!(
            classify_html(b"<html><!--Dr.COMWebLoginID_0.htm--></html>"),
            CampusPage::LoginForm
        );
        assert_eq!(
            classify_html(b"<script>\n<!--Dr.COMWebLoginID_1.htm-->\n</script><!--Dr.COMWebLoginID_1.htm--><html>"),
            CampusPage::CurrentSession
        );
        assert_eq!(
            classify_html(b"<!--Dr.COMWebLoginID_2.htm-->"),
            CampusPage::LoginRejected
        );
        assert_eq!(
            classify_html(b"<!--Dr.COMWebLoginID_3.htm-->"),
            CampusPage::LoginSucceeded
        );
        assert_eq!(
            classify_html(b"<script>const x='<!--Dr.COMWebLoginID_1.htm-->';</script>"),
            CampusPage::Unknown
        );
    }

    #[test]
    fn ambiguous_or_unknown_markers_stay_unknown() {
        assert_eq!(
            classify_html(b"<html>ordinary page</html>"),
            CampusPage::Unknown
        );
        assert_eq!(
            classify_html(b"<!--Dr.COMWebLoginID_9.htm-->"),
            CampusPage::Unknown
        );
        assert_eq!(
            classify_html(b"<!--Dr.COMWebLoginID_1.htm--><!--Dr.COMWebLoginID_1.htm-->"),
            CampusPage::Unknown
        );
        assert_eq!(
            classify_html(b"<!--prefix Dr.COMWebLoginID_1.htm suffix-->"),
            CampusPage::Unknown
        );
        assert_eq!(
            classify_html(b"<a title='Dr.COMWebLoginID_1.htm'>x</a>"),
            CampusPage::Unknown
        );
    }

    #[test]
    fn script_end_must_have_a_valid_tag_boundary() {
        assert_eq!(
            classify_html(b"<script>x</scriptfoo><!--Dr.COMWebLoginID_1.htm--></script><!--Dr.COMWebLoginID_1.htm-->"),
            CampusPage::CurrentSession
        );
    }

    #[test]
    fn accepts_only_a_complete_success_response_and_does_not_follow_redirects() {
        let body = b"<!--Dr.COMWebLoginID_1.htm-->";
        let mut ok =
            format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        ok.extend_from_slice(body);
        assert_eq!(
            parse_http_response(&ok).unwrap(),
            CampusPage::CurrentSession
        );
        let redirect =
            b"HTTP/1.0 302 Found\r\nLocation: http://example.invalid/\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(
            parse_http_response(redirect),
            Err(ProbeError::Redirect)
        ));
        let truncated = b"HTTP/1.0 200 OK\r\nContent-Length: 29\r\n\r\n<!--Dr.COMWebLoginID_1.htm";
        assert!(matches!(
            parse_http_response(truncated),
            Err(ProbeError::InvalidResponse)
        ));
    }
}
