#!/usr/bin/env python3
"""Dependency-free release staging; Python >= 3.11. See packaging/BUILDING.md."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
BINS = ("drcom-campus", "drcom-cli", "drcom-daemon")
LEGAL = ("LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.md")
TARGETS = {
    "x86_64-pc-windows-msvc": "windows-x86_64",
    "aarch64-apple-darwin": "macos-aarch64",
    "x86_64-apple-darwin": "macos-x86_64",
    "x86_64-unknown-linux-gnu": "linux-x86_64",
}
SEMVER = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?\Z")


def version(root: Path = ROOT, tag: str | None = None) -> str:
    value = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not SEMVER.fullmatch(value):
        raise ValueError(f"Invalid workspace version: {value!r}")
    if tag is not None and tag != f"v{value}":
        raise ValueError(f"Tag {tag!r} does not match workspace version v{value}")
    return value


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+", " ".join(map(str, args)), flush=True)
    return subprocess.run([str(x) for x in args], check=True, **kwargs)


def validate_binary(path: Path, target: str) -> None:
    """Reject missing binaries, wrong format and wrong architecture before publishing."""
    with path.open("rb") as stream:
        header = stream.read(64)
        if "windows" in target:
            if header[:2] != b"MZ" or len(header) < 64:
                raise ValueError(f"Not a PE binary: {path}")
            stream.seek(struct.unpack_from("<I", header, 60)[0])
            pe = stream.read(6)
            valid = pe == b"PE\0\0\x64\x86"
        elif "linux" in target:
            valid = (header[:6] == b"\x7fELF\x02\x01" and len(header) >= 20
                     and struct.unpack_from("<H", header, 18)[0] == 62)
        else:
            cpu = 0x0100000C if target.startswith("aarch64") else 0x01000007
            valid = (header[:4] == b"\xcf\xfa\xed\xfe" and len(header) >= 8
                     and struct.unpack_from("<I", header, 4)[0] == cpu)
    if not valid:
        raise ValueError(f"Binary format/architecture mismatch for {target}: {path}")


def copy(source: Path, destination: Path, executable: bool = False) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")
    path.chmod(0o644)


def stage_common(destination: Path, root: Path, release_version: str, target: str, revision: str) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    for name in (*LEGAL, "drcom.ini.example"):
        copy(root / name, destination / name)
    write_text(destination / "BUILD-INFO.json", json.dumps({
        "version": release_version, "target": target, "revision": revision,
        "source": f"https://github.com/ZincGluxx/drcom-rs/tree/{revision or 'v' + release_version}",
        "macos_signing": "ad-hoc; NOT notarized" if "apple" in target else None,
    }, indent=2) + "\n")
    copy(root / "packaging" / "RELEASE-NOTES.txt", destination / "RELEASE-NOTES.txt")


def stage_binaries(destination: Path, binary_dir: Path, target: str) -> None:
    for name in BINS:
        filename = name + (".exe" if "windows" in target else "")
        source = binary_dir / filename
        validate_binary(source, target)
        copy(source, destination / filename, executable=True)


def zip_tree(directory: Path, output: Path) -> None:
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(directory.rglob("*")):
            if path.is_file():
                archive.write(path, path.relative_to(directory.parent).as_posix())


def tar_tree(directory: Path, output: Path) -> None:
    def normalize(info: tarfile.TarInfo) -> tarfile.TarInfo:
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        # Windows chmod cannot set POSIX execute bits; normalize archive modes
        # explicitly so archive fixtures and cross-platform staging stay correct.
        executable = Path(info.name).name in (*BINS, "install.sh")
        info.mode = 0o755 if info.isdir() or executable else 0o644
        return info
    with tarfile.open(output, "w:gz") as archive:
        archive.add(directory, arcname=directory.name, filter=normalize)


def checksum(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_checksums(assets: list[Path], output: Path) -> None:
    for asset in assets:
        write_text(Path(str(asset) + ".sha256"), f"{checksum(asset)}  {asset.name}\n")
    write_text(output / "SHA256SUMS", "".join(f"{checksum(p)}  {p.name}\n" for p in sorted(assets)))


def windows_package(stage: Path, out: Path, binary_dir: Path, ver: str, target: str,
                    revision: str, iscc: str | None, root: Path = ROOT) -> list[Path]:
    stem = f"drcom-campus-{TARGETS[target]}-v{ver}"
    portable = stage / stem
    stage_common(portable, root, ver, target, revision)
    stage_binaries(portable, binary_dir, target)
    zip_path = out / f"{stem}-portable.zip"
    zip_tree(portable, zip_path)
    compiler = iscc or shutil.which("ISCC.exe") or shutil.which("iscc")
    if not compiler:
        candidate = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Inno Setup 6/ISCC.exe"
        if candidate.is_file():
            compiler = str(candidate)
    if not compiler:
        raise RuntimeError("Inno Setup 6 ISCC.exe is required; install Inno Setup or pass --iscc")
    numeric = ".".join(SEMVER.fullmatch(ver).groups()[:3])
    filename = stem + "-setup"
    run([compiler, "/Qp", f"/DMyAppVersion={ver}", f"/DMyAppFileVersion={numeric}",
         f"/DBinaryDir={portable}", f"/DPackageDir={portable}", f"/DOutputPath={out}",
         f"/DOutputName={filename}", str(root / "setup.iss")])
    installer = out / (filename + ".exe")
    if not installer.is_file():
        raise RuntimeError(f"ISCC did not create {installer}")
    return [zip_path, installer]


def macos_stage(stage: Path, binary_dir: Path, ver: str, target: str, revision: str,
                root: Path = ROOT) -> Path:
    app = stage / "DrCom Campus.app"
    contents = app / "Contents"
    stage_binaries(contents / "MacOS", binary_dir, target)
    stage_common(contents / "Resources", root, ver, target, revision)
    info = {
        "CFBundleName": "DrCom Campus", "CFBundleDisplayName": "DrCom 校园网助手",
        "CFBundleIdentifier": "io.github.zincgluxx.drcom-campus", "CFBundlePackageType": "APPL",
        "CFBundleExecutable": "drcom-campus", "CFBundleIconFile": "AppIcon.icns",
        "CFBundleShortVersionString": ver.split("-")[0].split("+")[0],
        "CFBundleVersion": ver.split("-")[0].split("+")[0], "LSMinimumSystemVersion": "11.0",
        "NSHighResolutionCapable": True, "NSPrincipalClass": "NSApplication",
        "NSHumanReadableCopyright": "Copyright (C) 2026 ZincGlux. AGPL-3.0-or-later.",
    }
    with (contents / "Info.plist").open("wb") as stream:
        plistlib.dump(info, stream)
    write_text(contents / "PkgInfo", "APPL????")
    return app


def macos_package(stage: Path, out: Path, binary_dir: Path, ver: str, target: str,
                  revision: str, root: Path = ROOT) -> list[Path]:
    stem = f"drcom-campus-{TARGETS[target]}-v{ver}"
    volume = stage / stem
    app = macos_stage(volume, binary_dir, ver, target, revision, root)
    iconset = stage / "AppIcon.iconset"
    iconset.mkdir()
    # Generate a valid native icon from the existing brand PNG; no Pillow/npm dependency.
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            suffix = "@2x" if scale == 2 else ""
            run(["sips", "-z", str(size * scale), str(size * scale),
                 str(root / "crates/drcom-gui/assets/icon-128.png"), "--out",
                 str(iconset / f"icon_{size}x{size}{suffix}.png")], stdout=subprocess.DEVNULL)
    run(["iconutil", "-c", "icns", str(iconset), "-o", str(app / "Contents/Resources/AppIcon.icns")])
    # No Developer ID, keychain import or secrets: explicitly ad-hoc, never notarized.
    # Sign nested helper executables before the app's main executable. codesign
    # recognizes CFBundleExecutable's path as the containing app and validates
    # its nested code: Intel binaries start unsigned, unlike arm64 linker's
    # default ad-hoc signatures. Main-first therefore fails on unsigned helpers.
    for name in BINS:
        if name != "drcom-campus":
            run(["codesign", "--force", "--sign", "-", str(app / "Contents/MacOS" / name)])
    run(["codesign", "--force", "--sign", "-", str(app)])
    run(["codesign", "--verify", "--deep", "--strict", str(app)])
    (volume / "Applications").symlink_to("/Applications", target_is_directory=True)
    dmg = out / f"{stem}.dmg"
    run(["hdiutil", "create", "-volname", "DrCom Campus", "-srcfolder", str(volume),
         "-format", "UDZO", "-ov", str(dmg)])
    run(["hdiutil", "verify", str(dmg)])
    app_archive = out / f"{stem}.app.tar.gz"
    # tar preserves executable modes, the app layout and the code signature.
    tar_tree(app, app_archive)
    return [app_archive, dmg]


def linux_stage(stage: Path, binary_dir: Path, ver: str, target: str, revision: str,
                root: Path = ROOT) -> Path:
    portable = stage / f"drcom-campus-linux-x86_64-v{ver}"
    stage_common(portable, root, ver, target, revision)
    stage_binaries(portable, binary_dir, target)
    copy(root / "packaging/install.sh", portable / "install.sh", executable=True)
    copy(root / "packaging/drcom-campus.desktop", portable / "drcom-campus.desktop")
    copy(root / "crates/drcom-gui/assets/icon-128.png",
         portable / "share/icons/hicolor/128x128/apps/drcom-campus.png")
    return portable


def linux_package(stage: Path, out: Path, binary_dir: Path, ver: str, target: str,
                  revision: str, root: Path = ROOT) -> list[Path]:
    portable = linux_stage(stage, binary_dir, ver, target, revision, root)
    tarball = out / (portable.name + ".tar.gz")
    tar_tree(portable, tarball)
    debroot = stage / "deb"
    for name in BINS:
        copy(portable / name, debroot / "usr/bin" / name, executable=True)
    copy(root / "packaging/drcom-campus.desktop", debroot / "usr/share/applications/drcom-campus.desktop")
    copy(root / "crates/drcom-gui/assets/icon-128.png",
         debroot / "usr/share/icons/hicolor/128x128/apps/drcom-campus.png")
    for name in (*LEGAL, "BUILD-INFO.json", "RELEASE-NOTES.txt", "drcom.ini.example"):
        copy(portable / name, debroot / "usr/share/doc/drcom-campus" / name)
    copy(root / "LICENSE", debroot / "usr/share/doc/drcom-campus/copyright")
    # Derive linked-library versions on the actual build host, not a guessed libc minimum.
    # AppIndicator/Wayland/X11 are also loaded dynamically, so declare them explicitly.
    write_text(stage / "debian/control", "Source: drcom-campus\n\nPackage: drcom-campus\nArchitecture: amd64\n")
    result = run(["dpkg-shlibdeps", "-O", *[f"-e{debroot / 'usr/bin' / name}" for name in BINS]],
                 cwd=stage, stdout=subprocess.PIPE, text=True)
    linked = next(line.removeprefix("shlibs:Depends=") for line in result.stdout.splitlines()
                  if line.startswith("shlibs:Depends="))
    deps = linked + ", libdbus-1-3, libgtk-3-0, libayatana-appindicator3-1, libxdo3, libxkbcommon0, libx11-6, libxcb1, libwayland-client0"
    deb_version = ver.replace("-", "~", 1)
    write_text(debroot / "DEBIAN/control", f"""Package: drcom-campus
Version: {deb_version}
Section: net
Priority: optional
Architecture: amd64
Maintainer: ZincGluxx <noreply@github.com>
Homepage: https://github.com/ZincGluxx/drcom-rs
Depends: {deps}
Recommends: gnome-keyring | keepassxc, iproute2, iputils-ping, curl
Suggests: network-manager, wl-clipboard, xclip, gnome-shell-extension-appindicator, libcap2-bin
Description: Dr.COM campus network client (Rust + Slint)
 GUI, CLI and headless daemon. Credentials use the desktop Secret Service.
 No service is enabled and no credentials are removed during install or uninstall.
""")
    deb = out / f"drcom-campus_{deb_version}_amd64.deb"
    run(["dpkg-deb", "--root-owner-group", "--build", str(debroot), str(deb)])
    run(["dpkg-deb", "--info", str(deb)])
    run(["desktop-file-validate", str(debroot / "usr/share/applications/drcom-campus.desktop")])
    return [tarball, deb]


def expected_assets(ver: str) -> set[str]:
    names = {f"drcom-campus-windows-x86_64-v{ver}-portable.zip",
             f"drcom-campus-windows-x86_64-v{ver}-setup.exe",
             f"drcom-campus-linux-x86_64-v{ver}.tar.gz",
             f"drcom-campus_{ver.replace('-', '~', 1)}_amd64.deb"}
    for arch in ("aarch64", "x86_64"):
        names.update(f"drcom-campus-macos-{arch}-v{ver}{suffix}" for suffix in (".app.tar.gz", ".dmg"))
    return names


def verify_release(out: Path, ver: str) -> None:
    expected = expected_assets(ver)
    allowed = expected | {name + ".sha256" for name in expected} | {"SHA256SUMS"}
    entries = list(out.iterdir())
    unsafe = [p.name for p in entries if not p.is_file() or p.is_symlink() or p.name not in allowed]
    actual = {p.name for p in entries if p.name in expected and p.is_file()}
    if actual != expected or unsafe:
        raise ValueError(f"Release asset mismatch. Missing={expected - actual}; unexpected={unsafe}")
    for name in sorted(expected):
        path = out / name
        if path.stat().st_size == 0:
            raise ValueError(f"Empty release asset: {name}")
        recorded = Path(str(path) + ".sha256").read_text(encoding="utf-8")
        if recorded != f"{checksum(path)}  {name}\n":
            raise ValueError(f"Checksum mismatch: {name}")
    write_checksums([out / name for name in expected], out)
    print(f"Verified {len(expected)} assets across {len(TARGETS)} native targets.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    ver = commands.add_parser("version", help="Print workspace version; optionally verify v* tag")
    ver.add_argument("--tag")
    package = commands.add_parser("package", help="Package already-built native binaries (does not compile)")
    package.add_argument("--target", choices=TARGETS, required=True)
    package.add_argument("--bin-dir", type=Path, help="Default: target/<triple>/release")
    package.add_argument("--out", type=Path, default=ROOT / "dist")
    package.add_argument("--revision", default="")
    package.add_argument("--tag")
    package.add_argument("--iscc", help="Inno Setup compiler path (Windows only)")
    verify = commands.add_parser("verify-release", help="Verify exact four-target asset set and merge SHA256SUMS")
    verify.add_argument("--out", type=Path, required=True)
    verify.add_argument("--tag", required=True)
    args = parser.parse_args()
    release_version = version(tag=args.tag)
    if args.command == "version":
        print(release_version)
    elif args.command == "verify-release":
        verify_release(args.out.resolve(), release_version)
    else:
        out = args.out.resolve()
        out.mkdir(parents=True, exist_ok=True)
        binary_dir = (args.bin_dir or ROOT / "target" / args.target / "release").resolve()
        # Only a newly-created temporary directory is cleaned; never recursively delete user paths.
        with tempfile.TemporaryDirectory(prefix=".package-", dir=out) as temp:
            stage = Path(temp)
            if "windows" in args.target:
                assets = windows_package(stage, out, binary_dir, release_version, args.target, args.revision, args.iscc)
            elif "apple" in args.target:
                assets = macos_package(stage, out, binary_dir, release_version, args.target, args.revision)
            else:
                assets = linux_package(stage, out, binary_dir, release_version, args.target, args.revision)
            write_checksums(assets, out)
            for asset in assets:
                print(f"{asset} ({asset.stat().st_size} bytes)")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError, StopIteration) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        sys.exit(1)
