#!/usr/bin/env python
# coding: utf-8
"""Golden wire vectors for the Rust Dr.COM port.

The functions below are copied verbatim from the widely circulated third-party
Python client that the C# `DrComCampus` client was ported from (see
`DrComCampus/Services/DrComAuthenticationService.cs`, which reproduces the same
byte layout). They are pure functions: no socket is opened and no credential is
read from disk.

Run this script to regenerate `reference_vectors.txt`; the Rust unit tests
assert byte-for-byte equality against that file:

    python reference/gen_vectors.py > reference/reference_vectors.txt

Keeping the generator in-tree means the Rust port can always be re-checked
against the reference implementation instead of against hand-written
expectations.
"""

import re
import struct
import random
from hashlib import md5

# ---- config of the reference client, pinned to deterministic test values ----
CONTROLCHECKSTATUS = b'\x20'
ADAPTERNUM = b'\x03'
IPDOG = b'\x01'
PRIMARY_DNS = '10.10.10.10'
dhcp_server = '0.0.0.0'
AUTH_VERSION = b'\x68\x00'
KEEP_ALIVE_VERSION = b'\xdc\x02'

HOST_IP = '10.0.0.9'
MAC = 0x112288776655
MAC_LEADING_ZERO = 0x001122334455
HOST_NAME = b'TESTHOST'


def md5sum(s):
    m = md5()
    m.update(s)
    return m.digest()


def dump(n):
    s = '%x' % n
    if len(s) & 1:
        s = '0' + s
    return bytes.fromhex(s)


def ror(md5: bytes, pwd: bytes):
    ret = b''
    for i in range(len(pwd)):
        x = md5[i] ^ pwd[i]
        ret += (((x << 3) & 0xFF) + (x >> 5)).to_bytes(1, 'big')
    return ret


def checksum(s):
    ret = 1234
    for i in re.findall(b'....', s):
        ret ^= int(i[::-1].hex(), 16)
    ret = (1968 * ret) & 0xffffffff
    return struct.pack('<I', ret)


def mkpkt(salt, usr, pwd, mac):
    data = b'\x03\x01\x00' + (len(usr) + 20).to_bytes(1, 'big')
    data += md5sum(b'\x03\x01' + salt + pwd)
    data += usr.ljust(36, b'\x00')
    data += CONTROLCHECKSTATUS
    data += ADAPTERNUM
    data += dump(int(data[4:10].hex(), 16) ^
                 mac).rjust(6, b'\x00')  # mac xor md51
    data += md5sum(b"\x01" + pwd + salt + b'\x00' * 4)  # md52
    data += b'\x01'  # number of ip
    data += b''.join([int(x).to_bytes(1, 'big') for x in HOST_IP.split('.')])
    data += b'\x00' * 4  # your ipaddress 2
    data += b'\x00' * 4  # your ipaddress 3
    data += b'\x00' * 4  # your ipaddress 4
    data += md5sum(data + b'\x14\x00\x07\x0b')[:8]  # md53
    data += IPDOG
    data += b'\x00' * 4  # delimeter
    data += HOST_NAME.ljust(32, b'\x00')
    data += b''.join([int(i).to_bytes(1, 'big') for i in PRIMARY_DNS.split('.')])  # primary dns
    data += b''.join([int(i).to_bytes(1, 'big') for i in dhcp_server.split('.')])  # DHCP dns
    data += b'\x00\x00\x00\x00'  # secondary dns:0.0.0.0
    data += b'\x00' * 8  # delimeter
    data += b'\x94\x00\x00\x00'  # unknow
    data += b'\x06\x00\x00\x00'  # os major
    data += b'\x02\x00\x00\x00'  # os minor
    data += b'\xf0\x23\x00\x00'  # OS build
    data += b'\x02\x00\x00\x00'  # os unknown
    data += b'\x44\x72\x43\x4f\x4d\x00\xcf\x07\x68'
    data += b'\x00' * 55  # unknown string
    data += b'\x33\x64\x63\x37\x39\x66\x35\x32\x31\x32\x65\x38\x31\x37\x30\x61\x63\x66\x61\x39\x65\x63\x39\x35\x66\x31\x64\x37\x34\x39\x31\x36\x35\x34\x32\x62\x65\x37\x62\x31'
    data += b'\x00' * 24
    data += AUTH_VERSION
    data += b'\x00' + len(pwd).to_bytes(1, 'big')
    data += ror(md5sum(b'\x03\x01' + salt + pwd), pwd)
    data += b'\x02\x0c'
    data += checksum(data + b'\x01\x26\x07\x11\x00\x00' + dump(mac))
    data += b'\x00\x00'  # delimeter
    data += dump(mac)
    if (len(pwd) / 4) != 4:
        data += b'\x00' * (len(pwd) // 4)  # strange。。。
    data += b'\x60\xa2'  # unknown, filled numbers randomly =w=
    data += b'\x00' * 28
    return data


def challenge_packet(ran):
    return b"\x01\x02" + struct.pack("<H", int(ran) % (0xFFFF)) + b"\x09" + b"\x00" * 15


def keep_alive_package_builder(number, random_, tail, type=1, first=False):
    data = b'\x07' + number.to_bytes(1, 'big') + b'\x28\x00\x0b' + type.to_bytes(1, 'big')
    if first:
        data += b'\x0f\x27'
    else:
        data += KEEP_ALIVE_VERSION
    data += b'\x2f\x12' + b'\x00' * 6
    data += tail
    data += b'\x00' * 4
    if type == 3:
        foo = b''.join([int(i).to_bytes(1, 'big') for i in HOST_IP.split('.')])  # host_ip
        # CRC
        # edited on 2014/5/12, filled zeros to checksum
        # crc = packet_CRC(data+foo)
        crc = b'\x00' * 4
        # data += struct.pack("!I",crc) + foo + b'\x00' * 8
        data += crc + foo + b'\x00' * 8
    else:  # packet type = 1
        data += b'\x00' * 16
    return data


def primary_keep_alive(salt, tail, pwd, unix_seconds):
    foo = struct.pack('!H', int(unix_seconds) % 0xFFFF)
    data = b'\xff' + md5sum(b'\x03\x01' + salt + pwd) + b'\x00\x00\x00'
    data += tail
    data += foo + b'\x00\x00\x00\x00'
    return data


SALT = bytes.fromhex('1a2b3c4d')
USER = b'testuser'
# A full-width account: the reference stores it in a fixed 36-byte field, so
# this is the longest name that does not spill into the control-status byte.
USER_36 = b'user' + b'a' * 32
PASSWORD = b'testpass'
PASSWORD_16 = b'0123456789abcdef'
# Twelve bytes is three four-byte groups, so the padding rule writes three
# zero bytes where an eight-byte password writes two and a sixteen-byte one
# writes none. Having all three lengths pinned keeps that branch honest.
PASSWORD_12 = b'0123456789ab'
TAIL = bytes.fromhex('a1b2c3d4')
# A MAC whose `dump()` is shorter than six bytes once the leading zeros drop.
MAC_SMALL = 0x00000abc
# The login response cookie the reference forwards in every primary keep-alive.
COOKIE = bytes.fromhex('101112131415161718191a1b1c1d1e1f')


def emit(name, value):
    print('%s = %s' % (name, value.hex()))


def main():
    emit('challenge.ran_12345678', challenge_packet(0x12345678))
    emit('login.user8.pwd8', mkpkt(SALT, USER, PASSWORD, MAC))
    emit('login.user8.pwd16', mkpkt(SALT, USER, PASSWORD_16, MAC))
    emit('login.user36.pwd12', mkpkt(SALT, USER_36, PASSWORD_12, MAC))
    emit('login.mac_leading_zero', mkpkt(SALT, USER, PASSWORD, MAC_LEADING_ZERO))
    emit('login.mac_small', mkpkt(SALT, USER, PASSWORD, MAC_SMALL))
    emit('keepalive.primary', primary_keep_alive(SALT, COOKIE, PASSWORD, 0x5F1E2A3B))
    emit('keepalive.type1.first',
         keep_alive_package_builder(0, dump(1), b'\x00' * 4, 1, True))
    emit('keepalive.type1.next',
         keep_alive_package_builder(7, dump(1), TAIL, 1, False))
    emit('keepalive.type3.next',
         keep_alive_package_builder(8, dump(1), TAIL, 3, False))


if __name__ == '__main__':
    main()
