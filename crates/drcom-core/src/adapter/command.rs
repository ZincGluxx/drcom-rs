//! Small bounded system-command runner. No shell, PATH lookup or interactive stdin.
use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn system_program(name: &str) -> io::Result<std::path::PathBuf> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid system command",
        ));
    }
    for directory in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let path = std::path::Path::new(directory).join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("system command {name} not installed"),
    ))
}

pub(crate) fn output(name: &str, arguments: &[&str], timeout: Duration) -> io::Result<Output> {
    let mut child = Command::new(system_program(name)?)
        .args(arguments)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Bound both streams independently and read concurrently so neither pipe
    // can deadlock the child. Exceeding the limit closes the pipe and fails.
    const LIMIT: u64 = 1024 * 1024;
    fn reader(stream: impl Read) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("system command output exceeded 1 MiB"));
        }
        Ok(bytes)
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || reader(stdout));
    let err = std::thread::spawn(move || reader(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{name} timed out"),
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error);
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("stdout reader failed"))?;
    let stderr = err
        .join()
        .map_err(|_| io::Error::other("stderr reader failed"))?;
    Ok(Output {
        status: status?,
        stdout: stdout?,
        stderr: stderr?,
    })
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn read(name: &str, arguments: &[&str]) -> Option<String> {
    let result = output(name, arguments, Duration::from_secs(3)).ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).into_owned())
}

pub(crate) fn valid_interface_name(name: &str) -> bool {
    // Reject path traversal and command-option ambiguity, not just NUL. Linux
    // IFNAMSIZ is 16; macOS also uses it. Non-ASCII names remain valid OS names.
    !name.is_empty()
        && name.len() < 16
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.contains(['/', '\\', '\0', '\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_and_interface_inputs_cannot_be_paths_or_options() {
        for name in ["", "../eth0", "-bad", "a/b", "a\\b", "a\0b", "eth0\n"] {
            assert!(!valid_interface_name(name), "{name:?}");
        }
        assert!(valid_interface_name("enp46s0"));
        assert!(valid_interface_name("en0"));
        assert!(system_program("../bin/sh").is_err());
    }
}
