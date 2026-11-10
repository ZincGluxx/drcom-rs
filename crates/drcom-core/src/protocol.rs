//! Dr.COM UDP wire fragments verified against the unpacked original `DrAuthSvr.dll`.
//!
//! The addresses below refer to the original module loaded at base `0x10000000`.
//! This module deliberately contains no socket or credential handling.

use crate::md5;

/// The original sends 20 bytes while entering the login challenge state
/// (`0x1002ecf0`). The caller supplies the sequence and nonce, which the
/// original generates immediately before constructing the packet.
pub fn challenge_request(sequence: u8, nonce: u16, client_option: u8) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0] = 0x01;
    packet[1] = sequence;
    packet[2..4].copy_from_slice(&nonce.to_le_bytes());
    packet[4] = client_option;
    packet
}

/// First login digest assembled at `0x1002ef00`: `03 01`, the four challenge
/// bytes, then at most 16 bytes of the zero-terminated account name.
/// Account encoding is intentionally left to the caller; the original hashes
/// bytes, not Unicode code points.
pub fn first_login_digest(challenge: [u8; 4], account: &[u8]) -> [u8; 16] {
    let account_len = account
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(account.len())
        .min(16);
    let mut input = Vec::with_capacity(6 + account_len);
    input.extend_from_slice(&[0x03, 0x01]);
    input.extend_from_slice(&challenge);
    input.extend_from_slice(&account[..account_len]);
    md5::digest(&input)
}

/// Second login digest at `0x1002f2f4`. The original zeroes a buffer, writes
/// `01 || account[0..16] || challenge`, then hashes `account_len + 9` bytes,
/// which includes four trailing zero bytes.
pub fn second_login_digest(challenge: [u8; 4], account: &[u8]) -> [u8; 16] {
    let account_len = account
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(account.len())
        .min(16);
    let mut input = Vec::with_capacity(account_len + 9);
    input.push(0x01);
    input.extend_from_slice(&account[..account_len]);
    input.extend_from_slice(&challenge);
    input.extend_from_slice(&[0u8; 4]);
    md5::digest(&input)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginPrefixError {
    PasswordTooLong,
}

/// Verified first 64 bytes of the larger login frame at `0x1002ef00`.
/// The complete frame also contains interface addresses, control extensions,
/// and a checksum, so this prefix must never be sent on its own.
pub fn login_frame_prefix(
    challenge: [u8; 4],
    account: &[u8],
    password: &[u8],
    control_check_status: u8,
    sequence: u8,
    mac: [u8; 6],
) -> Result<[u8; 64], LoginPrefixError> {
    let password_len = password
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(password.len());
    if password_len > 36 {
        return Err(LoginPrefixError::PasswordTooLong);
    }
    let digest = first_login_digest(challenge, account);
    let mut prefix = [0u8; 64];
    prefix[0] = 0x03;
    prefix[1] = 0x01;
    prefix[3] = password_len as u8;
    prefix[4..20].copy_from_slice(&digest);
    prefix[20..20 + password_len].copy_from_slice(&password[..password_len]);
    prefix[56] = control_check_status;
    prefix[57] = sequence;
    prefix[58..64].copy_from_slice(&masked_login_mac(mac, digest));
    Ok(prefix)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginCoreError {
    PasswordTooLong,
    TooManyAddresses,
}

/// First 105 bytes of the login structure. The original (`0x1002f3d2`)
/// hashes its first 101 bytes, including the temporary `14 00 07 0b` marker,
/// then replaces that marker and the following four bytes with the first
/// eight digest bytes. The remaining login extensions are still undecoded.
pub fn login_frame_core(
    challenge: [u8; 4],
    account: &[u8],
    password: &[u8],
    control_check_status: u8,
    sequence: u8,
    mac: [u8; 6],
    ipv4_addresses: &[[u8; 4]],
) -> Result<[u8; 105], LoginCoreError> {
    if ipv4_addresses.len() > 4 {
        return Err(LoginCoreError::TooManyAddresses);
    }
    let prefix = login_frame_prefix(
        challenge,
        account,
        password,
        control_check_status,
        sequence,
        mac,
    )
    .map_err(|_| LoginCoreError::PasswordTooLong)?;
    let mut frame = [0u8; 105];
    frame[..64].copy_from_slice(&prefix);
    frame[64..80].copy_from_slice(&second_login_digest(challenge, account));
    frame[80] = ipv4_addresses.len() as u8;
    for (slot, address) in frame[81..97]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(ipv4_addresses)
    {
        slot.copy_from_slice(address);
    }
    frame[97..101].copy_from_slice(&[0x14, 0, 7, 0x0b]);
    let digest = md5::digest(&frame[..101]);
    frame[97..105].copy_from_slice(&digest[..8]);
    Ok(frame)
}

/// Raw fields in the default `0x138`-byte login body. Their exact sources in
/// network configuration and the original optional extensions are still
/// being mapped; callers must not invent values for live authentication.
pub struct LoginBaseFields<'a> {
    pub challenge: [u8; 4],
    pub account: &'a [u8],
    pub password: &'a [u8],
    pub control_check_status: u8,
    pub sequence: u8,
    pub mac: [u8; 6],
    pub ipv4_addresses: &'a [[u8; 4]],
    pub byte_105: u8,
    pub block_110: &'a [u8; 200],
    pub byte_310: u8,
    pub byte_311: u8,
    pub field_318: u16,
}

/// Builds the 328-byte login variant without either optional extension.
/// The original copies a 200-byte body at offset 110, adds a 14-byte trailer
/// after offset `0x138`, aligns to four bytes, and fills the checksum slot.
/// The output is a structural test artifact until all raw fields are mapped.
pub fn login_base_packet(fields: &LoginBaseFields<'_>) -> Result<Vec<u8>, LoginCoreError> {
    let core = login_frame_core(
        fields.challenge,
        fields.account,
        fields.password,
        fields.control_check_status,
        fields.sequence,
        fields.mac,
        fields.ipv4_addresses,
    )?;
    let mut packet = vec![0u8; 328];
    packet[..105].copy_from_slice(&core);
    packet[105] = fields.byte_105;
    packet[110..310].copy_from_slice(fields.block_110);
    packet[310] = fields.byte_310;
    packet[311] = fields.byte_311;
    packet[312..314].copy_from_slice(&[0x02, 0x0c]);
    packet[318..320].copy_from_slice(&fields.field_318.to_le_bytes());
    packet[320..326].copy_from_slice(&fields.mac);
    finish_login_checksum(&mut packet, 314).expect("fixed packet has room for checksum");
    Ok(packet)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginChecksumError {
    MisalignedPacketLength,
    ChecksumOutsidePacket,
}

/// Final login-frame checksum at `0x1002ef00`. The caller must have already
/// built and zero-padded the complete frame to a four-byte boundary. The
/// checksum slot is first seeded with `0x11072601`, then the XOR of every
/// little-endian word (starting at `0x4d2`) is multiplied by `0x7b0` and
/// written back. The checksum slot itself may be unaligned.
pub fn finish_login_checksum(
    packet: &mut [u8],
    checksum_offset: usize,
) -> Result<u32, LoginChecksumError> {
    if !packet.len().is_multiple_of(4) {
        return Err(LoginChecksumError::MisalignedPacketLength);
    }
    if checksum_offset
        .checked_add(4)
        .is_none_or(|end| end > packet.len())
    {
        return Err(LoginChecksumError::ChecksumOutsidePacket);
    }
    packet[checksum_offset..checksum_offset + 4].copy_from_slice(&0x11072601u32.to_le_bytes());
    let xor = packet
        .as_chunks::<4>()
        .0
        .iter()
        .fold(0x4d2u32, |value, bytes| value ^ u32::from_le_bytes(*bytes));
    let checksum = xor.wrapping_mul(0x7b0);
    packet[checksum_offset..checksum_offset + 4].copy_from_slice(&checksum.to_le_bytes());
    Ok(checksum)
}

/// Six on-wire MAC bytes at offsets `0x3a..0x3f` of the original login
/// structure (`0x1002ef00`). Each byte is XORed with the corresponding byte
/// of the first login digest.
pub fn masked_login_mac(mac: [u8; 6], first_digest: [u8; 16]) -> [u8; 6] {
    let mut masked = [0u8; 6];
    for ((output, mac_byte), digest_byte) in masked.iter_mut().zip(mac).zip(first_digest) {
        *output = mac_byte ^ digest_byte;
    }
    masked
}

/// Unencrypted client-ping branch at `0x1002eae0`. The original sends either
/// 38 bytes or 42 bytes, depending on a separate extension flag. The encrypted
/// and newer ping variants have separate layouts and are not handled here.
pub fn unencrypted_client_ping(
    first_digest: [u8; 16],
    online_fields: [u8; 16],
    nonce: u16,
    access_control: Option<u32>,
) -> Vec<u8> {
    let mut packet = [0u8; 42];
    packet[0] = 0xff;
    packet[1..17].copy_from_slice(&first_digest);
    packet[20..36].copy_from_slice(&online_fields);
    packet[36..38].copy_from_slice(&nonce.to_le_bytes());
    if let Some(control) = access_control {
        packet[38..42].copy_from_slice(&control.to_le_bytes());
        packet.to_vec()
    } else {
        packet[..38].to_vec()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutPacketError {
    PasswordTooLong,
}

/// 80-byte unencrypted logout frame built by the original at `0x1002e140`.
/// The 16-byte digest hashes `06 01 || challenge || account`. The original
/// stores up to 36 password bytes in this fixed frame; longer input is
/// rejected here instead of overflowing the adjacent fields.
pub fn logout_packet(
    challenge: [u8; 4],
    account: &[u8],
    password: &[u8],
    control_check_status: u8,
    sequence: u8,
    mac: [u8; 6],
    online_fields: [u8; 16],
) -> Result<[u8; 80], LogoutPacketError> {
    let password_len = password
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(password.len());
    if password_len > 36 {
        return Err(LogoutPacketError::PasswordTooLong);
    }
    let account_len = account
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(account.len());
    let mut digest_input = Vec::with_capacity(6 + account_len);
    digest_input.extend_from_slice(&[0x06, 0x01]);
    digest_input.extend_from_slice(&challenge);
    digest_input.extend_from_slice(&account[..account_len]);
    let digest = md5::digest(&digest_input);

    let mut packet = [0u8; 80];
    packet[0] = 0x06;
    packet[1] = 0x01;
    packet[3] = password_len as u8;
    packet[4..20].copy_from_slice(&digest);
    packet[20..20 + password_len].copy_from_slice(&password[..password_len]);
    packet[56] = control_check_status;
    packet[57] = sequence;
    packet[58..64].copy_from_slice(&masked_login_mac(mac, digest));
    packet[64..80].copy_from_slice(&online_fields);
    Ok(packet)
}

/// Fields read by the original challenge handler (`0x10030890`).
/// Undecoded protocol fields stay as raw bytes until their meaning is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChallengeResponse<'a> {
    pub challenge: [u8; 4],
    pub field_08: [u8; 2],
    pub field_0a: [u8; 2],
    pub field_0c: [u8; 2],
    pub field_10: [u8; 4],
    pub field_14: [u8; 4],
    pub system_auth_option: u16,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeError {
    TooShort,
    UnexpectedType,
    SequenceMismatch,
    NonceMismatch,
}

impl<'a> ChallengeResponse<'a> {
    /// The handler checks type, sequence and nonce before using fields through
    /// offset `0x2b`. It may read more for optional authentication extensions;
    /// those extensions are not parsed here.
    pub fn parse(
        bytes: &'a [u8],
        expected_sequence: u8,
        expected_nonce: u16,
    ) -> Result<Self, ChallengeError> {
        if bytes.len() < 0x2c {
            return Err(ChallengeError::TooShort);
        }
        if bytes[0] != 0x02 {
            return Err(ChallengeError::UnexpectedType);
        }
        if bytes[1] != expected_sequence {
            return Err(ChallengeError::SequenceMismatch);
        }
        if u16::from_le_bytes([bytes[2], bytes[3]]) != expected_nonce {
            return Err(ChallengeError::NonceMismatch);
        }

        Ok(Self {
            challenge: bytes[4..8].try_into().expect("length checked"),
            field_08: bytes[8..10].try_into().expect("length checked"),
            field_0a: bytes[10..12].try_into().expect("length checked"),
            field_0c: bytes[12..14].try_into().expect("length checked"),
            field_10: bytes[16..20].try_into().expect("length checked"),
            field_14: bytes[20..24].try_into().expect("length checked"),
            system_auth_option: u16::from_le_bytes([bytes[0x2a], bytes[0x2b]]),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_request_matches_original_layout() {
        let packet = challenge_request(0x42, 0x1234, 0x7e);
        assert_eq!(&packet[..5], &[0x01, 0x42, 0x34, 0x12, 0x7e]);
        assert!(packet[5..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn challenge_response_checks_echo_before_extracting_fields() {
        let mut bytes = [0u8; 0x2c];
        bytes[..4].copy_from_slice(&[0x02, 0x42, 0x34, 0x12]);
        bytes[4..8].copy_from_slice(&[1, 2, 3, 4]);
        bytes[0x2a..0x2c].copy_from_slice(&0x8001u16.to_le_bytes());
        let parsed = ChallengeResponse::parse(&bytes, 0x42, 0x1234).unwrap();
        assert_eq!(parsed.challenge, [1, 2, 3, 4]);
        assert_eq!(parsed.system_auth_option, 0x8001);
        assert_eq!(
            ChallengeResponse::parse(&bytes, 0x43, 0x1234),
            Err(ChallengeError::SequenceMismatch)
        );
        assert_eq!(
            ChallengeResponse::parse(&bytes, 0x42, 0x1235),
            Err(ChallengeError::NonceMismatch)
        );
        assert_eq!(
            ChallengeResponse::parse(&bytes[..0x2b], 0x42, 0x1234),
            Err(ChallengeError::TooShort)
        );
    }

    #[test]
    fn first_digest_truncates_account_as_original_does() {
        let challenge = [1, 2, 3, 4];
        assert_eq!(
            first_login_digest(challenge, b"1234567890abcdef-extra"),
            first_login_digest(challenge, b"1234567890abcdef")
        );
        assert_eq!(
            first_login_digest(challenge, b"alice\0ignored"),
            first_login_digest(challenge, b"alice")
        );
        assert_eq!(
            first_login_digest(challenge, b"alice"),
            [
                0x41, 0xae, 0xdf, 0x1b, 0x46, 0x0f, 0x32, 0x42, 0x44, 0x94, 0x1f, 0xf3, 0xc3, 0xf6,
                0x4e, 0xc4,
            ]
        );
    }

    #[test]
    fn second_digest_includes_four_zero_bytes_after_challenge() {
        let expected = [
            0x7a, 0xc2, 0x90, 0xc2, 0xc5, 0x62, 0x27, 0xc8, 0x82, 0xdf, 0xdc, 0x8b, 0xb0, 0x25,
            0x56, 0x29,
        ];
        assert_eq!(second_login_digest([1, 2, 3, 4], b"alice"), expected);
        assert_eq!(
            second_login_digest([1, 2, 3, 4], b"1234567890abcdef-extra"),
            second_login_digest([1, 2, 3, 4], b"1234567890abcdef")
        );
    }

    #[test]
    fn login_core_hashes_address_list_and_replaces_marker() {
        let core = login_frame_core(
            [1, 2, 3, 4],
            b"alice",
            b"secret",
            0x42,
            9,
            [0, 1, 2, 3, 4, 5],
            &[[10, 0, 0, 1], [10, 0, 0, 2]],
        )
        .unwrap();
        assert_eq!(core[80], 2);
        assert_eq!(&core[81..89], &[10, 0, 0, 1, 10, 0, 0, 2]);
        assert!(core[89..97].iter().all(|byte| *byte == 0));
        assert_eq!(
            &core[97..105],
            &[0xd8, 0x53, 0xcd, 0x57, 0x06, 0xeb, 0x45, 0x86]
        );
        assert_eq!(
            login_frame_core([0; 4], b"a", b"p", 0, 0, [0; 6], &[[0; 4]; 5]),
            Err(LoginCoreError::TooManyAddresses)
        );
    }

    #[test]
    fn base_login_variant_places_raw_block_and_trailer() {
        let mut block = [0u8; 200];
        block[0] = 0xaa;
        block[199] = 0xbb;
        let packet = login_base_packet(&LoginBaseFields {
            challenge: [1, 2, 3, 4],
            account: b"alice",
            password: b"secret",
            control_check_status: 0x42,
            sequence: 9,
            mac: [0, 1, 2, 3, 4, 5],
            ipv4_addresses: &[[10, 0, 0, 1], [10, 0, 0, 2]],
            byte_105: 1,
            block_110: &block,
            byte_310: 2,
            byte_311: 3,
            field_318: 0x1234,
        })
        .unwrap();
        assert_eq!(packet.len(), 328);
        assert_eq!(packet[105], 1);
        assert_eq!(packet[110], 0xaa);
        assert_eq!(packet[309], 0xbb);
        assert_eq!(
            &packet[310..326],
            &[
                2, 3, 2, 12, 0xc0, 0xa6, 0xe5, 0x88, 0x34, 0x12, 0, 1, 2, 3, 4, 5
            ]
        );
        assert_eq!(&packet[326..], &[0, 0]);
    }

    #[test]
    fn login_prefix_places_first_digest_password_and_masked_mac() {
        let prefix = login_frame_prefix(
            [1, 2, 3, 4],
            b"alice",
            b"secret",
            0x42,
            0x09,
            [0, 1, 2, 3, 4, 5],
        )
        .unwrap();
        assert_eq!(&prefix[..4], &[3, 1, 0, 6]);
        assert_eq!(
            &prefix[4..20],
            &[
                0x41, 0xae, 0xdf, 0x1b, 0x46, 0x0f, 0x32, 0x42, 0x44, 0x94, 0x1f, 0xf3, 0xc3, 0xf6,
                0x4e, 0xc4,
            ]
        );
        assert_eq!(&prefix[20..26], b"secret");
        assert!(prefix[26..56].iter().all(|byte| *byte == 0));
        assert_eq!(&prefix[56..58], &[0x42, 0x09]);
        assert_eq!(&prefix[58..64], &[0x41, 0xaf, 0xdd, 0x18, 0x42, 0x0a]);
        assert_eq!(
            login_frame_prefix([0; 4], b"a", &[1; 37], 0, 0, [0; 6]),
            Err(LoginPrefixError::PasswordTooLong)
        );
    }

    #[test]
    fn checksum_uses_seed_xor_and_original_multiplier() {
        let mut packet: Vec<u8> = (0..16).collect();
        let value = finish_login_checksum(&mut packet, 6).unwrap();
        assert_eq!(value, 0xe6ade2f0);
        assert_eq!(&packet[6..10], &[0xf0, 0xe2, 0xad, 0xe6]);
        assert_eq!(
            finish_login_checksum(&mut packet[..15], 6),
            Err(LoginChecksumError::MisalignedPacketLength)
        );
        assert_eq!(
            finish_login_checksum(&mut packet, 13),
            Err(LoginChecksumError::ChecksumOutsidePacket)
        );
    }

    #[test]
    fn masked_mac_uses_first_six_digest_bytes() {
        let digest = first_login_digest([1, 2, 3, 4], b"alice");
        assert_eq!(
            masked_login_mac([0, 1, 2, 3, 4, 5], digest),
            [0x41, 0xaf, 0xdd, 0x18, 0x42, 0x0a]
        );
    }

    #[test]
    fn unencrypted_ping_layout_has_both_original_lengths() {
        let digest = [0x11; 16];
        let online = [0x22; 16];
        let short = unencrypted_client_ping(digest, online, 0x1234, None);
        assert_eq!(short.len(), 0x26);
        assert_eq!(short[0], 0xff);
        assert_eq!(&short[1..17], &digest);
        assert_eq!(&short[17..20], &[0, 0, 0]);
        assert_eq!(&short[20..36], &online);
        assert_eq!(&short[36..38], &[0x34, 0x12]);

        let long = unencrypted_client_ping(digest, online, 0x1234, Some(0x01020304));
        assert_eq!(long.len(), 0x2a);
        assert_eq!(&long[..38], &short);
        assert_eq!(&long[38..42], &[4, 3, 2, 1]);
    }

    #[test]
    fn logout_frame_matches_decompiled_field_offsets() {
        let packet = logout_packet(
            [1, 2, 3, 4],
            b"alice",
            b"secret",
            0x42,
            0x09,
            [0, 1, 2, 3, 4, 5],
            [0x55; 16],
        )
        .unwrap();
        assert_eq!(&packet[..4], &[6, 1, 0, 6]);
        assert_eq!(
            &packet[4..20],
            &[
                0x37, 0x65, 0x6f, 0xeb, 0xe7, 0xe6, 0x92, 0x87, 0xe4, 0x97, 0x69, 0x3a, 0xeb, 0x62,
                0x98, 0x8d,
            ]
        );
        assert_eq!(&packet[20..26], b"secret");
        assert!(packet[26..56].iter().all(|byte| *byte == 0));
        assert_eq!(&packet[56..58], &[0x42, 0x09]);
        assert_eq!(&packet[58..64], &[0x37, 0x64, 0x6d, 0xe8, 0xe3, 0xe3]);
        assert_eq!(&packet[64..80], &[0x55; 16]);
        assert_eq!(
            logout_packet([0; 4], b"a", &[1; 37], 0, 0, [0; 6], [0; 16]),
            Err(LogoutPacketError::PasswordTooLong)
        );
    }
}
