//! Render the real UI with synthetic data, without loading credentials or networking.
//!
//! Usage: `ui_preview <out.png> [width] [height] [network] [auto|light|dark]`.
//! The last argument pins the colour scheme, which is the only way to capture the
//! dark palette on a machine whose Windows theme is light.
use slint::ComponentHandle;
slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = std::env::args()
        .nth(1)
        .ok_or("provide an output PNG path")?;
    let width: f32 = std::env::args().nth(2).unwrap_or("520".into()).parse()?;
    let height: f32 = std::env::args().nth(3).unwrap_or("460".into()).parse()?;
    let app = AppWindow::new()?;
    // 0 = follow the system, 1 = light, 2 = dark (see `Theme.scheme-override`).
    let scheme = match std::env::args().nth(5).as_deref() {
        Some("light") => 1,
        Some("dark") => 2,
        _ => 0,
    };
    app.global::<Theme>().set_scheme_override(scheme);
    eprintln!(
        "scheme-override={} dark={} window-background={:?}",
        app.global::<Theme>().get_scheme_override(),
        app.global::<Theme>().get_dark(),
        app.global::<Theme>().get_window_background()
    );
    app.set_username("示例账号".into());
    app.set_status_detail("网卡已就绪，填写账号后即可连接".into());
    app.set_adapter_name("以太网 · Realtek PCIe GbE".into());
    app.set_ipv4_address("10.0.0.9".into());
    app.set_gateway("10.0.0.1".into());
    app.set_dns("10.0.0.1, 1.1.1.1".into());
    app.set_ipv6("2001:db8::1234".into());
    if std::env::args().nth(4).as_deref() == Some("network") {
        app.set_network_editor_visible(true);
        app.set_edit_adapter_name("以太网 · 示例网卡".into());
        app.set_edit_ip("10.0.0.9".into());
        app.set_edit_mask("255.255.255.0".into());
        app.set_edit_gateway("10.0.0.1".into());
        app.set_edit_dns("1.1.1.1".into());
        app.set_network_message("应用时会请求管理员权限，并可能短暂中断此网卡的连接。".into());
    }
    if std::env::args().nth(4).as_deref() == Some("failure") {
        let failure = "Windows 阻止了客户端使用网络，可能是端口被系统保留或安全软件拦截。请使用自动端口，并检查安全软件是否允许本程序联网。";
        app.set_status_detail("本次连接未成功，请查看下方说明".into());
        app.set_last_failure(format!("2026-10-16 10:29:43\n{failure}").into());
        app.set_campus_result("可以连通（202.198.16.3）".into());
        app.set_ipv4_result("未收到回复；可能断网或对方不回应 Ping，不能据此判断登录失败".into());
        app.set_ipv6_result("baidu.com 没有 IPv6 地址，暂时无法检测这条线路".into());
    }
    app.window()
        .set_size(slint::LogicalSize::new(width, height));
    app.show()?;
    let window = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
        let app = window.upgrade().unwrap();
        let pixels = app.window().take_snapshot().expect("snapshot");
        let file = std::fs::File::create(destination).expect("PNG output");
        let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header");
        writer
            .write_image_data(pixels.as_bytes())
            .expect("PNG pixels");
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop()?;
    Ok(())
}
