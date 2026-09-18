//! Build script: compiles the Slint UI and attaches the application icon to the
//! Windows executables.
//!
//! Cargo has no built-in way to put a Win32 resource into a binary, so the icon
//! is compiled to a `.res` file with the Windows SDK's `rc.exe` and handed to the
//! linker as an extra input (`cargo:rustc-link-arg-bins`), which is what the
//! `winres` / `embed-resource` crates do internally. Doing it by hand keeps the
//! build offline-capable and avoids another dependency.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    slint_build::compile("ui/app.slint").expect("compile Slint UI");
    embed_icon();
}

/// Embeds `assets/icon.ico` — the icon used by the C# client — into both binaries.
///
/// A missing `rc.exe` is deliberately not fatal: the build still succeeds with
/// the default icon, and the reason is surfaced as a cargo warning.
fn embed_icon() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Build the absolute path from CARGO_MANIFEST_DIR rather than canonicalizing:
    // on Windows `canonicalize` yields a `\\?\` verbatim path, which rc.exe
    // rejects as a syntax error.
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo always sets it"))
        .join("assets")
        .join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.is_file() {
        println!(
            "cargo:warning={} is missing - building without an application icon",
            icon.display()
        );
        return;
    }
    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=rc.exe was not found (install the Windows SDK) - \
             building without an application icon"
        );
        return;
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo always sets OUT_DIR"));
    let res = out_dir.join("drcom-app.res");
    // The version block is the only part of a resource script that can trip up an
    // unusual toolchain, so fall back to an icon-only script before giving up.
    if compile(&rc, &out_dir, &icon, true) || compile(&rc, &out_dir, &icon, false) {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!(
            "cargo:warning=rc.exe could not compile the icon resource - \
             building without an application icon"
        );
    }
}

/// Compiles a resource script into `OUT_DIR/drcom-app.res`.
///
/// `with_version` adds the `VERSIONINFO` block that fills in the Details tab of
/// the file properties dialog. Returns whether a resource file was produced.
fn compile(rc: &Path, out_dir: &Path, icon: &Path, with_version: bool) -> bool {
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let script = out_dir.join("drcom-app.rc");
    if fs::write(&script, resource_script(icon, &version, with_version)).is_err() {
        return false;
    }

    let res = out_dir.join("drcom-app.res");
    let _ = fs::remove_file(&res);
    let status = Command::new(rc)
        .arg("/nologo")
        // Neutral language; the strings themselves are already Chinese.
        .arg("/l")
        .arg("0x409")
        .arg("/fo")
        .arg(&res)
        .arg(&script)
        .status();
    matches!(status, Ok(status) if status.success()) && res.is_file()
}

/// Builds the resource script text.
///
/// rc.exe reads UTF-16LE natively and the strings below are Chinese, so the text
/// is encoded explicitly instead of trusting the machine's ANSI code page.
fn resource_script(icon: &Path, version: &str, with_version: bool) -> Vec<u8> {
    // rc.exe parses string literals with C escapes, so a single backslash turns
    // `\assets` into a bell character and the file is reported as not found.
    let icon = icon.display().to_string().replace('\\', "\\\\");
    let mut text = format!("1 ICON \"{icon}\"\r\n");
    if with_version {
        let quad = version_quad(version);
        text.push_str(&format!(
            "\r\n\
             1 VERSIONINFO\r\n\
             \x20 FILEVERSION {quad}\r\n\
             \x20 PRODUCTVERSION {quad}\r\n\
             \x20 FILEFLAGSMASK 0x3fL\r\n\
             \x20 FILEFLAGS 0x0L\r\n\
             \x20 FILEOS 0x40004L\r\n\
             \x20 FILETYPE 0x1L\r\n\
             \x20 FILESUBTYPE 0x0L\r\n\
             BEGIN\r\n\
             \x20 BLOCK \"StringFileInfo\"\r\n\
             \x20 BEGIN\r\n\
             \x20   BLOCK \"080404b0\"\r\n\
             \x20   BEGIN\r\n\
             \x20     VALUE \"CompanyName\", \"ZincGluxx\"\r\n\
             \x20     VALUE \"FileDescription\", \"DrCom 校园网助手（Rust）\"\r\n\
             \x20     VALUE \"FileVersion\", \"{version}\"\r\n\
             \x20     VALUE \"InternalName\", \"drcom-campus\"\r\n\
             \x20     VALUE \"LegalCopyright\", \"Copyright (C) ZincGluxx\"\r\n\
             \x20     VALUE \"OriginalFilename\", \"drcom-campus.exe\"\r\n\
             \x20     VALUE \"ProductName\", \"DrCom 校园网助手（Rust）\"\r\n\
             \x20     VALUE \"ProductVersion\", \"{version}\"\r\n\
             \x20   END\r\n\
             \x20 END\r\n\
             \x20 BLOCK \"VarFileInfo\"\r\n\
             \x20 BEGIN\r\n\
             \x20   VALUE \"Translation\", 0x804, 1200\r\n\
             \x20 END\r\n\
             END\r\n"
        ));
    }
    utf16le(&text)
}

/// `VERSIONINFO` wants exactly four 0..=65535 numbers; `0.1.0` becomes `0,1,0,0`.
fn version_quad(version: &str) -> String {
    let mut parts: Vec<u16> = version
        .split(['.', '-', '+'])
        .filter_map(|part| part.parse::<u16>().ok())
        .collect();
    parts.resize(4, 0);
    parts
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// UTF-16LE with a byte-order mark, the encoding rc.exe detects natively.
fn utf16le(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.extend_from_slice(&[0xff, 0xfe]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Locates the resource compiler shipped with the Windows SDK.
fn find_rc() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    // `WindowsSdkVerBinPath` is `...\Windows Kits\10\bin\<version>\`, so the
    // resource compiler sits one level below it.
    if let Some(root) = env::var_os("WindowsSdkVerBinPath")
        && let Some(rc) = rc_in(Path::new(&root))
    {
        return Some(rc);
    }
    // Otherwise take the newest SDK version installed; environment variables for
    // the SDK are only set inside a developer command prompt.
    for root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        if let Some(rc) = newest_rc(Path::new(root)) {
            return Some(rc);
        }
    }
    None
}

/// Picks `rc.exe` out of the highest-numbered version directory under `root`.
fn newest_rc(root: &Path) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // Names are version strings, so a lexicographic sort misranks 10.0.x against
    // 10.0.9; compare the numeric parts instead.
    versions.sort_by_key(|path| version_key(path));
    versions.iter().rev().find_map(|path| rc_in(path))
}

fn version_key(path: &Path) -> [u32; 4] {
    let text = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut key = [0u32; 4];
    for (slot, part) in key.iter_mut().zip(text.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    key
}

/// Looks for `rc.exe` in the architecture folders of one SDK `bin` directory.
///
/// The resource compiler is a host tool, so the x64 build works for any target;
/// the other folders are only a fallback for unusual installations.
fn rc_in(dir: &Path) -> Option<PathBuf> {
    ["x64", "x86", "arm64", ""].into_iter().find_map(|arch| {
        let candidate = if arch.is_empty() {
            dir.join("rc.exe")
        } else {
            dir.join(arch).join("rc.exe")
        };
        candidate.is_file().then_some(candidate)
    })
}
