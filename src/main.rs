#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DrCom 校园网助手（Rust 重写）图形入口。
//!
//! The authentication engine lives in the library crate; this binary only owns
//! the Slint window and, in `ui_backend`, the glue that connects the window to a
//! background session. The headless equivalent is the `drcom-cli` binary.
//!
//! The window is built on demand and dropped again when the user closes it to the
//! tray. Slint's software renderer keeps a full-size target — and its backing
//! store — alive for as long as the component exists, which made the old
//! "hide the window but keep it around" path the single largest memory consumer
//! in this process. The tray, the session and the timer survive on their own.

mod diagnostics;
mod single_instance;
mod static_ipv4;
mod ui_backend;

slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    // Won before anything else: a second copy must never reach `AppTray::new`,
    // or the notification area ends up with one icon per copy.
    let instance_name = single_instance::app_name();
    let instance = match single_instance::claim(&instance_name) {
        single_instance::Launch::Primary(instance) => instance,
        single_instance::Launch::Duplicate => return Ok(()),
    };

    // The window follows the Windows app theme by default; `DRCOM_THEME` pins it
    // instead (0 = auto, 1 = light, 2 = dark), for users whose desktop reports the
    // wrong scheme and for capturing screenshots of both palettes.
    let scheme_override = match std::env::var("DRCOM_THEME").as_deref() {
        Ok("light") => 1,
        Ok("dark") => 2,
        _ => 0,
    };

    let tray = AppTray::new()?;
    let backend = Rc::new(ui_backend::Backend::new(&tray));
    // `None` is the normal resting state: a tray-only process with no window and
    // therefore no render target.
    let window: Rc<RefCell<Option<AppWindow>>> = Rc::new(RefCell::new(None));

    let timer = slint::Timer::default();

    // Hands the window — and with it the render target — back to the system.
    //
    // Hiding is not enough on Windows. Slint's software renderer presents with
    // damage (it only blits the region it repainted, see `present_with_damage` in
    // the winit backend), and softbuffer's buffer keeps whatever the last frame
    // left in it. While the window is off screen Windows repaints the client area
    // from the background brush, so after a plain hide/show the window comes back
    // with only the region that changed *since* the show painted — the top of the
    // window stays blank. winit does not report `Occluded` on Windows, so nothing
    // resets the renderer's partial-rendering cache in between.
    //
    // A window that goes to the tray is therefore destroyed instead of hidden,
    // and the next open builds a fresh one whose first frame is a full repaint.
    // That is also the memory the tray-only mode is supposed to be free of.
    let release_window: Rc<dyn Fn()> = {
        let backend = Rc::clone(&backend);
        let slot = Rc::clone(&window);
        Rc::new(move || {
            backend.unbind();
            // A rebuild satisfies whatever asked for the release.
            backend.clear_release_request();
            if let Some(released) = slot.borrow_mut().take() {
                let _ = released.hide();
            }
        })
    };
    // A close handler runs while its own component is still on the stack, so it
    // can only ask for the release on the next turn of the event loop.
    let release_window_later: Rc<dyn Fn()> = {
        let release = Rc::clone(&release_window);
        Rc::new(move || {
            let release = Rc::clone(&release);
            slint::Timer::single_shot(Duration::ZERO, move || release());
        })
    };

    // Building the window is deferred to the first time it is actually needed.
    let open: Rc<dyn Fn()> = {
        let backend = Rc::clone(&backend);
        let slot = Rc::clone(&window);
        let release = Rc::clone(&release_window);
        let release_after_close = Rc::clone(&release_window_later);
        Rc::new(move || {
            // Something hid the window and the one-second timer has not picked
            // the request up yet: honour it now, or the window would be taken
            // away again right after being put on screen.
            if backend.take_release_request() {
                release();
            } else if let Some(on_screen) = slot
                .borrow()
                .as_ref()
                .filter(|window| window.window().is_visible())
            {
                let _ = on_screen.show();
                return;
            }
            // Anything still in the slot is hidden, and a hidden window must not
            // be shown again — it would come back half painted.
            release();

            let built = match AppWindow::new() {
                Ok(built) => built,
                Err(error) => {
                    eprintln!("无法创建窗口：{error}");
                    return;
                }
            };
            built.global::<Theme>().set_scheme_override(scheme_override);

            // Slint's default response to the close button is to hide the window.
            // That is only half of what "关闭到托盘" means, so the decision is taken
            // here: hide and *release* the component when the option is on, and
            // otherwise hand over to the normal shutdown path — which logs out
            // first and only then ends the event loop. Returning `KeepWindowShown`
            // is what stops the window disappearing before that logout finishes.
            let closing = Rc::clone(&backend);
            let release = Rc::clone(&release_after_close);
            built.window().on_close_requested(move || {
                if closing.close_to_tray() {
                    release();
                    slint::CloseRequestResponse::HideWindow
                } else {
                    closing.quit();
                    slint::CloseRequestResponse::KeepWindowShown
                }
            });

            backend.bind(&built);
            let _ = built.show();
            *slot.borrow_mut() = Some(built);
        })
    };

    // A second launch (shortcut, Start menu, autostart entry) must surface the
    // window this process may be holding in the tray. That callback runs on the
    // single-instance thread, so it can only raise a flag for the event loop.
    let show_request = Arc::new(AtomicBool::new(false));

    let tick_backend = Rc::clone(&backend);
    let tick_open = Rc::clone(&open);
    let tick_release = Rc::clone(&release_window);
    let tick_request = Arc::clone(&show_request);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            if tick_request.swap(false, Ordering::Relaxed) {
                tick_open();
            }
            // The session thread and the auto-hide after a login both go through
            // `hide_to_tray`, which only records the request: the component has to
            // be released here, where no window callback is on the stack.
            tick_backend.tick();
            if tick_backend.take_release_request() {
                tick_release();
            }
        },
    );

    let tray_open = Rc::clone(&open);
    tray.on_show_requested(move || tray_open());
    let quit_backend = Rc::clone(&backend);
    tray.on_quit_requested(move || quit_backend.quit());

    let activate_request = Arc::clone(&show_request);
    instance.listen(move || activate_request.store(true, Ordering::Relaxed));

    tray.show()?;
    // `--tray` (used by the autostart entry, and handy on its own) starts in the
    // notification area without paying for a window nobody asked for.
    let start_hidden = std::env::args().any(|arg| arg == "--tray")
        || matches!(std::env::var("DRCOM_START_HIDDEN").as_deref(), Ok("1"));
    if !start_hidden {
        open();
    }
    slint::run_event_loop()
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
