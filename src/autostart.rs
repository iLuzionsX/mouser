//! Start at login: a LaunchAgent on macOS, an HKCU `Run` value on Windows.
//! Both launch this same executable with no arguments, so the settings come
//! from the config file.

#[cfg(target_os = "macos")]
use std::io;

#[cfg(target_os = "macos")]
fn plist() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join("Library/LaunchAgents/dev.mouser.mouser.plist")
}

#[cfg(target_os = "macos")]
pub fn enabled() -> bool {
    plist().exists()
}

#[cfg(target_os = "macos")]
pub fn set(on: bool) -> io::Result<()> {
    if !on {
        return std::fs::remove_file(plist()).or_else(|e| match e.kind() {
            io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        });
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.mouser.mouser</string>
  <key>ProgramArguments</key><array><string>{exe}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    );
    std::fs::create_dir_all(plist().parent().unwrap_or(std::path::Path::new(".")))?;
    std::fs::write(plist(), xml)
}

#[cfg(windows)]
mod reg {
    use windows_sys::Win32::System::Registry::*;

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    pub const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub const NAME: &str = "mouser";

    pub fn enabled() -> bool {
        let (key, name) = (wide(KEY), wide(NAME));
        // SAFETY: null buffers ask only whether the value exists.
        let found = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        found == 0
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let (key, name) = (wide(KEY), wide(NAME));
        let status = if on {
            let exe = std::env::current_exe()?;
            let cmd = wide(&format!("\"{}\"", exe.display()));
            // SAFETY: every pointer is a NUL-terminated UTF-16 buffer that
            // outlives the call; the size includes the terminator.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    cmd.as_ptr().cast(),
                    (cmd.len() * 2) as u32,
                )
            }
        } else {
            // SAFETY: as above.
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
                2 => 0, // ERROR_FILE_NOT_FOUND: already off
                s => s,
            }
        };
        match status {
            0 => Ok(()),
            s => Err(std::io::Error::from_raw_os_error(s as i32)),
        }
    }
}

#[cfg(windows)]
pub use reg::{enabled, set};
