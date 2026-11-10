use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSuccess {
    pub session_cookie: [u8; 16],
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFailure {
    pub code: u8,
    pub ip: Option<Ipv4Addr>,
    pub mac: Option<[u8; 6]>,
    /// Raw trailing text of a code-21 reply, exactly as it arrived on the wire.
    pub message: Option<Vec<u8>>,
}

impl LoginFailure {
    /// The server's own explanation, ready to show to the user.
    ///
    /// The deployment answers with simplified-Chinese GBK text — the original
    /// client's `auth_log.txt` records replies such as
    /// `此账号密码错误，请到自助服务平台修改邮箱密码！` and `账号录入错误或欠费` — and this
    /// client renders UTF-8. Showing the bare error code instead throws away the
    /// only part of the reply a user can act on, so the bytes are converted here.
    pub fn message_text(&self) -> Option<String> {
        let trimmed = trimmed_message(self.message.as_deref()?);
        if trimmed.is_empty() {
            return None;
        }
        Some(decode_simplified_chinese(trimmed))
    }
}

/// Drops the padding the fixed-width reply carries: everything from the first
/// NUL on, plus the trailing whitespace the server pads with.
fn trimmed_message(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let sliced = &bytes[..end];
    let start = sliced
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(sliced.len());
    let tail = sliced[start..]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &sliced[start..start + tail]
}

#[cfg(windows)]
fn decode_simplified_chinese(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    /// Code page 936. Spelled out instead of using `CP_ACP` so the conversion
    /// stays correct on a machine whose ANSI code page is not Chinese.
    const GBK: u32 = 936;
    // Flags must be 0 for a DBCS code page: `MB_ERR_INVALID_CHARS` is only
    // accepted for UTF-8 and UTF-7, and passing it here fails the call outright.
    const NO_FLAGS: u32 = 0;

    let length = bytes.len() as i32;
    // SAFETY: `bytes` is valid for `length` bytes; a null output with a zero
    // count is the documented way to ask for the required buffer size.
    let needed = unsafe {
        MultiByteToWideChar(
            GBK,
            NO_FLAGS,
            bytes.as_ptr(),
            length,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut buffer = vec![0u16; needed as usize];
    // SAFETY: the buffer is exactly the size the first call reported.
    let written = unsafe {
        MultiByteToWideChar(
            GBK,
            NO_FLAGS,
            bytes.as_ptr(),
            length,
            buffer.as_mut_ptr(),
            needed,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    buffer.truncate(written as usize);
    String::from_utf16_lossy(&buffer)
}

#[cfg(not(windows))]
fn decode_simplified_chinese(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResponse {
    Success(LoginSuccess),
    Failure(LoginFailure),
    Other(u8),
    TooShort,
}

/// Parses the response branches used by the original login handler. The
/// success cookie is at offset 23 when the response is long enough, matching
/// the reference JLU implementation; shorter success packets are preserved
/// without inventing a cookie.
pub fn parse(bytes: &[u8]) -> LoginResponse {
    let Some(&kind) = bytes.first() else {
        return LoginResponse::TooShort;
    };
    match kind {
        0x04 => {
            if bytes.len() < 39 {
                return LoginResponse::TooShort;
            }
            let mut cookie = [0u8; 16];
            cookie.copy_from_slice(&bytes[23..39]);
            LoginResponse::Success(LoginSuccess {
                session_cookie: cookie,
                raw: bytes.to_vec(),
            })
        }
        0x05 => {
            if bytes.len() < 5 {
                return LoginResponse::TooShort;
            }
            let code = bytes[4];
            let ip = if (code == 1 || code == 7) && bytes.len() >= 9 {
                Some(Ipv4Addr::new(bytes[5], bytes[6], bytes[7], bytes[8]))
            } else {
                None
            };
            let mac = if (code == 1 || code == 11) && bytes.len() >= 15 {
                Some(bytes[9..15].try_into().expect("six bytes"))
            } else {
                None
            };
            let message = if code == 21 && bytes.len() > 20 && bytes[20] != 0 {
                Some(bytes[20..].to_vec())
            } else {
                None
            };
            LoginResponse::Failure(LoginFailure {
                code,
                ip,
                mac,
                message,
            })
        }
        other => LoginResponse::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_success_cookie_at_reference_offset() {
        let mut packet = vec![0u8; 39];
        packet[0] = 4;
        packet[23..39].copy_from_slice(b"0123456789abcdef");
        assert_eq!(
            parse(&packet),
            LoginResponse::Success(LoginSuccess {
                session_cookie: *b"0123456789abcdef",
                raw: packet,
            })
        );
    }

    #[test]
    fn extracts_failure_ip_mac_and_server_message() {
        let mut packet = vec![0u8; 26];
        packet[0] = 5;
        packet[4] = 1;
        packet[5..9].copy_from_slice(&[10, 0, 0, 7]);
        packet[9..15].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            parse(&packet),
            LoginResponse::Failure(LoginFailure {
                code: 1,
                ip: Some(Ipv4Addr::new(10, 0, 0, 7)),
                mac: Some([1, 2, 3, 4, 5, 6]),
                message: None,
            })
        );
    }

    /// Builds a code-21 reply, which carries the server's reason from offset 20.
    fn message_packet(message: &[u8]) -> Vec<u8> {
        let mut packet = vec![0u8; 20];
        packet[0] = 5;
        packet[4] = 21;
        packet.extend_from_slice(message);
        packet
    }

    fn failure(packet: &[u8]) -> LoginFailure {
        match parse(packet) {
            LoginResponse::Failure(failure) => failure,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn the_server_reason_survives_parsing_and_padding_is_trimmed() {
        // `LGI ERR 客户端认证失败 : ldap auth error` is what the deployment's own
        // log recorded, padded out to a fixed width.
        let mut text = b"ldap auth error".to_vec();
        text.extend_from_slice(&[0; 12]);
        let reply = failure(&message_packet(&text));
        assert_eq!(
            reply.message_text().as_deref(),
            Some("ldap auth error"),
            "trailing NUL padding must not reach the user"
        );
        // A reply of nothing but padding has no reason to show.
        assert_eq!(failure(&message_packet(&[0u8; 8])).message_text(), None);
    }

    #[test]
    #[cfg(windows)]
    fn the_gbk_reason_the_server_sends_is_decoded_for_display() {
        // `此账号密码错误，请到自助服务平台修改邮箱密码！`, exactly as the deployment
        // sends it: GBK, because the original client is a GBK Windows program.
        let gbk: &[u8] = &[
            0xb4, 0xcb, 0xd5, 0xcb, 0xba, 0xc5, 0xc3, 0xdc, 0xc2, 0xeb, 0xb4, 0xed, 0xce, 0xf3,
            0xa3, 0xac, 0xc7, 0xeb, 0xb5, 0xbd, 0xd7, 0xd4, 0xd6, 0xfa, 0xb7, 0xfe, 0xce, 0xf1,
            0xc6, 0xbd, 0xcc, 0xa8, 0xd0, 0xde, 0xb8, 0xc4, 0xd3, 0xca, 0xcf, 0xe4, 0xc3, 0xdc,
            0xc2, 0xeb, 0xa3, 0xa1,
        ];
        let reply = failure(&message_packet(gbk));
        assert_eq!(
            reply.message_text().as_deref(),
            Some("此账号密码错误，请到自助服务平台修改邮箱密码！")
        );
    }
}
