//! Headless backend host.
//!
//! TODO(ipc-daemon task): parse `--stdio` / `--listen` / `--no-startup-probe` /
//! `--no-auto-login`, build a [`drcom_service::Service`], tick it once a second
//! and serve `drcom-ipc` requests until `shutdown`.

fn main() {
    eprintln!("drcom-daemon: not implemented yet");
    std::process::exit(2);
}
