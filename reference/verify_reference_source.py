#!/usr/bin/env python
# coding: utf-8
"""Prove that the in-tree golden-vector generator is a faithful copy.

`reference/gen_vectors.py` carries its own transcription of the login frame,
challenge and keep-alive builders so that `cargo test` can regenerate the
expected bytes without depending on a file outside the repository. That is the
right call for reproducible builds, but it makes the whole "the Rust port
matches the reference byte for byte" claim rest on a hand transcription.

This script removes that assumption. It loads the *authoritative* original
client -- `~/Documents/Codex/2026-07-13/ba/newclinet-py3.py`, the script that
authenticates on this deployment -- extracts its functions with `ast` (the file
binds UDP 61440 at import time, so it can never be imported directly), and runs
it head to head against the in-tree copy over randomised inputs.

The socket the original talks to is replaced by a recorder, so nothing is sent
and no credential is read. Run:

    python reference/verify_reference_source.py
"""

import ast
import importlib.util
import os
import random
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
AUTHORITATIVE = os.path.join(
    os.path.expanduser('~'), 'Documents', 'Codex', '2026-07-13', 'ba',
    'newclinet-py3.py')
IN_TREE = os.path.join(HERE, 'gen_vectors.py')

# Functions we can run safely. `login`/`main`/`keep_alive2` drive a real
# session, so they stay out; everything else is a pure builder or a
# request/response round trip we can answer with a recorder.
WANTED_FUNCTIONS = (
    'md5sum', 'dump', 'ror', 'checksum', 'mkpkt',
    'keep_alive_package_builder', 'keep_alive1', 'challenge', 'log',
)

HOST_IP = '10.0.0.9'
HOST_NAME = b'TESTHOST'
PRIMARY_DNS = '10.10.10.10'
DHCP = '0.0.0.0'
SERVER = '10.100.61.3'
SERVER_PORT = 61440


class Recorder:
    """Stand-in for the reference client's module-level socket `s`."""

    def __init__(self, reply=b'\x07' + b'\x00' * 19):
        self.sent = []
        self.reply = reply

    def sendto(self, data, address):
        self.sent.append((data, address))

    def recvfrom(self, _size):
        return self.reply, (SERVER, SERVER_PORT)

    def settimeout(self, _value):
        pass


def load_authoritative(path):
    """Exec only the constants and builders of the original client.

    Module-level `Expr`/`If` statements and any assignment whose value needs the
    `socket` module are skipped, which is what keeps `s = socket.socket(...)`
    and the following `s.bind(...)` from ever running.
    """
    tree = ast.parse(open(path, 'r', encoding='utf-8').read())
    namespace = {
        're': __import__('re'),
        'struct': __import__('struct'),
        'random': __import__('random'),
        'time': __import__('time'),
        'md5': __import__('hashlib').md5,
        'platform': __import__('platform'),
    }
    kept = []
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 \
                and isinstance(node.targets[0], ast.Name):
            try:
                namespace[node.targets[0].id] = ast.literal_eval(node.value)
            except ValueError:
                continue
        elif isinstance(node, ast.FunctionDef) and node.name in WANTED_FUNCTIONS:
            module = ast.Module(body=[node], type_ignores=[])
            exec(compile(ast.fix_missing_locations(module), path, 'exec'),
                 namespace)
            kept.append(node.name)
    return namespace, kept


def load_in_tree(path):
    spec = importlib.util.spec_from_file_location('gen_vectors', path)
    module = importlib.util.module_from_spec(spec)
    sys.modules['gen_vectors'] = module
    spec.loader.exec_module(module)
    return module


def align(namespace):
    """Pin the deployment constants and mute the reference's own logging.

    `log()` prints every packet the original builds; this check only needs the
    bytes the builders return, so it is replaced with a sink.
    """
    namespace.update({
        'host_ip': HOST_IP,
        'host_name': HOST_NAME,
        'PRIMARY_DNS': PRIMARY_DNS,
        'dhcp_server': DHCP,
        'DEBUG': False,
        's': Recorder(),
        'log': lambda *args, **kwargs: None,
    })


def random_cases(count, seed=20260915):
    rng = random.Random(seed)
    cases = []
    for _ in range(count):
        usr_len = rng.randint(1, 36)
        pwd_len = rng.randint(1, 16)
        # 0..3 leading zero bytes exercises `dump()`'s shortest-big-endian rule
        # and the `.rjust(6, b'\x00')` that follows it.
        lead = rng.randint(0, 3)
        mac = rng.randrange(1 << (8 * (6 - lead)))
        salt = bytes(rng.randrange(256) for _ in range(4))
        cases.append((
            salt,
            bytes(rng.randrange(ord('a'), ord('z') + 1) for _ in range(usr_len)),
            bytes(rng.randrange(ord('0'), ord('9') + 1) for _ in range(pwd_len)),
            mac,
        ))
    return cases


def first_divergence(a, b):
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else None
        y = b[i] if i < len(b) else None
        if x != y:
            return i
    return None


def compare(label, left, right, cases):
    """Report the first case where two builders disagree on a byte."""
    bad, first = 0, None
    for case in cases:
        a, b = left(*case), right(*case)
        if a != b:
            bad += 1
            if first is None:
                offset = first_divergence(a, b)
                first = (case, offset,
                         a[offset:offset + 8].hex(' '),
                         b[offset:offset + 8].hex(' '))
    print('%-42s %s  (%d cases, %d differing)'
          % (label, 'PASS' if bad == 0 else 'FAIL', len(cases), bad))
    if first:
        case, offset, x, y = first
        print('    first divergence at offset %d: salt=%s usr=%d pwd=%d mac=0x%x'
              % (offset, case[0].hex(), len(case[1]), len(case[2]), case[3]))
        print('    authoritative=%s  in-tree=%s' % (x, y))
    return bad


def check_primary_keep_alive(source, tree, salt, pwd, tail):
    """keep_alive1 builds the 0xff frame inline; check the wire bytes."""
    recorder = Recorder()
    source['s'] = recorder
    source['keep_alive1'](salt, tail, pwd, SERVER)
    wire, address = recorder.sent[0]
    digest = source['md5sum'](b'\x03\x01' + salt + pwd)
    nonce = wire[36:38]
    problems = []
    if len(wire) != 42:
        problems.append('length %d != 42' % len(wire))
    if wire[0] != 0xff:
        problems.append('prefix 0x%02x != 0xff' % wire[0])
    if wire[1:17] != digest:
        problems.append('md51 mismatch')
    if wire[17:20] != b'\x00' * 3:
        problems.append('separator mismatch')
    if wire[20:36] != tail:
        problems.append('session tail moved')
    if wire[38:42] != b'\x00' * 4:
        problems.append('trailer mismatch')
    if address != (SERVER, SERVER_PORT):
        problems.append('sent to %r' % (address,))
    # The nonce is the current Unix second modulo 0xffff, big endian.
    now = int(time.time()) % 0xFFFF
    got = int.from_bytes(nonce, 'big')
    if abs(got - now) > 2 and not (now < 3 and got > 0xFFFC):
        problems.append('nonce %d not within 2s of %d' % (got, now))
    # The same digest must be what the in-tree copy produces.
    if tree.md5sum(b'\x03\x01' + salt + pwd) != digest:
        problems.append('in-tree md5sum disagrees')
    print('%-42s %s  (%s)'
          % ('keep_alive1 (primary keep-alive frame)',
             'PASS' if not problems else 'FAIL',
             'len=%d, nonce=%s' % (len(wire), nonce.hex())))
    for problem in problems:
        print('    %s' % problem)
    return len(problems)


def check_challenge(source, tree):
    """challenge() sends a 20-byte request and returns bytes 4..8 of the reply."""
    salt = bytes.fromhex('1a2b3c4d')
    reply = bytearray(0x2c)
    reply[0] = 0x02
    reply[2:4] = (0x1234).to_bytes(2, 'little')
    reply[4:8] = salt
    recorder = Recorder(reply=bytes(reply))
    source['s'] = recorder
    got = source['challenge'](SERVER, 0x12345678)
    wire, _address = recorder.sent[0]
    expected = tree.challenge_packet(0x12345678)
    problems = []
    if wire != expected:
        problems.append('request %s != in-tree %s'
                        % (wire.hex(), expected.hex()))
    if got != salt:
        problems.append('returned salt %s' % got.hex())
    print('%-42s %s  (%d bytes sent, salt echoed)'
          % ('challenge (20-byte request + salt)',
             'PASS' if not problems else 'FAIL', len(wire)))
    for problem in problems:
        print('    %s' % problem)
    return len(problems)


def main():
    if not os.path.exists(AUTHORITATIVE):
        print('authoritative client not found: %s' % AUTHORITATIVE)
        print('this check needs the original script; skipping')
        return 0

    source, kept = load_authoritative(AUTHORITATIVE)
    align(source)
    tree = load_in_tree(IN_TREE)
    print('authoritative: %s' % AUTHORITATIVE)
    print('loaded functions: %s' % ', '.join(kept))
    print()

    cases = random_cases(200)
    failures = 0

    failures += compare('mkpkt (login frame)', source['mkpkt'], tree.mkpkt,
                        cases)
    failures += compare('dump', source['dump'], tree.dump,
                        [(n,) for n in (0, 1, 0xff, 0x100, 0x112288776655,
                                        0x001122334455, 0x7fffffffffff)]
                        + [(case[3],) for case in cases])
    failures += compare('ror', source['ror'], tree.ror,
                        [(bytes(range(16)), case[2]) for case in cases])
    failures += compare('checksum', source['checksum'], tree.checksum,
                        [(bytes(range(64 + i)),) for i in range(8)])

    ka_cases = [
        (0, b'\x00', b'\x00' * 4, 1, True),
        (7, b'\x00', bytes.fromhex('a1b2c3d4'), 1, False),
        (8, b'\x00', bytes.fromhex('a1b2c3d4'), 3, False),
        (255, b'\x00', b'\x00' * 4, 3, False),
    ]
    failures += compare('keep_alive_package_builder',
                        source['keep_alive_package_builder'],
                        tree.keep_alive_package_builder, ka_cases)

    print()
    # The call site passes the 16-byte session cookie the login response
    # returned (`package_tail = data[23:39]`), not a 4-byte keep-alive tail.
    failures += check_primary_keep_alive(
        source, tree, bytes.fromhex('1a2b3c4d'), b'testpass', bytes(range(0x10, 0x20)))
    failures += check_challenge(source, tree)

    print()
    if failures:
        print('%d check(s) FAILED -- reference/gen_vectors.py is not a faithful '
              'copy of the authoritative client' % failures)
        return 1
    print('all checks passed: reference/gen_vectors.py is a faithful copy of the '
          'authoritative client')
    return 0


if __name__ == '__main__':
    sys.exit(main())
