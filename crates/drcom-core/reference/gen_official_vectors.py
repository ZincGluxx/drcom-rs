#!/usr/bin/env python3
# coding: utf-8
"""Golden vectors for the *official-capture* protocol generation.

`gen_vectors.py` reproduces the third-party Python client, which is the profile
this port follows by default.  A second generation is documented in the
[jlu-drcom-client] repository: the Java/JavaFX rewrite by YouthLin and the
Android port derived from it.  That generation was written against captured
official-client traffic, and it disagrees with the Python client in a handful of
bytes and in one algorithm (the checksum).

This script is a faithful transcription of

    jlu-drcom-java/src/main/java/com/youthlin/jlu/drcom/util/ByteUtil.java
    jlu-drcom-java/src/main/java/com/youthlin/jlu/drcom/task/DrcomTask.java

so that the Rust port can be checked against those bytes rather than against a
hand-written expectation.  Random fields are injected explicitly, because the
Java code calls `ByteUtil.randByte()` in four places.

Run:  python reference/gen_official_vectors.py > reference/official_vectors.txt

[jlu-drcom-client]: https://github.com/ZincGluxx/jlu-drcom-client
"""

from hashlib import md5

# ---------------------------------------------------------------- primitives


def md5sum(data):
    return md5(data).digest()


def ljust(src, count, fill=b"\x00"):
    """ByteUtil.ljust: truncate when longer, pad when shorter."""
    if len(src) >= count:
        return src[:count]
    return src + fill * (count - len(src))


def ror(digest, password):
    """ByteUtil.ror: xor then rotate left by three, per byte."""
    out = bytearray()
    for i in range(len(password)):
        x = digest[i] ^ password[i]
        out.append(((x << 3) & 0xFF) + (x >> 5))
    return bytes(out)


def checksum(data):
    """ByteUtil.checksum.

    Folds every four-byte window *little-endian* into a `0x4d2` seed with a
    clean four-byte stride, multiplies by 1968 modulo 2**32, and writes the
    result little-endian.

    The Python client groups the same buffer with `re.findall(b'....', s)`.
    Python's `.` does not match `0x0a`, so that scan skips a byte whenever a
    window contains a newline and every later window is taken from a shifted
    offset.  ByteUtil.java documents the difference and states that the Python
    value is the wrong one, which is the second independent confirmation of the
    quirk recorded in `reference_login.rs`.
    """
    value = 0x4D2
    offset = 0
    while offset + 4 <= len(data):
        value ^= int.from_bytes(data[offset:offset + 4], "little")
        offset += 4
    # Trailing one to three bytes are a partial window, right-aligned so they
    # occupy the low end of the word.
    if offset < len(data):
        tail = bytearray(4)
        rest = data[offset:]
        tail[4 - len(rest):] = rest
        value ^= int.from_bytes(bytes(tail), "little")
    value = (value * 1968) & 0xFFFFFFFF
    return value.to_bytes(4, "little")


def crc(data):
    """ByteUtil.crc.

    Folds every two-byte window little-endian, multiplies by 711, and writes
    the (possibly shorter) result little-endian, zero-extending to four bytes.
    """
    value = 0
    offset = 0
    while offset + 2 <= len(data):
        value ^= int.from_bytes(data[offset:offset + 2], "little")
        offset += 2
    value *= 711
    return value.to_bytes(4, "little")


# ------------------------------------------------------------- keep-alive 40


def make_keep_packet_40(count, kind, extra, tail2, rand, version, client_ip):
    """DrcomTask.makeKeepPacket40.

    `kind` 1 emits the sent form of keep40_1 (and of keep40_extra), `kind` 2 the
    keep40_2 form, which carries the checksum and the client address.  The
    received types (0x02 and 0x04) never appear on the wire.
    """
    data = bytearray(40)
    data[0] = 0x07
    data[1] = count & 0xFF
    data[2] = 0x28
    data[3] = 0x00
    data[4] = 0x0B
    data[5] = 0x01 if (kind == 1 or extra) else 0x03
    if extra:
        data[6], data[7] = 0x0F, 0x27
    else:
        data[6], data[7] = version
    data[8], data[9] = rand
    data[16:20] = tail2
    if kind == 2:
        # The Java code pre-writes the client address into the checksum slot,
        # folds data[0:28] including it, then overwrites the slot with the
        # result and moves the address to offset 28.  The Python client instead
        # folds its own four-byte placeholder and sends a zero checksum.
        data[24:28] = client_ip
        data[24:28] = crc(bytes(data[0:28]))
        data[28:32] = client_ip
    return bytes(data)


# ------------------------------------------------------------------- login


def make_login_packet(username, password, salt, client_ip, mac, hostname,
                      rand):
    """DrcomTask.makeLoginPacket.  Returns the frame plus the saved md5a."""
    code, packet_type, eof = 0x03, 0x01, 0x00
    control_check, adapter_num, ip_dog = 0x20, 0x05, 0x01
    primary_dns, dhcp = bytes([10, 10, 10, 10]), bytes([0, 0, 0, 0])

    pass_len = min(len(password), 16)
    data_len = 334 + (pass_len - 1) // 4 * 4
    data = bytearray(data_len)

    data[0], data[1], data[2] = code, packet_type, eof
    data[3] = len(username) + 20

    md5a = md5sum(bytes([code, packet_type]) + salt + password)
    data[4:20] = md5a
    data[20:56] = ljust(username, 36)
    data[56], data[57] = control_check, adapter_num
    for i in range(6):
        data[58 + i] = md5a[i] ^ mac[i]
    data[64:80] = md5sum(bytes([0x01]) + password + salt + bytes(4))

    data[80] = 0x01
    data[81:85] = client_ip
    # 85..97 stay zero: three empty address slots.
    data[97], data[98], data[99], data[100] = 0x14, 0x00, 0x07, 0x0B
    data[97:105] = md5sum(bytes(data[0:101]))[:8]
    data[105] = ip_dog
    data[110:142] = ljust(hostname, 32)
    data[142:146] = primary_dns
    data[146:150] = dhcp

    data[162] = 0x94
    data[166] = 0x06
    data[170] = 0x02
    data[174] = 0xF0
    data[175] = 0x23
    data[178] = 0x02
    data[182:191] = bytes([0x44, 0x72, 0x43, 0x4F, 0x4D, 0x00, 0xCF, 0x07, 0x6A])

    # The key the Java author captured from the official client.  The Python
    # client sends a different forty-byte string in the same slot.
    data[246:286] = b"1c210c99585fd22ad03d35c956911aeec1eb449b"
    data[310] = 0x6A

    data[313] = pass_len
    data[314:314 + pass_len] = ror(md5a, password)
    data[314 + pass_len] = 0x02
    data[315 + pass_len] = 0x0C

    # The checksum input is the frame so far plus the six marker bytes plus the
    # first four MAC bytes -- four, not six: the Java arraycopy is length four.
    data[316 + pass_len:322 + pass_len] = bytes([0x01, 0x26, 0x07, 0x11, 0x00, 0x00])
    data[322 + pass_len:326 + pass_len] = mac[:4]
    fixed = checksum(bytes(data[0:326 + pass_len]))
    data[316 + pass_len:320 + pass_len] = fixed

    data[320 + pass_len] = 0x00
    data[321 + pass_len] = 0x00
    data[322 + pass_len:328 + pass_len] = mac

    zero_count = (4 - pass_len % 4) % 4
    for i in range(zero_count):
        data[328 + pass_len + i] = 0x00
    data[328 + pass_len + zero_count] = rand[0]
    data[329 + pass_len + zero_count] = rand[1]
    return bytes(data), md5a


# ------------------------------------------------------------------ logout


def make_logout_packet(username, password, salt, mac, tail1):
    """DrcomTask.makeLogoutPacket."""
    data = bytearray(80)
    data[0], data[1], data[2] = 0x06, 0x01, 0x00
    data[3] = len(username) + 20
    digest = md5sum(bytes([0x06, 0x01]) + salt + password)
    data[4:20] = digest
    data[20:56] = ljust(username, 36)
    data[56], data[57] = 0x20, 0x05
    for i in range(6):
        data[58 + i] = digest[i] ^ mac[i]
    data[64:80] = tail1
    return bytes(data)


# -------------------------------------------------------------------- main

SALT = bytes.fromhex("1a2b3c4d")
USER = b"testuser"
PASSWORD = b"testpass"
MAC = bytes.fromhex("112288776655")
CLIENT_IP = bytes([10, 0, 0, 9])
HOSTNAME = b"TESTHOST"
TAIL1 = bytes.fromhex("a1b2c3d4e5f60718293a4b5c6d7e8f90")
TAIL2 = bytes.fromhex("a1b2c3d4")
VERSION = bytes.fromhex("dc02")


def emit(name, data):
    print("%s = %s" % (name, data.hex()))


def main():
    emit("official.crc.ramp", crc(bytes(range(28))))
    emit("official.crc.tail2", crc(bytes.fromhex("070028000b010f27") + TAIL2))
    emit("official.keep40.extra",
         make_keep_packet_40(0, 1, True, bytes(4), bytes([0x11, 0x22]), VERSION, CLIENT_IP))
    emit("official.keep40.data1",
         make_keep_packet_40(1, 1, False, TAIL2, bytes([0x33, 0x44]), VERSION, CLIENT_IP))
    emit("official.keep40.data3",
         make_keep_packet_40(2, 2, False, TAIL2, bytes([0x55, 0x66]), VERSION, CLIENT_IP))
    emit("official.keep40.negotiated",
         make_keep_packet_40(3, 1, False, TAIL2, bytes([0x77, 0x88]), bytes.fromhex("3412"), CLIENT_IP))
    emit("official.logout.user8.pwd8",
         make_logout_packet(USER, PASSWORD, SALT, MAC, TAIL1))
    packet, _ = make_login_packet(USER, PASSWORD, SALT, CLIENT_IP, MAC, HOSTNAME,
                                  bytes([0x99, 0xAA]))
    emit("official.login.user8.pwd8", packet)
    packet16, _ = make_login_packet(USER, b"0123456789abcdef", SALT, CLIENT_IP, MAC,
                                    HOSTNAME, bytes([0x99, 0xAA]))
    emit("official.login.user8.pwd16", packet16)


if __name__ == "__main__":
    main()
