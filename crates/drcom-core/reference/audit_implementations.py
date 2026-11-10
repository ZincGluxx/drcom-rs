#!/usr/bin/env python3
# coding: utf-8
"""Cross-check the archived Dr.COM implementations against this port.

The vendored tree under `reference/jlu-drcom-client/` holds eight independent
implementations of the same campus protocol, written between 2014 and 2020.
They are not all the same protocol: three generations are present, and the login
frame differs between them in field values, in a whole trailing block, and in
length.  This script

  1. classifies every vendored implementation into a generation by reading the
     marker bytes out of its own source, so the claim is checkable rather than
     remembered;
  2. diffs the two generations this port can build, byte for byte, and prints the
     field each divergence falls in;
  3. reports which generation the Rust code follows, and whether `--check` still
     agrees.

Run:  python reference/audit_implementations.py
      python reference/audit_implementations.py --check

No socket is opened and no credential is read: the login frames are built from
fixed test values.
"""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# The archival tree is a separate clone because it carries binaries (jar, png,
# zip) that have no business inside the crate. Resolve it wherever it sits.
VENDOR_CANDIDATES = [
    os.path.join(HERE, "jlu-drcom-client"),
    os.path.normpath(os.path.join(HERE, "..", "..", "reference", "jlu-drcom-client")),
]
VENDOR = next((path for path in VENDOR_CANDIDATES if os.path.isdir(path)), None)
CLONE_HINT = "git clone --depth 1 https://github.com/ZincGluxx/jlu-drcom-client"

# Per-file probes: name -> list of patterns. Patterns are Latin-1 byte strings
# so a source file can be read without guessing its encoding; every marker here
# is ASCII, and the non-ASCII text in these files never carries a marker.
PROBES = {
    "auth version literal": [b"\\x68\\x00", b"\\x6a", b"0x6A", b"0x6a", b"\\x6e\\x00"],
    "adapters/control byte": [b"ADAPTERNUM = '\\x03'", b"adapterNum = 0x05", b"adapterNum=0x05",
                              b"data_4 = {0x00, 0x00}", b"'{0x03, 0x01}'"],
    "hostname field 32": [b"ljust(32", b"ljust(hostname, 32)", b"new byte [32]", b"new byte[32]"],
    "hostname field 71": [b"ljust(71", b"host_name[71]", b"delimiter_3[1]"],
    "DRCOM CHECK block": [b"\\x44\\x72\\x43\\x4f\\x4d", b"0x44, 0x72, 0x43, 0x4F"],
    "40-byte client key": [b"1c210c99585fd22ad03d35c956911aeec1eb449b",
                           b"3dc79f5212e8170acfa9ec95f1d74916542be7b1",
                           b"\\x33\\x64\\x63\\x37\\x39\\x66\\x35"],
    "trailing 60 a2": [b"\\x60\\xa2", b"\\x60\\xa2"],
    "trailing 6d/6e": [b"\\x6d\\x00\\x00", b"\\x6e, 0x00, 0x00"],
    "keep38 + keep40 cycle": [b"keep40_extra", b"keep_alive2", b"primary_keep_alive"],
    "keep40 checksum folded": [b"ByteUtil.crc", b"packet_CRC", b"packet_checksum1",
                               b"packet_crc", b"crc("],
}

GENERATIONS = {
    "A": "old client (DrCOM 5.2.0): hostname 71 + host_os 128, no DRCOM block",
    "B": "new client (JLU 5.2.1): DRCOM block, 40-byte key, 60 a2 tail",
    "C": "official capture (2017 rewrite): DRCOM block, shorter frame, folded checksum",
    "D": "early client (2015 Android): hostname 32, no DRCOM block, 6e 00 00 tail",
    "?": "unclassified",
}


def classify(path):
    """Guess the generation from markers present in the file itself."""
    try:
        raw = open(path, "rb").read()
    except OSError:
        return "?", {}

    hits = {}
    for label, patterns in PROBES.items():
        hit = [p.decode("latin-1") for p in patterns if p in raw]
        if hit:
            hits[label] = hit

    return generation_of(hits), hits


def generation_of(hits):
    """The generation a marker set belongs to, whichever file it came from."""
    if "hostname field 71" in hits:
        return "A"
    if "40-byte client key" in hits:
        # The two known key spellings travel with their own generation: every
        # Java/Android source captured from the official client uses the first,
        # and every hand-written client in the archive uses the second.
        key = " ".join(hits["40-byte client key"])
        return "C" if "1c210c99" in key else "B"
    if "DRCOM CHECK block" in hits:
        return "C"
    if "hostname field 32" in hits:
        # A 32-byte host name without the DRCOM block is the 2015 Android port,
        # which sends 164 zero bytes where the later clients send the block.
        return "D"
    return "?"


def sweep():
    print("=" * 78)
    print("1. Every archived implementation, classified from its own source")
    print("=" * 78)
    if VENDOR is None:
        print("SKIP the archive is not present at any of:")
        for path in VENDOR_CANDIDATES:
            print("       %s" % path)
        print("     fetch it with:  %s" % CLONE_HINT)
        return []
    print("archive: %s" % VENDOR)
    print()

    # Group by implementation, not by file: a login builder is spread over
    # several files in the Java and Android trees, and the support files carry
    # none of the markers on their own.
    groups = {}
    for root, dirs, files in os.walk(VENDOR):
        dirs[:] = [d for d in dirs if d != ".git"]
        for name in sorted(files):
            if not name.endswith((".py", ".c", ".java")):
                continue
            path = os.path.join(root, name)
            relative = os.path.relpath(path, VENDOR).replace(os.sep, "/")
            # A file at the archive root is an implementation on its own; the
            # tree-shaped ports keep their login builder spread over files, so
            # they are grouped by their top-level directory.
            group = relative.rsplit("/", 1)[0] if "/" in relative else relative
            marker_text = classify(path)[1]
            record = groups.setdefault(group, {"hits": {}, "files": 0})
            record["files"] += 1
            for label, found in marker_text.items():
                record["hits"].setdefault(label, []).extend(found)

    counts = {}
    for group in sorted(groups):
        record = groups[group]
        generation = generation_of(record["hits"])
        counts[generation] = counts.get(generation, 0) + 1
        print("%-2s %-52s %2d source files" % (generation, group, record["files"]))
        if record["hits"]:
            print("     %s" % ", ".join(sorted(record["hits"])))
    print()
    for generation in sorted(counts):
        print("  generation %s: %d implementations -- %s"
              % (generation, counts[generation], GENERATIONS[generation]))
    return groups


# The two generations this port can build, using fixed test credentials. The
# builders come from the generators that also produce the Rust golden vectors,
# so a divergence here is a divergence in what the Rust tests pin.
LOGIN_LAYOUT = [
    (0, 4, "magic 03 01 00 + length byte"),
    (4, 20, "md51"),
    (20, 56, "account field (36 bytes)"),
    (56, 58, "control status, adapter number"),
    (58, 64, "mac ^ md51[..6]"),
    (64, 80, "md52"),
    (80, 97, "address count, client address, three empty slots"),
    (97, 105, "md53"),
    (105, 110, "ip dog, four zero bytes"),
    (110, 142, "host name (32 bytes)"),
    (142, 162, "primary DNS, DHCP, secondary DNS, delimiter"),
    (162, 191, "OS fingerprint, DRCOM marker"),
    (191, 246, "zero run"),
    (246, 286, "40-byte client key"),
    (286, 310, "zero run"),
    (310, 314, "auth version, password length"),
    (None, None, "rotated password, 02 0c, checksum, mac, tail"),
]


def field_of(offset):
    for start, end, label in LOGIN_LAYOUT:
        if start is None:
            return label
        if start <= offset < end:
            return label
    return "beyond the documented layout"


def load_generator(name):
    """Import a generator by path without executing its __main__ block."""
    import importlib.util
    path = os.path.join(HERE, name)
    spec = importlib.util.spec_from_file_location(name[:-3], path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def diff_profiles():
    print()
    print("=" * 78)
    print("2. The two buildable generations, byte for byte")
    print("=" * 78)
    reference = load_generator("gen_vectors.py")
    official = load_generator("gen_official_vectors.py")

    salt = bytes.fromhex("1a2b3c4d")
    user = b"testuser"
    password = b"testpass"
    mac = 0x112288776655
    official_mac = bytes.fromhex("112288776655")
    client_ip = bytes([10, 0, 0, 9])
    hostname = b"TESTHOST"

    reference_frame = reference.mkpkt(salt, user, password, mac)
    official_frame, _ = official.make_login_packet(
        user, password, salt, client_ip, official_mac, hostname, bytes([0x99, 0xAA]))

    print("reference (Python client) login frame: %d bytes" % len(reference_frame))
    print("official  (2017 rewrite)  login frame: %d bytes" % len(official_frame))
    print("length difference: %+d" % (len(official_frame) - len(reference_frame)))
    print()

    common = min(len(reference_frame), len(official_frame))
    differing = [i for i in range(common) if reference_frame[i] != official_frame[i]]
    by_field = {}
    for offset in differing:
        by_field.setdefault(field_of(offset), []).append(offset)
    print("first %d bytes: %d differ" % (common, len(differing)))
    for label, offsets in sorted(by_field.items(), key=lambda kv: kv[1][0]):
        print("  %-46s offsets %s%s" % (
            label,
            ", ".join(str(o) for o in offsets[:6]),
            "" if len(offsets) <= 6 else " (+%d more)" % (len(offsets) - 6)))
    print()
    print("bytes present only in one frame: reference %d, official %d"
          % (len(reference_frame) - common, len(official_frame) - common))
    print()

    # The checksum divergence is an algorithm difference, not just a field.
    print("checksum algorithms:")
    print("  reference  re.findall(b'....', s) -- Python's '.' does not match")
    print("             newline 0x0a, so a window containing one is skipped and")
    print("             the scan shifts by one byte for the rest of the buffer.")
    print("  official   clean four-byte stride, documented in ByteUtil.java as")
    print("             the correct value, with the note that the Python client")
    print("             still works because the server does not verify it.")
    return reference_frame, official_frame


def main(argv):
    sweep()
    reference_frame, official_frame = diff_profiles()

    print()
    print("=" * 78)
    print("3. What this port follows")
    print("=" * 78)
    print("Rust builds the reference (generation B) frame by default and pins it")
    print("against reference/reference_vectors.txt; the generation C keep-alive")
    print("layout and checksum are pinned separately against")
    print("reference/official_vectors.txt.")

    if "--check" in argv:
        problems = []
        if len(reference_frame) != 368:
            problems.append("reference login frame is %d bytes, expected 368"
                            % len(reference_frame))
        if len(official_frame) != 338:
            problems.append("official login frame is %d bytes, expected 338"
                            % len(official_frame))
        # Below the key slot the generations differ at the adapter-number byte
        # and at the last byte of the DRCOM marker. The md53 field at 97..105
        # then differs as a consequence, because it digests the first 101 bytes
        # and therefore covers the adapter byte. Anything outside this set means
        # a generator changed rather than just a constant.
        expected_early = {57, 190} | set(range(97, 105))
        common = min(len(reference_frame), len(official_frame))
        early = {i for i in range(246)
                 if reference_frame[i] != official_frame[i]}
        if early != expected_early:
            problems.append("unexpected early divergence at %s (established: 57,"
                            " 190, and md53 at 97..104)"
                            % sorted(early - expected_early))
        if reference_frame[246:286] == official_frame[246:286]:
            problems.append("the two generations now share a client key, which"
                            " contradicts both captured sources")
        if common < 246:
            problems.append("the frames are too short to compare")
        if problems:
            print()
            for problem in problems:
                print("FAIL %s" % problem)
            return 1
        print()
        print("OK  both frames still match the pinned lengths and the shared prefix")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
