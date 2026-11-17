# Native packaging and release operator notes

No extra Rust packaging crate/framework is used. `scripts/package.py` requires
Python **3.11+** and only its standard library. It packages **already built**
`drcom-campus`, `drcom-cli` and `drcom-daemon`, validates native file headers and
architecture, copies legal notices/config template, then emits checksums.

## Required native tools

- Windows x64: MSVC/Windows SDK for Rust, **Inno Setup 6** (`ISCC.exe`). The
  script auto-detects the normal installation path or accepts `--iscc PATH`.
  CI sets `-C target-feature=+crt-static` so portable builds do not depend on a
  separately installed Visual C++ runtime. To match CI locally, set `RUSTFLAGS`
  to that value before building.
- macOS: Xcode command-line tools plus the OS `sips`, `iconutil`, `codesign`,
  `hdiutil`. No Developer ID certificate, Apple account or secrets are required.
  The app is **ad-hoc signed, NOT notarized**. Icons are generated from the
  existing GUI PNG with native tools; all executables are in `Contents/MacOS`.
- Linux x64: GCC/Clang, `pkg-config`, GTK3, Ayatana AppIndicator, libxdo, D-Bus,
  X11/Wayland/xkbcommon, udev, fontconfig/freetype development packages. `.deb`
  additionally needs `dpkg-dev` (`dpkg-shlibdeps`, `dpkg-deb`) and
  `desktop-file-utils`. The workflow installs the exact apt list. Runtime linked
  library versions are calculated by `dpkg-shlibdeps` on Ubuntu 22.04, with
  dynamically loaded desktop/tray dependencies declared explicitly. A Secret
  Service provider is recommended. This is **not an AppImage** and not static.

## Local builds

Run at the workspace root; build and package with the **same target triple**:

```sh
cargo build --locked --release --workspace --bins --target x86_64-pc-windows-msvc
python scripts/package.py package --target x86_64-pc-windows-msvc

cargo build --locked --release --workspace --bins --target aarch64-apple-darwin
python3 scripts/package.py package --target aarch64-apple-darwin

cargo build --locked --release --workspace --bins --target x86_64-apple-darwin
python3 scripts/package.py package --target x86_64-apple-darwin

cargo build --locked --release --workspace --bins --target x86_64-unknown-linux-gnu
python3 scripts/package.py package --target x86_64-unknown-linux-gnu
```

Use `--bin-dir target/release` for an ordinary local release build without
`--target`. `--out DIRECTORY` changes output (default `dist/`). `--revision SHA`
records source provenance. Optional `--tag v1.2.3` rejects workspace/tag mismatch.
The macOS packaging command must run natively for the selected architecture;
there is no cross-compilation or universal binary claim.

Plain `ISCC.exe setup.iss` remains supported after a local release build: version
comes from the GUI executable's Cargo version resource, binaries from
`target/release`, legal files from the repo root and output goes to `dist`.
CI overrides version, numeric file version, binary/staging/output directories
using preprocessor `/D` options. No hard-coded version needs updating in ISS.

The old `bash scripts/package-linux.sh` local command remains a wrapper around
locked workspace build + the new Python packager.

## CI, manual artifacts and release safety

`.github/workflows/native.yml` runs format, Clippy (`-D warnings`), locked native
workspace tests and package unit tests on Windows x64, macOS arm64, macOS x64 and
Linux x64. Every Cargo build/test/Clippy command uses `--locked`; Cargo.lock is
checked for changes. Rust is pinned to 1.98.0 with rustfmt/Clippy.

- PR/ordinary branch pushes: validation only; read-only contents, no custom secrets,
  persisted checkout token, `pull_request_target` or privileged workflow trigger.
- Pushes to the integration branch `macos`: also build/package all four targets
  and upload artifacts, so the first branch push validates native packaging
  without requiring manual dispatch or creating a Release.
- **Run workflow** (`workflow_dispatch`): builds all four targets and uploads
  `packages-<triple>` artifacts (14-day retention). It **never creates a Release**,
  even if manually run from a tag. Download artifacts from the Actions run page.
- **Pushing a `v*` tag**: checks exact `v<workspace.package.version>`, validates all
  four targets, builds/packages all binaries, then an isolated release job gets
  `contents: write`. Only that job receives its token in the publishing step.
  It verifies all **8 primary assets** and every individual checksum, merges
  `SHA256SUMS`, and publishes via GitHub CLI. A prerelease version gets a
  prerelease Release. Existing Releases are never silently overwritten.
- No signing/notarization secrets or manual publish toggle exist. To test a
  release candidate without publishing, use manual dispatch, **not a tag push**.
- This implementation does not push tags, commit, push or publish by itself.

Expected main assets: Windows setup EXE + portable ZIP; macOS arm64 and x64 each
`.app.tar.gz` (contains an actual `.app`) + DMG; Linux portable tar.gz + amd64 DEB.
Each gets `.sha256`, and Release includes aggregate `SHA256SUMS`. All archives,
app bundles and installers include LICENSE, NOTICE and THIRD-PARTY-NOTICES.md.

## Linux branch compatibility and installation

Preserves origin/linux's asset and top-level directory exactly:
`drcom-campus-linux-x86_64-v<VERSION>.tar.gz`, matching `.sha256`, root GUI/CLI,
`install.sh`, desktop entry and `share/icons/hicolor/128x128/apps/drcom-campus.png`.
New archives add the daemon and all notices. Historical remote installer scripts
can download new assets with `--version`; those old scripts only install GUI/CLI.

The new root `install.sh` resolves latest by default (explicit `--version` still
works), **requires SHA256 verification**, rejects unsafe archive paths/types and
delegates to the archive's installer, which installs **all three** binaries. It
uses direct GitHub by default: third-party mirrors are opt-in with `--mirror`.
User-level install needs no root, retains account data and never enables a
system daemon. `--autostart` is explicit; `--no-autostart` preserves existing
entries. `--uninstall` removes programs/launchers but retains accounts.

Custom test/mirror directory: `DRCOM_INSTALL_BASE_URL=http://127.0.0.1:8000`
plus `--version`. Checksums are integrity checks, not publisher signatures;
trusting a third-party mirror for both script/assets and sums is a trust choice.

## Self-tests

```sh
python -m unittest discover -s scripts -p 'test_package*.py' -v
for script in install.sh packaging/install.sh scripts/package-linux.sh; do bash -n "$script"; done
shellcheck install.sh packaging/install.sh scripts/package-linux.sh
# Linux only, isolated temporary HOME and localhost HTTP server; no real accounts:
python3 scripts/test_install.py
# Validate exact four-target release directory without publishing:
python scripts/package.py verify-release --out release-assets --tag v1.2.3
# If installed: actionlint .github/workflows/native.yml
```

Cross-platform unit tests cover architecture rejection, tag mismatch, Windows
staging/defines/ZIP, macOS relative executable/Info.plist/legal files, Linux
legacy tar layout/executable modes, missing binaries and release-set/checksum
tampering. Native macOS DMG/codesign and Linux dpkg execution are validated on
the corresponding GitHub runner; Windows-local tests cannot prove those tools.
