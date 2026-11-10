#!/usr/bin/env python
# coding: utf-8
"""Audit the C# port against the original Python client it was ported from.

`DrComCampus/Services/DrComAuthenticationService.cs` claims to be
"移植自 Python 版本". This script implements both login-frame builders with
identical inputs and reports every byte where they disagree, so the divergence
is a measurement rather than an opinion.

It also prints the client-key literal from each source file, because that is the
one field where a hand transcription error survives review: it is a 40-character
hex string with no delimiters.

Pure functions only: no socket, no credential read from disk. Run:

    python reference/audit_csharp_port.py
"""

import os
import struct
import sys
from hashlib import md5
import re

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

PY_CLIENT = os.path.join(
    os.path.expanduser('~'), 'Documents', 'Codex', '2026-07-13', 'ba',
    'newclinet-py3.py')
CSHARP = os.path.join(REPO, 'DrComCampus', 'Services',
                     'DrComAuthenticationService.cs')
GEN_VECTORS = os.path.join(HERE, 'gen_vectors.py')


# --------------------------------------------------------------------------
# 1. Prove the key length from the source files themselves.
# --------------------------------------------------------------------------
def extract_key(path):
    """Pull the 40-ish character hex client key out of a source file."""
    text = open(path, 'r', encoding='utf-8', errors='replace').read()
    # The Python client writes it as escaped byte literals, the C# as a string.
    escapes = re.findall(r"(?:\\x[0-9a-fA-F]{2}){20,}", text)
    if escapes:
        blob = escapes[0]
        return bytes.fromhex(''.join(
            part[2:] for part in re.findall(r'\\x[0-9a-fA-F]{2}', blob)))
    literals = re.findall(r'"[0-9a-f]{20,}"', text)
    if literals:
        return literals[0].strip('"').encode()
    raise SystemExit('no client key literal found in ' + path)


def report_keys():
    print('=== client key literals ===')
    for label, path in (('python(newclinet-py3.py)', PY_CLIENT),
                        ('python(gen_vectors.py)', GEN_VECTORS),
                        ('c#  (DrComAuthenticationService.cs)', CSHARP)):
        if not os.path.exists(path):
            print('%-34s MISSING %s' % (label, path))
            continue
        key = extract_key(path)
        print('%-34s len=%-3d odd=%-5s %s'
              % (label, len(key), bool(len(key) % 2), key.decode()))


# --------------------------------------------------------------------------
# 2. Both login-frame builders, parameterised identically.
# --------------------------------------------------------------------------
CONTROLCHECKSTATUS = b'\x20'
ADAPTERNUM = b'\x03'
IPDOG = b'\x01'
AUTH_VERSION = b'\x68\x00'
PRIMARY_DNS = '10.10.10.10'
DHCP = '0.0.0.0'

PY_KEY = (b'3dc79f5212e8170acfa9ec95f1d74916542be7b1')
CS_KEY = b'3dc79f5212e8170acfa9ec95f1d749916542be7b1'


def md5sum(s):
    return md5(s).digest()


def dump(n):
    s = '%x' % n
    if len(s) & 1:
        s = '0' + s
    return bytes.fromhex(s)


def ror(digest, pwd):
    return bytes((((digest[i] ^ pwd[i]) << 3 & 0xFF) + ((digest[i] ^ pwd[i]) >> 5))
                 for i in range(len(pwd)))


def checksum_python(s):
    """re.findall(b'....', s): '.' never matches 0x0a, so the scan shifts."""
    ret = 1234
    for i in re.findall(b'....', s):
        ret ^= int(i[::-1].hex(), 16)
    return struct.pack('<I', (1968 * ret) & 0xffffffff)


def checksum_csharp(s):
    """BinaryPrimitives.ReadUInt32LittleEndian in a flat four-byte stride."""
    ret = 1234
    for i in range(0, len(s) - 3, 4):
        ret ^= int.from_bytes(s[i:i + 4], 'little')
    return struct.pack('<I', (1968 * ret) & 0xffffffff)


def ip(s):
    return bytes(int(x) for x in s.split('.'))


def mkpkt_python(salt, usr, pwd, mac, host_ip, host_name, key=PY_KEY):
    data = b'\x03\x01\x00' + (len(usr) + 20).to_bytes(1, 'big')
    data += md5sum(b'\x03\x01' + salt + pwd)
    data += usr.ljust(36, b'\x00')
    data += CONTROLCHECKSTATUS + ADAPTERNUM
    data += dump(int(data[4:10].hex(), 16) ^ mac).rjust(6, b'\x00')
    data += md5sum(b'\x01' + pwd + salt + b'\x00' * 4)
    data += b'\x01' + ip(host_ip) + b'\x00' * 12
    data += md5sum(data + b'\x14\x00\x07\x0b')[:8]
    data += IPDOG + b'\x00' * 4
    data += host_name.ljust(32, b'\x00')
    data += ip(PRIMARY_DNS) + ip(DHCP) + b'\x00' * 4 + b'\x00' * 8
    data += (b'\x94\x00\x00\x00\x06\x00\x00\x00\x02\x00\x00\x00'
             b'\xf0\x23\x00\x00\x02\x00\x00\x00\x44\x72\x43\x4f\x4d\x00\xcf\x07\x68')
    data += b'\x00' * 55 + key + b'\x00' * 24
    data += AUTH_VERSION + b'\x00' + len(pwd).to_bytes(1, 'big')
    data += ror(md5sum(b'\x03\x01' + salt + pwd), pwd)
    data += b'\x02\x0c'
    data += checksum_python(data + b'\x01\x26\x07\x11\x00\x00' + dump(mac))
    data += b'\x00\x00' + dump(mac)
    if (len(pwd) / 4) != 4:
        data += b'\x00' * (len(pwd) // 4)
    data += b'\x60\xa2' + b'\x00' * 28
    return data


def mkpkt_csharp(salt, usr, pwd, mac, host_ip, host_name, key=CS_KEY):
    md51 = md5sum(b'\x03\x01' + salt + pwd)
    v = 0
    for j in range(6):
        v = (v << 8) | md51[j]
    xor = v ^ (mac & 0xFFFFFFFFFFFF)
    xor_result = bytes((xor >> (8 * (5 - j))) & 0xFF for j in range(6))

    data = bytearray(b'\x03\x01\x00' + (len(usr) + 20).to_bytes(1, 'big'))
    data += md51
    data += usr.ljust(36, b'\x00')
    data += CONTROLCHECKSTATUS + ADAPTERNUM
    data += xor_result
    data += md5sum(b'\x01' + pwd + salt + b'\x00' * 4)
    data += b'\x01' + ip(host_ip) + b'\x00' * 12
    data += md5sum(bytes(data) + b'\x14\x00\x07\x0b')[:8]
    data += IPDOG + b'\x00' * 4
    data += host_name.ljust(32, b'\x00')
    data += ip(PRIMARY_DNS) + ip(DHCP) + b'\x00' * 4 + b'\x00' * 8
    data += (b'\x94\x00\x00\x00\x06\x00\x00\x00\x02\x00\x00\x00'
             b'\xf0\x23\x00\x00\x02\x00\x00\x00\x44\x72\x43\x4f\x4d\x00\xcf\x07\x68')
    data += b'\x00' * 55 + key + b'\x00' * 24
    data += AUTH_VERSION + b'\x00' + len(pwd).to_bytes(1, 'big')
    data += ror(md51, pwd)
    data += b'\x02\x0c'
    check = bytes(data) + b'\x01\x26\x07\x11\x00\x00' + dump(mac)
    data += checksum_csharp(check)
    data += b'\x00\x00' + dump(mac)
    pwd_padding = len(pwd) // 4
    if pwd_padding > 0:
        data += b'\x00' * pwd_padding
    data += b'\x60\xa2' + b'\x00' * 28
    return bytes(data)


# --------------------------------------------------------------------------
# 3. Compare.
# --------------------------------------------------------------------------
SALT = bytes.fromhex('1a2b3c4d')
USER = b'testuser'
CASES = [
    ('pwd 8 bytes', b'testpass', 0x112288776655),
    ('pwd 16 bytes', b'0123456789abcdef', 0x112288776655),
    ('mac leading zero', b'testpass', 0x001122334455),
]


def diff(a, b):
    """Index ranges where two byte strings disagree (length-aware)."""
    spans = []
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else None
        y = b[i] if i < len(b) else None
        if x != y:
            if spans and spans[-1][1] == i:
                spans[-1][1] = i + 1
            else:
                spans.append([i, i + 1])
    return spans


def fmt(span, a, b):
    i, j = span
    return 'offset %3d..%3d  py=%s  c#=%s' % (
        i, j - 1,
        a[i:j].hex(' ') if i < len(a) else '<eof>',
        b[i:j].hex(' ') if i < len(b) else '<eof>')


def main():
    report_keys()
    print()
    print('=== login frame: original python vs C# port (same inputs) ===')
    for name, pwd, mac in CASES:
        py = mkpkt_python(SALT, USER, pwd, mac, '10.0.0.9', b'TESTHOST')
        cs = mkpkt_csharp(SALT, USER, pwd, mac, '10.0.0.9', b'TESTHOST')
        spans = diff(py, cs)
        print('%-18s len py=%-4d c#=%-4d delta=%+d  differing bytes=%d'
              % (name, len(py), len(cs), len(cs) - len(py),
                 sum(j - i for i, j in spans)))
        for span in spans[:6]:
            print('    ' + fmt(span, py, cs))
        if len(spans) > 6:
            print('    ... %d more spans' % (len(spans) - 6))

    print()
    print('=== isolating each divergence (python frame with one C# trait) ===')
    pwd = b'testpass'
    mac = 0x112288776655
    base = mkpkt_python(SALT, USER, pwd, mac, '10.0.0.9', b'TESTHOST')
    only_key = mkpkt_python(SALT, USER, pwd, mac, '10.0.0.9', b'TESTHOST',
                            key=CS_KEY)
    print('key 40 -> 41 chars:          len %d -> %d, first diff at %s'
          % (len(base), len(only_key),
             diff(base, only_key)[0][0] if diff(base, only_key) else 'none'))

    # checksum trait on its own: same frame, flat stride instead of skipping.
    check_input = base[:324] + b'\x01\x26\x07\x11\x00\x00' + dump(mac)
    print('checksum grouping:           py=%s  flat-stride=%s'
          % (checksum_python(check_input).hex(),
             checksum_csharp(check_input).hex()))

    pwd16 = b'0123456789abcdef'
    py16 = mkpkt_python(SALT, USER, pwd16, mac, '10.0.0.9', b'TESTHOST')
    cs16 = mkpkt_csharp(SALT, USER, pwd16, mac, '10.0.0.9', b'TESTHOST')
    print('16-byte pwd padding:         len py=%d c#=%d (c# adds %d extra '
          'zero bytes before the 60 a2 trailer)'
          % (len(py16), len(cs16), len(cs16) - len(py16) - 1 + 4 - 4))


if __name__ == '__main__':
    main()
