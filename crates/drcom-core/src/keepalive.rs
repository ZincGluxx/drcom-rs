//! Keep-alive packet construction and reply classification.
//!
//! Two families of `0xff` / `0x07` packets exist, and this module builds both:
//!
//! * the 42-byte primary packet ([`primary_keepalive`]), sent every cycle;
//! * the 40-byte `0x07` packet, in three on-wire forms — the extra form that
//!   opens a keep-alive run and returns every tenth round, and the `data1` /
//!   `data3` pair sent every cycle ([`build_packet`],
//!   [`build_packet_negotiated`]).
//!
//! Two independent implementations of the same protocol are transcribed in
//! `reference/`. They agree byte-for-byte on the 40-byte layout, which is why a
//! single builder is parameterised here rather than duplicated:
//!
//! * `reference/gen_vectors.py` (third-party Python client) — hard-codes the
//!   nonce at offsets 8..10 as `2f 12` and the version at 6..8 as `dc 02`, and
//!   leaves the checksum slot zero.
//! * `reference/gen_official_vectors.py` (Java/Android rewrite) — randomises the
//!   nonce, takes the version from the keep-alive reply, and folds a real
//!   checksum into the `data3` form.
//!
//! [`build_packet`] reproduces the Python client exactly and is what
//! `reference/reference_vectors.txt` pins. [`build_packet_negotiated`] exposes
//! the differences as parameters.

/// The nonce the Python client hard-codes. Its own `random` argument is dead
/// code: the parameter is accepted and then ignored, so every packet in a
/// session carries the same two bytes here.
pub const REFERENCE_NONCE: [u8; 2] = [0x2f, 0x12];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAlivePacketType {
    /// Sent form of the extra packet and of `data1`; the reply is `0x02`.
    Type1,
    /// Sent form of `data3`; the reply is `0x04`.
    Type3,
}

/// Whether the `data3` form carries a folded checksum or leaves the slot zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAliveChecksum {
    /// What the Python client actually sends: four zero bytes. The `data3`
    /// checksum was filled with zeros by a 2014 edit in that client, and the
    /// deployment accepts it.
    Zero,
    /// Fold the first 28 bytes. The Java rewrite reads the client address into
    /// the checksum slot, folds `data[0..28]` with it in place, then overwrites
    /// the slot with the result and moves the address to offset 28 — so the
    /// folded input is `data[0..24] || client address`.
    Fold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAliveResponse {
    /// `07 .. 10 ..`: the server is pushing a message rather than acknowledging
    /// the keep-alive. The Python client treats it as a prompt to resend, and
    /// the Java rewrite logs it as a file packet.
    Message {
        counter: u8,
    },
    Continue {
        counter: u8,
    },
    Data {
        counter: u8,
    },
    Invalid,
}

/// Builds the 40-byte `0x07` packet with the profile the Python client uses.
///
/// `first` selects the extra form (`0f 27` at offsets 6..8), which opens a
/// keep-alive run and is repeated every tenth round. It is also what a resend
/// after a `07 .. 10` message packet must *not* use: the reference client
/// switches to the negotiated version for every resend, so only the first
/// datagram of a run ever carries the marker.
pub fn build_packet(
    sequence: u8,
    tail: [u8; 4],
    packet_type: KeepAlivePacketType,
    first: bool,
    local_ipv4: [u8; 4],
    negotiated_version: [u8; 2],
) -> [u8; 40] {
    build_packet_negotiated(
        sequence,
        tail,
        packet_type,
        first,
        REFERENCE_NONCE,
        negotiated_version,
        local_ipv4,
        KeepAliveChecksum::Zero,
    )
}

/// Builds the 40-byte `0x07` packet with every profile difference exposed.
///
/// Offsets: `07`, sequence, `28 00 0b`, type, the extra marker or the version,
/// the two nonce bytes, six zero bytes, the four-byte tail, four zero bytes,
/// the checksum slot, the client address, and eight trailing zero bytes.
#[allow(clippy::too_many_arguments)]
pub fn build_packet_negotiated(
    sequence: u8,
    tail: [u8; 4],
    packet_type: KeepAlivePacketType,
    first: bool,
    nonce: [u8; 2],
    negotiated_version: [u8; 2],
    client_ipv4: [u8; 4],
    checksum: KeepAliveChecksum,
) -> [u8; 40] {
    let mut packet = [0u8; 40];
    packet[0] = 0x07;
    packet[1] = sequence;
    packet[2] = 0x28;
    packet[4] = 0x0b;
    packet[5] = match packet_type {
        KeepAlivePacketType::Type1 => 1,
        KeepAlivePacketType::Type3 => 3,
    };
    packet[6..8].copy_from_slice(if first {
        &[0x0f, 0x27]
    } else {
        &negotiated_version
    });
    packet[8..10].copy_from_slice(&nonce);
    packet[16..20].copy_from_slice(&tail);

    // The client address and the checksum slot only exist in the `data3` form.
    if packet_type == KeepAlivePacketType::Type3 {
        match checksum {
            KeepAliveChecksum::Zero => packet[28..32].copy_from_slice(&client_ipv4),
            KeepAliveChecksum::Fold => {
                packet[24..28].copy_from_slice(&client_ipv4);
                let folded = crc(&packet[..28]);
                packet[24..28].copy_from_slice(&folded);
                packet[28..32].copy_from_slice(&client_ipv4);
            }
        }
    }
    packet
}

/// Folds every two-byte window little-endian, multiplies by 711 and writes the
/// low four bytes little-endian. Transcribed from `ByteUtil.crc` in the Java
/// rewrite, which in turn reproduces the older clients' `packet_CRC`.
pub fn crc(data: &[u8]) -> [u8; 4] {
    let mut value = 0u32;
    for pair in data.as_chunks::<2>().0 {
        value ^= u32::from(u16::from_le_bytes([pair[0], pair[1]]));
    }
    value.wrapping_mul(711).to_le_bytes()
}

/// The version pair a `0xff`/`0x07` reply carries at offsets 28..30.
///
/// The Python client hard-codes `dc 02` and its own comment records that the
/// value it captured differed, so the reply is the better source whenever the
/// server sends one. Both the Java rewrite and the Android port read it here.
pub fn negotiated_version(response: &[u8]) -> Option<[u8; 2]> {
    (response.len() >= 30 && response[0] == 0x07).then(|| [response[28], response[29]])
}

/// True for the `07 .. 10` reply the server uses to push a message.
///
/// Both reference implementations treat it as a retry prompt rather than a
/// keep-alive acknowledgement: the Python client resends the same packet with
/// an incremented counter, and the Java rewrite records that the first one
/// arrives right after login and carries the text `This Program can not run in
/// dos mode`.
pub fn is_message_packet(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0x07 && bytes[2] == 0x10
}

/// Primary keep-alive sent every 20 seconds before the 40-byte pair.
///
/// Both the reference Python client and the shipping C# client emit this exact
/// 42-byte frame: `ff`, the login `md51` digest, three zero bytes, the 16-byte
/// session cookie from the login response, the clock nonce, then four zero
/// bytes. The nonce is big-endian, unlike the little-endian challenge nonce,
/// and the trailing four bytes are zero rather than an access-control value.
/// `protocol.rs` keeps the original binary's 38-byte and optional-extension
/// variants of this packet; this is the profile the campus deployment expects.
pub fn primary_keepalive(
    first_digest: [u8; 16],
    session_cookie: [u8; 16],
    unix_seconds: u64,
) -> [u8; 42] {
    let mut packet = [0u8; 42];
    packet[0] = 0xff;
    packet[1..17].copy_from_slice(&first_digest);
    packet[20..36].copy_from_slice(&session_cookie);
    let nonce = (unix_seconds % 0xffff) as u16;
    packet[36..38].copy_from_slice(&nonce.to_be_bytes());
    packet
}

pub fn classify_response(
    bytes: &[u8],
    expected_sequence: u8,
    handshake: bool,
) -> KeepAliveResponse {
    if bytes.len() < if handshake { 4 } else { 20 } || bytes[0] != 0x07 {
        return KeepAliveResponse::Invalid;
    }
    if is_message_packet(bytes) {
        return KeepAliveResponse::Message { counter: bytes[1] };
    }
    if bytes[2] == 0x28 && (bytes[1] == expected_sequence || bytes[1] == 0) {
        return KeepAliveResponse::Continue { counter: bytes[1] };
    }
    if !handshake {
        return KeepAliveResponse::Data { counter: bytes[1] };
    }
    KeepAliveResponse::Invalid
}

pub fn update_tail(destination: &mut [u8; 4], response: &[u8]) {
    if response.len() >= 20 {
        destination.copy_from_slice(&response[16..20]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_matches_reference_offsets() {
        let packet = build_packet(
            4,
            [1, 2, 3, 4],
            KeepAlivePacketType::Type3,
            true,
            [10, 0, 0, 9],
            [0x12, 0x34],
        );
        assert_eq!(
            &packet[..10],
            &[7, 4, 0x28, 0, 0x0b, 3, 0x0f, 0x27, 0x2f, 0x12]
        );
        assert_eq!(&packet[16..20], &[1, 2, 3, 4]);
        assert_eq!(&packet[28..32], &[10, 0, 0, 9]);
    }

    #[test]
    fn response_classification_and_tail_update() {
        let mut response = [0u8; 20];
        response[..4].copy_from_slice(&[7, 2, 0x28, 0]);
        response[16..20].copy_from_slice(&[9, 8, 7, 6]);
        assert_eq!(
            classify_response(&response, 2, false),
            KeepAliveResponse::Continue { counter: 2 }
        );
        let mut tail = [0u8; 4];
        update_tail(&mut tail, &response);
        assert_eq!(tail, [9, 8, 7, 6]);
    }

    #[test]
    fn primary_keepalive_matches_reference_bytes() {
        let first = crate::reference_login::first_digest(&[0x1a, 0x2b, 0x3c, 0x4d], b"testpass");
        let cookie = [
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
            0x1e, 0x1f,
        ];
        let packet = primary_keepalive(first, cookie, 1_595_812_411);
        assert_eq!(
            packet.to_vec(),
            crate::reference_vectors::vector("keepalive.primary")
        );
        assert_eq!(packet.len(), 0x2a);
        assert_eq!(&packet[20..36], &cookie);
        assert_eq!(&packet[36..38], &[0x89, 0x59]);
        assert!(packet[38..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn three_packet_forms_match_reference_bytes() {
        let tail = [0xa1, 0xb2, 0xc3, 0xd4];
        let first = build_packet(
            0,
            [0u8; 4],
            KeepAlivePacketType::Type1,
            true,
            [10, 0, 0, 9],
            [0xdc, 0x02],
        );
        assert_eq!(
            first.to_vec(),
            crate::reference_vectors::vector("keepalive.type1.first")
        );
        let next = build_packet(
            7,
            tail,
            KeepAlivePacketType::Type1,
            false,
            [10, 0, 0, 9],
            [0xdc, 0x02],
        );
        assert_eq!(
            next.to_vec(),
            crate::reference_vectors::vector("keepalive.type1.next")
        );
        let third = build_packet(
            8,
            tail,
            KeepAlivePacketType::Type3,
            false,
            [10, 0, 0, 9],
            [0xdc, 0x02],
        );
        assert_eq!(
            third.to_vec(),
            crate::reference_vectors::vector("keepalive.type3.next")
        );
        // The reference also places the local address at offset 28, not 24.
        assert_eq!(&third[28..32], &[10, 0, 0, 9]);
    }

    #[test]
    fn official_packets_match_the_java_implementation() {
        let tail = [0xa1, 0xb2, 0xc3, 0xd4];
        let client_ip = [10, 0, 0, 9];
        let version = [0xdc, 0x02];
        let extra = build_packet_negotiated(
            0,
            [0u8; 4],
            KeepAlivePacketType::Type1,
            true,
            [0x11, 0x22],
            version,
            client_ip,
            KeepAliveChecksum::Zero,
        );
        assert_eq!(
            extra.to_vec(),
            crate::reference_vectors::official_vector("official.keep40.extra")
        );
        let data1 = build_packet_negotiated(
            1,
            tail,
            KeepAlivePacketType::Type1,
            false,
            [0x33, 0x44],
            version,
            client_ip,
            KeepAliveChecksum::Zero,
        );
        assert_eq!(
            data1.to_vec(),
            crate::reference_vectors::official_vector("official.keep40.data1")
        );
        let data3 = build_packet_negotiated(
            2,
            tail,
            KeepAlivePacketType::Type3,
            false,
            [0x55, 0x66],
            version,
            client_ip,
            KeepAliveChecksum::Fold,
        );
        assert_eq!(
            data3.to_vec(),
            crate::reference_vectors::official_vector("official.keep40.data3")
        );
        // Folding moves the address from the checksum slot to offset 28 and
        // leaves the two forms differing only in that slot and at 28..32.
        assert_eq!(&data3[28..32], &client_ip);
        assert_ne!(&data3[24..28], &[0, 0, 0, 0]);
    }

    #[test]
    fn official_checksum_folds_little_endian_words() {
        assert_eq!(
            crc(&(0u8..28).collect::<Vec<u8>>()).to_vec(),
            crate::reference_vectors::official_vector("official.crc.ramp")
        );
        // The folded input is the first 24 bytes of the packet followed by the
        // in-place client address, exactly as the Java arraycopy orders it.
        let mut folded_input = Vec::from(&[0x07, 0x00, 0x28, 0x00, 0x0b, 0x01, 0x0f, 0x27][..]);
        folded_input.extend_from_slice(&[0xa1, 0xb2, 0xc3, 0xd4]);
        assert_eq!(crc(&folded_input).len(), 4);
    }

    #[test]
    fn version_is_taken_from_the_reply_when_the_server_sends_one() {
        let mut reply = vec![0u8; 30];
        reply[0] = 0x07;
        reply[28] = 0x34;
        reply[29] = 0x12;
        assert_eq!(negotiated_version(&reply), Some([0x34, 0x12]));
        // A short reply, or one that is not a keep-alive reply, has no version.
        assert_eq!(negotiated_version(&reply[..29]), None);
        let mut not_keepalive = reply.clone();
        not_keepalive[0] = 0x04;
        assert_eq!(negotiated_version(&not_keepalive), None);
    }

    #[test]
    fn message_packets_are_never_mistaken_for_acknowledgements() {
        // A real one carries text, so it clears the twenty-byte floor that the
        // non-handshake branch applies before it looks at the type byte.
        let mut message = vec![0u8; 32];
        message[..4].copy_from_slice(&[0x07, 0x00, 0x10, 0x00]);
        message[20..].copy_from_slice(b"This Program");
        assert!(is_message_packet(&message));
        // A message reply wins over the handshake branch, which is what the
        // Python client relies on when it resends after the first extra packet.
        assert_eq!(
            classify_response(&message, 0, true),
            KeepAliveResponse::Message { counter: 0 }
        );
        assert_eq!(
            classify_response(&message, 0, false),
            KeepAliveResponse::Message { counter: 0 }
        );
        assert!(!is_message_packet(&[0x07, 0x00, 0x28, 0x00]));
        assert!(!is_message_packet(&[0x07]));
    }
}
