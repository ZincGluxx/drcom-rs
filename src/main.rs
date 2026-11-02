#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

//! DrCom 校园网助手（Rust 重写）图形入口。
//!
//! The authentication engine lives in the library crate; this binary only owns
//! the Slint window and, in `ui_backend`, the glue that connects the window to a
//! background session. The headless equivalent is the `drcom-cli` binary.

mod diagnostics;
mod single_instance;
mod static_ipv4;
mod ui_backend;

slint::include_modules!();

use slint::ComponentHandle;

/// Command line the GUI accepts. A desktop launch passes no arguments and keeps
/// the window-first behaviour; `--silent` exists for login autostart entries.
const USAGE: &str = "\
DrCom 校园网助手（Rust 版）
用法：drcom-campus [--silent]

  --silent, -s, --background   静默启动：不显示主窗口，按已保存的设置自动登录并在后台保活。
                               需要先在窗口里保存账号并勾选“自动登录”。
                               之后再次启动本程序即可唤出窗口。
  --help, -h                   显示本帮助

环境变量：
  DRCOM_THEME=light|dark       固定浅色 / 深色配色
  DRCOM_SILENT=1               等价于 --silent
  DRCOM_SERVER=...             覆盖认证服务器地址
";

struct LaunchOptions {
    silent: bool,
    help: bool,
}

impl LaunchOptions {
    fn from_environment() -> Self {
        let mut options = Self {
            silent: false,
            help: false,
        };
        for argument in std::env::args().skip(1) {
            match argument.as_str() {
                "--silent" | "--background" | "-s" => options.silent = true,
                "--help" | "-h" => options.help = true,
                _ => {}
            }
        }
        if std::env::var("DRCOM_SILENT").is_ok_and(|value| !value.is_empty() && value != "0") {
            options.silent = true;
        }
        options
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let options = LaunchOptions::from_environment();
    if options.help {
        print!("{USAGE}");
        return Ok(());
    }

    // Won before anything else: a second copy must never reach `AppTray::new`,
    // or the notification area ends up with one icon per copy.
    let instance = match single_instance::claim(single_instance::APP_NAME) {
        single_instance::Launch::Primary(instance) => instance,
        single_instance::Launch::Duplicate => return Ok(()),
    };

    let app = AppWindow::new()?;
    // The window follows the desktop colour scheme by default; `DRCOM_THEME` pins it
    // instead (0 = auto, 1 = light, 2 = dark), for users whose desktop reports the
    // wrong scheme and for capturing screenshots of both palettes. Slint follows the
    // live theme where the platform reports one; the Linux X11 backend never does,
    // so ask the desktop once at start-up instead.
    let scheme_override = match std::env::var("DRCOM_THEME").as_deref() {
        Ok("light") => 1,
        Ok("dark") => 2,
        _ => desktop_scheme_hint().unwrap_or(0),
    };
    app.global::<Theme>().set_scheme_override(scheme_override);
    // The tray icon is the only way back to a hidden window, and on Linux it is
    // served over StatusNotifierItem: a desktop without a watcher (stock GNOME, for
    // example) fails to create one. The window must still start, so the tray is
    // optional — `tray-available` makes the UI quit on close instead of hiding
    // into a tray that does not exist.
    let tray = if status_notifier_available() {
        AppTray::new().ok()
    } else {
        None
    };
    app.set_tray_available(tray.is_some());
    // Arc rather than Rc: the single-instance worker thread has to call
    // surface_window(). Every field of Backend is Send + Sync already.
    let backend = std::sync::Arc::new(ui_backend::Backend::attach(
        &app,
        tray.as_ref(),
        options.silent,
    ));
    let timer = slint::Timer::default();
    let tick = backend.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || tick.tick(),
    );
    if let Some(tray) = &tray {
        let show_backend = backend.clone();
        tray.on_show_requested(move || show_backend.surface_window());
        let quit_backend = backend.clone();
        tray.on_quit_requested(move || quit_backend.quit());
    }

    // Slint's default response to the close button is to hide the window. That
    // is only half of what the "关闭到托盘" option means, so the decision is taken
    // here: hide when the option is on, and otherwise hand over to the normal
    // shutdown path — which logs out first and only then ends the event loop.
    // Returning `KeepWindowShown` in both cases is deliberate: the backend has
    // already minimized the window, and Slint's own `HideWindow` reaction destroys
    // the surface on Wayland (winit cannot hide there), which is what left an
    // unopenable entry in the dock. The quit branch keeps the window until logout.
    let close_backend = backend.clone();
    app.window().on_close_requested(move || {
        if close_backend.close_to_tray() {
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            close_backend.quit();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

    // Launching the executable again surfaces the window that is already
    // running — which is usually hidden in the tray — instead of adding a
    // second icon nobody can tell apart.
    let activate_backend = backend.clone();
    instance.listen(move || {
        let backend = activate_backend.clone();
        let _ = slint::invoke_from_event_loop(move || backend.surface_window());
    });

    // Silent start: the window is created (so the session can put login failures
    // and the tray tooltip somewhere) but stays hidden. A second launch of the
    // binary surfaces it through the single-instance handover.
    if !options.silent {
        app.show()?;
        backend.note_window_shown();
    }
    if let Some(tray) = &tray {
        tray.show()?;
    }
    // A hidden-but-alive window must not end the loop: that is exactly the state
    // of a silent start and of a window that was closed on a desktop without a
    // tray. Shutdown is always explicit — the close button when the option says
    // "quit", the tray menu, or the session's logout flow.
    slint::run_event_loop_until_quit()
}

/// Whether this desktop can host Slint's Linux tray backend.
///
/// Slint implements the Linux tray as a StatusNotifierItem, and its constructor
/// fails when no watcher owns `org.kde.StatusNotifierWatcher` (stock GNOME, for
/// example). Asking the session bus first keeps the console quiet — Slint would
/// otherwise print a warning at start-up — and lets the window tell the truth
/// about what the close button does.
#[cfg(target_os = "linux")]
fn status_notifier_available() -> bool {
    if let Some(available) = session_bus_name_owner_with_busctl() {
        return available;
    }
    if let Some(available) = session_bus_name_owner_with_gdbus() {
        return available;
    }
    // Neither tool is installed: let the tray constructor try, `.ok()` covers
    // the failure case.
    true
}

#[cfg(target_os = "linux")]
fn session_bus_name_owner_with_busctl() -> Option<bool> {
    let output = std::process::Command::new("busctl")
        .args(["--user", "status", "org.kde.StatusNotifierWatcher"])
        .output()
        .ok()?;
    // A missing bus or an unknown name both mean "no tray is possible here".
    Some(output.status.success())
}

#[cfg(target_os = "linux")]
fn session_bus_name_owner_with_gdbus() -> Option<bool> {
    let output = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.NameHasOwner",
            "org.kde.StatusNotifierWatcher",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Some(answer.trim().ends_with("true"))
}

#[cfg(not(target_os = "linux"))]
fn status_notifier_available() -> bool {
    true
}

/// Best-effort desktop colour-scheme probe for Linux.
///
/// Slint exposes `Palette.color-scheme` from the window, but X11 never reports a
/// scheme and Wayland depends on the compositor. GNOME and KDE both publish the
/// preference through their own tools, so ask them once at start-up. `DRCOM_THEME`
/// still takes precedence, and a desktop that cannot be identified keeps falling
/// back to Slint's own auto detection.
#[cfg(target_os = "linux")]
fn desktop_scheme_hint() -> Option<i32> {
    fn output(command: &str, arguments: &[&str]) -> Option<String> {
        let output = std::process::Command::new(command)
            .args(arguments)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    if let Some(value) = output(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    ) {
        let value = value.trim().trim_matches('\'').to_ascii_lowercase();
        if value.contains("prefer-dark") {
            return Some(2);
        }
        if value.contains("prefer-light") {
            return Some(1);
        }
        // `default` follows the GTK theme, and Adwaita-dark must still be respected.
        if value == "default"
            && let Some(theme) = output(
                "gsettings",
                &["get", "org.gnome.desktop.interface", "gtk-theme"],
            )
        {
            let theme = theme.trim().trim_matches('\'').to_ascii_lowercase();
            return Some(if theme.contains("dark") { 2 } else { 1 });
        }
    }
    for tool in ["kreadconfig6", "kreadconfig5"] {
        if let Some(value) = output(
            tool,
            &[
                "--file",
                "kdeglobals",
                "--group",
                "General",
                "--key",
                "ColorScheme",
            ],
        ) {
            let value = value.trim().to_ascii_lowercase();
            if !value.is_empty() {
                return Some(if value.contains("dark") { 2 } else { 1 });
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn desktop_scheme_hint() -> Option<i32> {
    None
}

#[cfg(test)]
mod theme_tests {
    //! The window paints its own surfaces instead of using the style's, so a broken
    //! dark mode does not crash anything — it just makes the text unreadable. These
    //! checks pin the contract rather than the exact colours: both schemes resolve,
    //! the text keeps a readable contrast against the surface behind it, and the two
    //! schemes are actually different.

    use super::{AppWindow, Theme};
    use slint::ComponentHandle;

    /// WCAG 2.1 contrast ratio between two opaque colours.
    fn contrast(a: slint::Color, b: slint::Color) -> f64 {
        fn luminance(colour: slint::Color) -> f64 {
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.039_28 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(colour.red())
                + 0.7152 * channel(colour.green())
                + 0.0722 * channel(colour.blue())
        }
        let (one, other) = (luminance(a), luminance(b));
        let (lighter, darker) = if one > other {
            (one, other)
        } else {
            (other, one)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn both_colour_schemes_keep_the_text_readable() {
        let Ok(app) = AppWindow::new() else {
            eprintln!("skipped: no windowing backend available to build the component on");
            return;
        };
        let theme = app.global::<Theme>();
        let mut surfaces = Vec::new();
        for (scheme, name) in [(1, "light"), (2, "dark")] {
            theme.set_scheme_override(scheme);
            let surface = theme.get_surface_background();
            surfaces.push(surface);
            for (label, colour) in [
                ("heading", theme.get_heading_text()),
                ("body", theme.get_body_text()),
                ("muted", theme.get_muted_text()),
                ("accent", theme.get_accent()),
            ] {
                let ratio = contrast(surface, colour);
                assert!(
                    ratio >= 4.5,
                    "{name} scheme: {label} text only reaches {ratio:.2}:1 on the card surface"
                );
            }
            let hero = contrast(theme.get_hero_background(), theme.get_heading_text());
            assert!(
                hero >= 4.5,
                "{name} scheme: hero heading only reaches {hero:.2}:1"
            );
            // The status dot in the hero card is the one colour that has to stay
            // legible on the hero fill and still read as "connected".
            let online = contrast(theme.get_hero_background(), theme.get_online_text());
            assert!(
                online >= 4.5,
                "{name} scheme: online indicator only reaches {online:.2}:1"
            );
        }
        // A dark scheme that resolves to the light palette would pass every contrast
        // check above and still be broken.
        assert_ne!(
            surfaces[0], surfaces[1],
            "the dark scheme paints the same card surface as the light one"
        );
    }
}

#[cfg(test)]
mod icon_tests {
    //! The window and tray icons come from `assets/`. Slint can only decode them
    //! because `Cargo.toml` switches on the `png` feature of the `image` crate —
    //! Slint itself pulls that crate in with no codecs at all. Drop the
    //! dependency and the icons silently become empty instead of failing to
    //! build, so the decoding is pinned down here.

    use std::path::Path;

    fn asset(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(name)
    }

    #[test]
    fn the_slint_icons_decode_with_the_formats_enabled_for_this_build() {
        for (name, expected) in [("icon-32.png", 32), ("icon-128.png", 128)] {
            let path = asset(name);
            let image = slint::Image::load_from_path(&path)
                .unwrap_or_else(|_| panic!("{} could not be decoded", path.display()));
            let size = image.size();
            assert_eq!(
                (size.width, size.height),
                (expected, expected),
                "{} decoded to the wrong size",
                path.display()
            );
        }
    }

    #[test]
    fn the_windows_icon_taken_from_the_c_sharp_client_is_intact() {
        // build.rs feeds this file to rc.exe, so an accidental edit that is not
        // really an icon would otherwise only show up as a build warning.
        let bytes = std::fs::read(asset("icon.ico")).expect("assets/icon.ico");
        assert_eq!(
            u16::from_le_bytes([bytes[0], bytes[1]]),
            0,
            "icon.ico has a reserved field that is not zero"
        );
        assert_eq!(
            u16::from_le_bytes([bytes[2], bytes[3]]),
            1,
            "icon.ico is not an icon (type 1)"
        );
        assert!(
            u16::from_le_bytes([bytes[4], bytes[5]]) >= 1,
            "icon.ico carries no images"
        );
    }
}
