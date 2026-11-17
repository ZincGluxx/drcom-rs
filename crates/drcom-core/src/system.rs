//! Small frontend-independent platform facts.

/// Native host name even for GUI launches whose environment lacks HOSTNAME.
pub fn host_name() -> String {
    for name in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return value;
        }
    }
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let length = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            if let Ok(value) = std::str::from_utf8(&buffer[..length])
                && !value.trim().is_empty()
            {
                return value.to_string();
            }
        }
    }
    "DRCOM-PC".into()
}
