//! Byte-exact port of the login frame, primary keep-alive digest and challenge
//! nonce used by the third-party Python Dr.COM client. The local C# client
//! follows most of this layout but differs in its fixed client key and checksum
//! walk, so it is not this profile's byte-level oracle.
//!
//! This profile is deliberately separate from the module in `protocol.rs`,
//! which holds the raw fragments recovered from the unpacked original
//! `DrAuthSvr.dll`. The two are cross-checked against each other where they
//! overlap; see `docs/original-client-analysis.md` for the outstanding
//! differences.
//!
//! Every builder here is validated against `reference/reference_vectors.txt`,
//! which `reference/gen_vectors.py` produces from the reference implementation
//! itself. That keeps the port honest: the expected bytes come from the code
//! from a separately maintained implementation, not from this port. A capture
//! from the original Windows client is still required before treating it as the
//! original-client profile.

use crate::md5::digest;

/// The reference stores the account in a fixed 36-byte field. Longer names
/// would spill into the control-status byte, so they are rejected instead of
/// silently truncating the field while the length byte keeps the real length.
pub const ACCOUNT_FIELD_LEN: usize = 36;

/// `ror()` in the reference indexes the 16-byte first digest with the password
/// index, so a password longer than the digest is not representable.
pub const MAX_PASSWORD_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceLoginConfig {
    /// IPv4 address of the adapter the authentication socket is bound to.
    pub host_ipv4: [u8; 4],
    /// MAC as an integer, matching the reference `mac` global.
    pub mac: u64,
    pub hostname: Vec<u8>,
    pub primary_dns: [u8; 4],
    pub dhcp_server: [u8; 4],
    pub auth_version: [u8; 2],
    pub control_status: u8,
    pub adapter_num: u8,
    pub ip_dog: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    PasswordTooLong,
    UsernameTooLong,
}

/// `md51`: `MD5(03 01 || salt || password)`. The reference reuses this digest
/// in the login frame, in the MAC mask and in every primary keep-alive, so it
/// is exposed instead of being recomputed per packet.
pub fn first_digest(salt: &[u8], password: &[u8]) -> [u8; 16] {
    let mut input = Vec::with_capacity(2 + salt.len() + password.len());
    input.extend_from_slice(&[0x03, 0x01]);
    input.extend_from_slice(salt);
    input.extend_from_slice(password);
    digest(&input)
}

/// `md52`: `MD5(01 || password || salt || 00 00 00 00)`.
fn second_digest(salt: &[u8], password: &[u8]) -> [u8; 16] {
    let mut input = Vec::with_capacity(5 + password.len() + salt.len());
    input.push(0x01);
    input.extend_from_slice(password);
    input.extend_from_slice(salt);
    input.extend_from_slice(&[0u8; 4]);
    digest(&input)
}

/// `ror()`: XOR each password byte with the first digest and rotate left by 3.
fn rotated_password(first: &[u8; 16], password: &[u8]) -> Vec<u8> {
    password
        .iter()
        .enumerate()
        .map(|(index, byte)| (first[index] ^ byte).rotate_left(3))
        .collect()
}

/// The reference `dump()`: the shortest big-endian byte string of a value,
/// zero-padded to an even number of hex digits. A MAC whose most significant
/// byte is zero therefore yields five bytes, not six.
fn dump(value: u64) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();
    let leading_zeros = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    bytes.drain(..leading_zeros);
    bytes
}

fn minimal_mac_bytes(mac: u64, width: usize) -> Vec<u8> {
    let mut bytes = dump(mac);
    if bytes.len() < width {
        let mut padded = vec![0u8; width - bytes.len()];
        padded.append(&mut bytes);
        bytes = padded;
    }
    bytes
}

/// The reference `checksum()`: XOR little-endian words into the `1234` seed,
/// multiply by `1968`, and return the result little-endian.
///
/// The reference groups the buffer with `re.findall(b'....', s)`, and Python's
/// `.` does not match a newline byte. A window containing `0x0a` therefore fails
/// to match, the scan advances by one byte instead of four, and every later
/// window is taken from a shifted offset. That is not the obvious intent, but it
/// is what the client that authenticates on this deployment actually computes:
/// a plain four-byte stride produces a different value for all three login
/// vectors. The C# port does stride by four and still authenticates, which
/// suggests the server does not reject a mismatch here; confirming that against
/// the original binary is still open (see `docs/original-client-analysis.md`).
fn checksum(data: &[u8]) -> [u8; 4] {
    let mut value = 1234u32;
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let window = &data[offset..offset + 4];
        if window.contains(&0x0a) {
            offset += 1;
            continue;
        }
        value ^= u32::from_le_bytes(window.try_into().expect("four-byte window"));
        offset += 4;
    }
    value.wrapping_mul(1968).to_le_bytes()
}

fn padded(value: &[u8], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width];
    let copy = value.len().min(width);
    out[..copy].copy_from_slice(&value[..copy]);
    out
}

/// OS/build fingerprint block the reference emits verbatim at offset 162.
const OS_FINGERPRINT: [u8; 29] = [
    0x94, 0x00, 0x00, 0x00, // unknown
    0x06, 0x00, 0x00, 0x00, // os major
    0x02, 0x00, 0x00, 0x00, // os minor
    0xf0, 0x23, 0x00, 0x00, // os build
    0x02, 0x00, 0x00, 0x00, // os unknown
    0x44, 0x72, 0x43, 0x4f, 0x4d, 0x00, 0xcf, 0x07, 0x68, // "DrCOM" marker
];

/// Fixed 40-byte client key the reference sends at offset 246. Transcribing it
/// by hand is easy to get wrong — an earlier revision of this port had one
/// extra digit, which the reference vectors caught.
const CLIENT_KEY: &[u8] = b"3dc79f5212e8170acfa9ec95f1d74916542be7b1";

/// Challenge nonce of the reference client: Unix seconds modulo `0xffff`.
/// The original derives its nonce from the same clock; the modulus is `0xffff`
/// rather than 16 truncated bits, which matters only for the echoed value.
pub fn challenge_nonce(unix_seconds: u64) -> u16 {
    (unix_seconds % 0xffff) as u16
}

/// Builds the complete login frame. Layout (offsets are absolute):
///
/// | offset | field |
/// | --- | --- |
/// | 0 | `03 01 00`, then account length + 20 |
/// | 4 | `md51` |
/// | 20 | account, padded to 36 bytes |
/// | 56 | control status, adapter number |
/// | 58 | MAC ^ `md51[..6]`, six bytes |
/// | 64 | `md52` |
/// | 80 | IP count (`01`), host IPv4, three empty address slots |
/// | 97 | `md53 = MD5(frame[..97] || 14 00 07 0b)[..8]` |
/// | 105 | IP dog, four zero bytes |
/// | 110 | host name, padded to 32 bytes |
/// | 142 | primary DNS, DHCP server, secondary DNS, padding |
/// | 162 | OS fingerprint, 55 zero bytes, client key, 24 zero bytes |
/// | 310 | negotiated auth version, `00`, password length |
/// | 314 | rotated password |
/// | | `02 0c`, checksum, `00 00`, MAC, password padding, `60 a2`, 28 zero bytes |
pub fn build_login_packet(
    salt: &[u8],
    username: &[u8],
    password: &[u8],
    config: &ReferenceLoginConfig,
) -> Result<Vec<u8>, BuildError> {
    if username.len() > ACCOUNT_FIELD_LEN {
        return Err(BuildError::UsernameTooLong);
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(BuildError::PasswordTooLong);
    }

    let first = first_digest(salt, password);
    let mut mac_digest = 0u64;
    for byte in &first[..6] {
        mac_digest = (mac_digest << 8) | u64::from(*byte);
    }
    let mac_mask = minimal_mac_bytes(mac_digest ^ (config.mac & 0x0000_ffff_ffff_ffff), 6);

    let mut frame = Vec::with_capacity(384);
    frame.extend_from_slice(&[0x03, 0x01, 0x00, (username.len() + 20) as u8]);
    frame.extend_from_slice(&first);
    frame.extend_from_slice(&padded(username, ACCOUNT_FIELD_LEN));
    frame.push(config.control_status);
    frame.push(config.adapter_num);
    frame.extend_from_slice(&mac_mask);
    frame.extend_from_slice(&second_digest(salt, password));

    frame.push(0x01);
    frame.extend_from_slice(&config.host_ipv4);
    frame.extend_from_slice(&[0u8; 12]);

    let mut third_input = frame.clone();
    third_input.extend_from_slice(&[0x14, 0x00, 0x07, 0x0b]);
    frame.extend_from_slice(&digest(&third_input)[..8]);

    frame.push(config.ip_dog);
    frame.extend_from_slice(&[0u8; 4]);
    frame.extend_from_slice(&padded(&config.hostname, 32));
    frame.extend_from_slice(&config.primary_dns);
    frame.extend_from_slice(&config.dhcp_server);
    frame.extend_from_slice(&[0u8; 12]);

    frame.extend_from_slice(&OS_FINGERPRINT);
    frame.extend_from_slice(&[0u8; 55]);
    frame.extend_from_slice(CLIENT_KEY);
    frame.extend_from_slice(&[0u8; 24]);

    frame.extend_from_slice(&config.auth_version);
    frame.push(0x00);
    frame.push(password.len() as u8);
    frame.extend_from_slice(&rotated_password(&first, password));
    frame.extend_from_slice(&[0x02, 0x0c]);

    let mut checksum_input = frame.clone();
    checksum_input.extend_from_slice(&[0x01, 0x26, 0x07, 0x11, 0x00, 0x00]);
    checksum_input.extend_from_slice(&dump(config.mac));
    frame.extend_from_slice(&checksum(&checksum_input));

    frame.extend_from_slice(&[0x00, 0x00]);
    frame.extend_from_slice(&dump(config.mac));

    // The reference skips this padding when the password is exactly 16 bytes,
    // where `len(pwd) / 4 == 4` in the original float division.
    if password.len() / 4 != 4 {
        frame.extend(std::iter::repeat_n(0u8, password.len() / 4));
    }
    frame.extend_from_slice(&[0x60, 0xa2]);
    frame.extend_from_slice(&[0u8; 28]);

    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference_vectors::vector;

    const SALT: [u8; 4] = [0x1a, 0x2b, 0x3c, 0x4d];
    const USER: &[u8] = b"testuser";
    /// A full-width account: 36 bytes is the whole fixed field.
    const USER_36: &[u8] = b"useraaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PASSWORD: &[u8] = b"testpass";
    const PASSWORD_16: &[u8] = b"0123456789abcdef";
    /// Three four-byte groups, so the padding rule writes three zero bytes.
    const PASSWORD_12: &[u8] = b"0123456789ab";
    const MAC: u64 = 0x112288776655;
    /// Whose `dump()` is two bytes rather than six.
    const MAC_SMALL: u64 = 0x0000_0abc;

    fn config(mac: u64) -> ReferenceLoginConfig {
        ReferenceLoginConfig {
            host_ipv4: [10, 0, 0, 9],
            mac,
            hostname: b"TESTHOST".to_vec(),
            primary_dns: [10, 10, 10, 10],
            dhcp_server: [0, 0, 0, 0],
            auth_version: [0x68, 0x00],
            control_status: 0x20,
            adapter_num: 0x03,
            ip_dog: 0x01,
        }
    }

    #[test]
    fn login_frame_matches_reference_bytes() {
        let packet = build_login_packet(&SALT, USER, PASSWORD, &config(MAC)).unwrap();
        assert_eq!(packet, vector("login.user8.pwd8"));
    }

    #[test]
    fn checksum_reproduces_reference_grouping_not_a_flat_stride() {
        let packet = build_login_packet(&SALT, USER, PASSWORD, &config(MAC)).unwrap();
        assert_eq!(
            &packet[324..328],
            &[0x90, 0x99, 0x2b, 0xaa],
            "reference value"
        );
        // A plain four-byte stride — which the C# port uses — yields this
        // instead. Pinning both makes an accidental "simplification" of the
        // grouping visible, because it would silently change the wire bytes.
        let flat = [0x78u8, 0x39, 0xee, 0x10];
        assert_ne!(&packet[324..328], &flat[..]);
    }

    #[test]
    fn password_padding_is_skipped_for_sixteen_byte_password() {
        let packet = build_login_packet(&SALT, USER, PASSWORD_16, &config(MAC)).unwrap();
        assert_eq!(packet, vector("login.user8.pwd16"));
        // Eight extra rotated-password bytes, two fewer padding bytes.
        assert_eq!(packet.len(), vector("login.user8.pwd8").len() + 6);
        // No zero padding before the `60 a2` trailer for a 16-byte password.
        let trailer = packet.len() - 30;
        assert_eq!(&packet[trailer..trailer + 2], &[0x60, 0xa2]);
    }

    #[test]
    fn mac_with_leading_zero_matches_reference_dump_semantics() {
        let packet = build_login_packet(&SALT, USER, PASSWORD, &config(0x0011_2233_4455)).unwrap();
        assert_eq!(packet, vector("login.mac_leading_zero"));
        assert_eq!(dump(0), vec![0x00]);
        assert_eq!(dump(0x1122_8877_6655).len(), 6);
        assert_eq!(dump(0x0011_2233_4455).len(), 5);
    }

    #[test]
    fn widest_account_and_twelve_byte_password_match_reference() {
        let packet = build_login_packet(&SALT, USER_36, PASSWORD_12, &config(MAC)).unwrap();
        assert_eq!(packet, vector("login.user36.pwd12"));
        // Twelve bytes is three four-byte groups: four more rotated password
        // bytes than an eight-byte password, and one more padding byte.
        assert_eq!(packet.len(), vector("login.user8.pwd8").len() + 5);
        let trailer = packet.len() - 30;
        assert_eq!(&packet[trailer..trailer + 2], &[0x60, 0xa2]);
        assert_eq!(
            &packet[trailer - 3..trailer],
            &[0u8; 3],
            "three padding bytes"
        );
    }

    #[test]
    fn a_short_mac_only_shortens_the_trailer_copy() {
        let packet = build_login_packet(&SALT, USER, PASSWORD, &config(MAC_SMALL)).unwrap();
        assert_eq!(packet, vector("login.mac_small"));
        assert_eq!(dump(MAC_SMALL), vec![0x0a, 0xbc]);
        // `dump()` yields two bytes for this MAC and the frame uses it twice:
        // once inside the checksum input, which is not part of the frame, and
        // once in the trailer. The MAC XOR field is `.rjust(6, ..)`, so it keeps
        // all six bytes and only the trailer shrinks — by four.
        assert_eq!(packet.len(), vector("login.user8.pwd8").len() - 4);
    }

    #[test]
    fn a_ten_byte_password_produces_the_length_seen_on_the_wire() {
        // The frame length is not a constant. Two things move it: the
        // rotated-password block grows one byte per password byte, and the zero
        // padding before the `60 a2` trailer is `password.len() / 4` bytes. So
        // "368" is only the eight-byte reference case.
        //
        // A real session on 2026-09-15 logged `登录报文 370 字节` against the JLU
        // server, which is a ten-character password: two more rotated bytes and
        // the same two padding bytes (`10 / 4 == 2`, as for eight). Pinning it
        // here keeps that field observation from being re-derived by hand.
        let packet = build_login_packet(&SALT, USER, b"0123456789", &config(MAC)).unwrap();
        assert_eq!(packet.len(), vector("login.user8.pwd8").len() + 2);
        assert_eq!(packet.len(), 370);
    }

    #[test]
    fn client_key_is_forty_bytes_at_offset_246() {
        let packet = build_login_packet(&SALT, USER, PASSWORD, &config(MAC)).unwrap();
        // Spelled out rather than only referenced, so a transcription slip
        // cannot hide behind the constant it is supposed to be checking.
        assert_eq!(CLIENT_KEY, &b"3dc79f5212e8170acfa9ec95f1d74916542be7b1"[..]);
        assert_eq!(CLIENT_KEY.len(), 40, "the shipping C# port has 41 here");
        assert_eq!(&packet[246..286], CLIENT_KEY);
        assert_eq!(packet.len(), 368);
    }

    #[test]
    fn challenge_nonce_uses_reference_modulus() {
        assert_eq!(challenge_nonce(0x1234_5678), 0x68ac);
        assert_eq!(challenge_nonce(1_595_812_411), 0x8959);
    }

    #[test]
    fn oversized_credentials_are_rejected() {
        assert_eq!(
            build_login_packet(&SALT, &[b'a'; 37], PASSWORD, &config(MAC)),
            Err(BuildError::UsernameTooLong)
        );
        assert_eq!(
            build_login_packet(&SALT, USER, &[b'a'; 17], &config(MAC)),
            Err(BuildError::PasswordTooLong)
        );
    }

    #[test]
    fn first_digest_is_reused_by_the_primary_keep_alive() {
        let digest = first_digest(&SALT, PASSWORD);
        let expected: [u8; 16] = vector("keepalive.primary")[1..17].try_into().unwrap();
        assert_eq!(digest, expected);
    }
}
