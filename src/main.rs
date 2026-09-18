#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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

fn main() -> Result<(), slint::PlatformError> {
    // Won before anything else: a second copy must never reach `AppTray::new`,
    // or the notification area ends up with one icon per copy.
    let instance = match single_instance::claim(single_instance::APP_NAME) {
        single_instance::Launch::Primary(instance) => instance,
        single_instance::Launch::Duplicate => return Ok(()),
    };

    let app = AppWindow::new()?;
    let tray = AppTray::new()?;
    let backend = std::rc::Rc::new(ui_backend::Backend::attach(&app, &tray));
    let timer = slint::Timer::default();
    let tick = backend.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || tick.tick(),
    );
    let app_weak = app.as_weak();
    tray.on_show_requested(move || {
        if let Some(app) = app_weak.upgrade() {
            let _ = app.show();
        }
    });
    let quit_backend = backend.clone();
    tray.on_quit_requested(move || quit_backend.quit());

    // Slint's default response to the close button is to hide the window. That
    // is only half of what the "关闭到托盘" option means, so the decision is taken
    // here: hide when the option is on, and otherwise hand over to the normal
    // shutdown path — which logs out first and only then ends the event loop.
    // Returning `KeepWindowShown` is what stops the window disappearing before
    // that logout finishes.
    let close_backend = backend.clone();
    app.window().on_close_requested(move || {
        if close_backend.close_to_tray() {
            slint::CloseRequestResponse::HideWindow
        } else {
            close_backend.quit();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

    // Launching the executable again surfaces the window that is already
    // running — which is usually hidden in the tray — instead of adding a
    // second icon nobody can tell apart.
    let activate_weak = app.as_weak();
    instance.listen(move || {
        let app_weak = activate_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                let _ = app.show();
            }
        });
    });

    app.show()?;
    tray.show()?;
    slint::run_event_loop()
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
