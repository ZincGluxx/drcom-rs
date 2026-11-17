"""Portable unit tests: fake executable headers, real archives, no native tools needed."""
from pathlib import Path
import plistlib
import struct
import tempfile
import unittest
import zipfile
from unittest.mock import patch

import package


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binaries = self.root / "binaries"
        self.binaries.mkdir()

    def fixture(self, target):
        if "windows" in target:
            data = bytearray(128)
            data[:2] = b"MZ"
            struct.pack_into("<I", data, 60, 64)
            data[64:70] = b"PE\0\0\x64\x86"
            extension = ".exe"
        elif "linux" in target:
            data = bytearray(64)
            data[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", data, 18, 62)
            extension = ""
        else:
            data = bytearray(64)
            data[:4] = b"\xcf\xfa\xed\xfe"
            struct.pack_into("<I", data, 4, 0x0100000C if target.startswith("aarch64") else 0x01000007)
            extension = ""
        for binary in package.BINS:
            (self.binaries / (binary + extension)).write_bytes(data)

    def test_workspace_version_and_tag_guard(self):
        value = package.version()
        self.assertEqual(package.version(tag="v" + value), value)
        with self.assertRaisesRegex(ValueError, "does not match"):
            package.version(tag="v999.0.0")
        with self.assertRaises(ValueError):
            package.version(tag=value)

    def test_every_native_format_and_wrong_architecture(self):
        for target in package.TARGETS:
            self.fixture(target)
            filename = "drcom-campus" + (".exe" if "windows" in target else "")
            package.validate_binary(self.binaries / filename, target)
        self.fixture("x86_64-unknown-linux-gnu")
        with self.assertRaises(ValueError):
            package.validate_binary(self.binaries / "drcom-campus", "aarch64-apple-darwin")

    def test_linux_archive_contains_three_binaries_legal_and_legacy_layout(self):
        target = "x86_64-unknown-linux-gnu"
        self.fixture(target)
        stage = package.linux_stage(self.root / "stage", self.binaries, "1.2.3", target, "abc")
        self.assertEqual(stage.name, "drcom-campus-linux-x86_64-v1.2.3")
        for name in (*package.BINS, *package.LEGAL, "install.sh", "drcom-campus.desktop",
                     "share/icons/hicolor/128x128/apps/drcom-campus.png"):
            self.assertTrue((stage / name).is_file(), name)
        archive = self.root / "test.tar.gz"
        package.tar_tree(stage, archive)
        with package.tarfile.open(archive) as contents:
            for binary in package.BINS:
                item = contents.getmember(stage.name + "/" + binary)
                self.assertEqual(item.mode & 0o777, 0o755)
            self.assertFalse(any(item.issym() or item.islnk() for item in contents.getmembers()))

    def test_macos_bundle_relative_executable_and_legal(self):
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            self.fixture(target)
            app = package.macos_stage(self.root / target, self.binaries, "1.2.3-beta.1", target, "abc")
            with (app / "Contents/Info.plist").open("rb") as stream:
                info = plistlib.load(stream)
            self.assertEqual(info["CFBundleExecutable"], "drcom-campus")
            self.assertEqual(info["CFBundleShortVersionString"], "1.2.3")
            for binary in package.BINS:
                self.assertTrue((app / "Contents/MacOS" / binary).is_file())
            for name in package.LEGAL:
                self.assertTrue((app / "Contents/Resources" / name).is_file())

    def test_windows_zip_and_installer_defines_with_spaces(self):
        target = "x86_64-pc-windows-msvc"
        self.fixture(target)
        out = self.root / "output with spaces"
        out.mkdir()
        stage = self.root / "stage with spaces"
        def compiler(args, **kwargs):
            name = next(a.removeprefix("/DOutputName=") for a in args if a.startswith("/DOutputName="))
            (out / (name + ".exe")).write_bytes(b"installer fixture")
        with patch.object(package, "run", side_effect=compiler) as call:
            assets = package.windows_package(stage, out, self.binaries, "1.2.3-beta.1", target, "abc", "ISCC.exe")
        self.assertIn("/DMyAppVersion=1.2.3-beta.1", call.call_args.args[0])
        self.assertIn("/DMyAppFileVersion=1.2.3", call.call_args.args[0])
        with zipfile.ZipFile(assets[0]) as archive:
            names = {Path(name).name for name in archive.namelist()}
            self.assertTrue(set(package.LEGAL) <= names)
            self.assertTrue({name + ".exe" for name in package.BINS} <= names)

    def test_exact_release_set_and_tampering(self):
        out = self.root / "assets"
        out.mkdir()
        ver = "1.2.3"
        assets = []
        for name in package.expected_assets(ver):
            path = out / name
            path.write_bytes(name.encode())
            assets.append(path)
        self.assertEqual(len(assets), 8)
        package.write_checksums(assets, out)
        package.verify_release(out, ver)
        self.assertEqual(len((out / "SHA256SUMS").read_text().splitlines()), 8)
        extra = out / "unexpected.sha256"
        extra.write_text("untrusted metadata")
        with self.assertRaisesRegex(ValueError, "unexpected"):
            package.verify_release(out, ver)
        extra.unlink()
        assets[0].write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            package.verify_release(out, ver)
        assets[0].unlink()
        with self.assertRaisesRegex(ValueError, "Missing"):
            package.verify_release(out, ver)

    def test_deb_staging_includes_daemon_runtime_deps_and_no_services(self):
        target = "x86_64-unknown-linux-gnu"
        self.fixture(target)
        stage = self.root / "stage"
        out = self.root / "out"
        stage.mkdir()
        out.mkdir()
        def native_tool(args, **kwargs):
            if args[0] == "dpkg-shlibdeps":
                return package.subprocess.CompletedProcess(args, 0, "shlibs:Depends=libc6 (>= 2.34), libgcc-s1 (>= 3.0)\n")
            if args[0] == "dpkg-deb" and "--build" in args:
                Path(args[-1]).write_bytes(b"deb fixture")
            return package.subprocess.CompletedProcess(args, 0)
        with patch.object(package, "run", side_effect=native_tool):
            assets = package.linux_package(stage, out, self.binaries, "1.2.3-beta.1", target, "abc")
        self.assertEqual(assets[1].name, "drcom-campus_1.2.3~beta.1_amd64.deb")
        control = (stage / "deb/DEBIAN/control").read_text()
        self.assertIn("libc6 (>= 2.34)", control)
        self.assertIn("libdbus-1-3", control)
        self.assertIn("gnome-keyring | keepassxc", control)
        self.assertIn("libcap2-bin", control)
        for binary in package.BINS:
            self.assertTrue((stage / "deb/usr/bin" / binary).is_file())
        for name in package.LEGAL:
            self.assertTrue((stage / "deb/usr/share/doc/drcom-campus" / name).is_file())
        self.assertEqual([p.name for p in (stage / "deb/DEBIAN").iterdir()], ["control"])

    def test_missing_binary_fails(self):
        with self.assertRaises(FileNotFoundError):
            package.stage_binaries(self.root / "stage", self.binaries, "x86_64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
