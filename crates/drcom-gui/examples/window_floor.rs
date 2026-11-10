//! Footprint probe: shows the real window with the real (software) renderer and
//! stays alive so an external tool can read the process memory counters.
//!
//! Not part of the shipped product. FLOOR_HIDE_MS=<n> hides the window after n
//! milliseconds, which is what the tray path does, so the cost of the render
//! target can be measured separately from the cost of the process itself.
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    app.global::<Theme>().set_scheme_override(1);
    app.set_username("示例账号".into());
    app.set_status_detail("网卡已就绪，填写账号后即可连接".into());
    app.set_adapter_name("以太网 · Realtek PCIe GbE".into());
    app.set_ipv4_address("10.0.0.9".into());
    app.set_gateway("10.0.0.1".into());
    // FLOOR_SIZE=WxH overrides the layout-pinned size, in logical pixels. Used to
    // work out how much of the footprint is per-pixel render target.
    if let Ok(spec) = std::env::var("FLOOR_SIZE")
        && let Some((w, h)) = spec.split_once('x')
        && let (Ok(w), Ok(h)) = (w.trim().parse::<f32>(), h.trim().parse::<f32>())
    {
        app.window().set_size(slint::LogicalSize::new(w, h));
    }
    app.show()?;
    let physical = app.window().size();
    println!(
        "window physical={}x{} scale={:.3}",
        physical.width,
        physical.height,
        app.window().scale_factor()
    );

    let hide_ms: u64 = std::env::var("FLOOR_HIDE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    println!("shown; hide_after_ms={hide_ms}");

    let keep_tray = std::env::var("FLOOR_TRAY").is_ok();
    let mut _tray = None;
    if keep_tray {
        let tray = AppTray::new()?;
        tray.show()?;
        _tray = Some(tray);
    }

    if hide_ms > 0 {
        let weak = app.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(hide_ms),
            move || {
                if let Some(app) = weak.upgrade() {
                    let _ = app.hide();
                    println!("hidden");
                }
            },
        );
        slint::run_event_loop()?;
        drop(timer);
    } else {
        slint::run_event_loop()?;
    }
    Ok(())
}
