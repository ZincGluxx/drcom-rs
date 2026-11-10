//! Window-lifecycle probe: rebuilds the sequences the real client goes through,
//! so a bad first frame can be looked at without touching the network or the
//! single-instance mutex.
//!
//! OPEN_MODE=startup   (default) build + show the window before the event loop
//! OPEN_MODE=tray-click          build + show it from inside the running loop,
//!                               which is what `--tray` does on a tray click
//! OPEN_MODE=reshow              start shown, hide to the tray, show it again
//!                               — this is the broken sequence: Slint's software
//!                               renderer only presents the region it repainted,
//!                               so the rest of the window comes back blank
//! OPEN_MODE=release-reshow      start shown, hide *and release*, build a new
//!                               window — what main.rs does, and it comes back
//!                               fully painted
//! OPEN_TICK=1                   push a property update every second
//! OPEN_SNAPSHOT=<png>           dump Slint's own frame buffer
//! OPEN_SNAPSHOT_MS=<n>          when to take it (default 2000)
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;

slint::include_modules!();

fn seed(app: &AppWindow) {
    app.global::<Theme>().set_scheme_override(0);
    app.set_username("example-user".into());
    app.set_password("correct horse battery".into());
    app.set_connection_state("已连接".into());
    app.set_online(true);
    app.set_session_active(true);
    app.set_status_detail("UDP 保活正在运行，下行正常".into());
    app.set_duration("已在线 1 分 0 秒".into());
    app.set_adapter_name("以太网".into());
    app.set_ipv4_address("10.0.0.9".into());
    app.set_campus_page_status("此程序 UDP 保活已建立".into());
    app.set_config_status("已记住你的账号".into());
    app.set_auto_reconnect(true);
    app.set_minimize_to_tray(true);
}

fn build(slot: &Rc<RefCell<Option<AppWindow>>>) {
    let app = match AppWindow::new() {
        Ok(app) => app,
        Err(error) => {
            eprintln!("cannot build window: {error}");
            return;
        }
    };
    seed(&app);
    let _ = app.show();
    println!(
        "window shown size={:?} scale={}",
        app.window().size(),
        app.window().scale_factor()
    );
    *slot.borrow_mut() = Some(app);
}

fn main() -> Result<(), slint::PlatformError> {
    let mode = std::env::var("OPEN_MODE").unwrap_or_else(|_| "startup".to_string());
    let tray = AppTray::new()?;
    tray.show()?;
    let slot: Rc<RefCell<Option<AppWindow>>> = Rc::new(RefCell::new(None));

    match mode.as_str() {
        "tray-click" => {
            let slot = Rc::clone(&slot);
            slint::Timer::single_shot(std::time::Duration::from_millis(1200), move || {
                println!("building window from inside the event loop");
                build(&slot);
            });
        }
        "reshow" => {
            build(&slot);
            let hidden = Rc::clone(&slot);
            slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                if let Some(app) = hidden.borrow().as_ref() {
                    let _ = app.hide();
                    println!("hidden to tray");
                }
            });
            let shown = Rc::clone(&slot);
            slint::Timer::single_shot(std::time::Duration::from_millis(4000), move || {
                if let Some(app) = shown.borrow().as_ref() {
                    let _ = app.show();
                    println!(
                        "shown again size={:?} scale={}",
                        app.window().size(),
                        app.window().scale_factor()
                    );
                }
            });
        }
        "release-reshow" => {
            // What main.rs does now: a window that goes to the tray is hidden and
            // then released, so the next open starts from a fresh render target.
            build(&slot);
            let hidden = Rc::clone(&slot);
            slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                if let Some(app) = hidden.borrow_mut().take() {
                    let _ = app.hide();
                    println!("hidden and released");
                }
            });
            let rebuilt = Rc::clone(&slot);
            slint::Timer::single_shot(std::time::Duration::from_millis(4000), move || {
                println!("building a fresh window");
                build(&rebuilt);
            });
        }
        _ => build(&slot),
    }

    let tick_timer = slint::Timer::default();
    if std::env::var("OPEN_TICK").is_ok() {
        let weak_slot = Rc::clone(&slot);
        let mut seconds = 0u32;
        tick_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                seconds += 1;
                if let Some(app) = weak_slot.borrow().as_ref()
                    && app.window().is_visible()
                {
                    app.set_duration(format!("已在线 1 分 {seconds} 秒").into());
                }
            },
        );
    }

    if let Ok(destination) = std::env::var("OPEN_SNAPSHOT") {
        let delay: u64 = std::env::var("OPEN_SNAPSHOT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2000);
        let slot = Rc::clone(&slot);
        slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
            let Some(pixels) = slot
                .borrow()
                .as_ref()
                .map(|app| app.window().take_snapshot().expect("snapshot"))
            else {
                return;
            };
            let file = std::fs::File::create(&destination).expect("PNG output");
            let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer
                .write_image_data(pixels.as_bytes())
                .expect("PNG pixels");
            println!("snapshot written to {destination}");
        });
    }

    let result = slint::run_event_loop();
    drop(tick_timer);
    result
}
