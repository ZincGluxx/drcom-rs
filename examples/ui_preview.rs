//! Render the real UI with synthetic data, without loading credentials or networking.
use slint::ComponentHandle;
slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = std::env::args()
        .nth(1)
        .ok_or("provide an output PNG path")?;
    let width: f32 = std::env::args().nth(2).unwrap_or("520".into()).parse()?;
    let height: f32 = std::env::args().nth(3).unwrap_or("390".into()).parse()?;
    let app = AppWindow::new()?;
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
