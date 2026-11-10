//! Footprint probe: tray icon only, and (with FLOOR_OPEN_MS) a window that is
//! created, shown and then dropped again while the tray stays alive.
//!
//! Not part of the shipped product. It answers two questions:
//!   1. what does the resident tray cost when no window ever exists?
//!   2. after a window has been created and destroyed, does the process go back
//!      to that same number, or does the renderer stay behind?
use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let tray = AppTray::new()?;
    tray.show()?;
    println!("tray only, no window");

    let open_ms: u64 = std::env::var("FLOOR_OPEN_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let close_ms: u64 = std::env::var("FLOOR_CLOSE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let window = Rc::new(RefCell::new(None::<AppWindow>));
    if open_ms > 0 {
        let holder = window.clone();
        let opener = slint::Timer::default();
        opener.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(open_ms),
            move || {
                if let Ok(app) = AppWindow::new() {
                    app.global::<Theme>().set_scheme_override(1);
                    app.set_username("示例账号".into());
                    let _ = app.show();
                    println!("window shown");
                    *holder.borrow_mut() = Some(app);
                }
            },
        );
        std::mem::forget(opener);
    }

    if close_ms > 0 {
        let holder = window.clone();
        let closer = slint::Timer::default();
        closer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(close_ms),
            move || {
                if let Some(app) = holder.borrow_mut().take() {
                    let _ = app.hide();
                    drop(app);
                    println!("window dropped");
                }
            },
        );
        std::mem::forget(closer);
    }

    slint::run_event_loop()
}
