//! Start at login: a LaunchAgent on macOS, a logon scheduled task on Windows.
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

/// Windows: a scheduled task with a logon trigger for this user. Explorer
/// silently skipped the HKCU `Run` value this used before (no launch was
/// ever logged), while Task Scheduler keeps a run history.
#[cfg(windows)]
mod task {
    use std::io;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Output};
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW};

    const NAME: &str = "mouser";
    /// This is a GUI-subsystem exe: without this flag every schtasks call
    /// would flash a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn schtasks(args: &[&str]) -> io::Result<Output> {
        Command::new("schtasks.exe").args(args).creation_flags(CREATE_NO_WINDOW).output()
    }

    fn check(out: Output) -> io::Result<()> {
        if out.status.success() {
            return Ok(());
        }
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(io::Error::other(format!("schtasks: {msg}")))
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    /// Drops the `Run` value older versions wrote; true if there was one.
    fn remove_run_value() -> bool {
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (key, name) = (wide(r"Software\Microsoft\Windows\CurrentVersion\Run"), wide(NAME));
        // SAFETY: both are NUL-terminated UTF-16 buffers that outlive the call.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) == 0 }
    }

    /// An install that had start at login through the `Run` value gets the
    /// task instead. Returns true if it did.
    pub fn upgrade() -> io::Result<bool> {
        if !remove_run_value() {
            return Ok(false);
        }
        set(true).map(|()| true)
    }

    pub fn enabled() -> bool {
        schtasks(&["/Query", "/TN", NAME]).is_ok_and(|o| o.status.success())
    }

    pub fn set(on: bool) -> io::Result<()> {
        remove_run_value();
        if !on {
            return match schtasks(&["/Delete", "/TN", NAME, "/F"]) {
                Ok(_) if !enabled() => Ok(()),
                Ok(out) => check(out),
                Err(e) => Err(e),
            };
        }
        let exe = std::env::current_exe()?;
        let user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap_or_default(),
            std::env::var("USERNAME").unwrap_or_default()
        );
        // No time limit, runs on battery, one copy: mouser lives all session.
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command></Exec></Actions>
</Task>
"#,
            user = escape(&user),
            exe = escape(&exe.display().to_string()),
        );
        // schtasks wants the file in UTF-16 with a BOM, as the header says.
        let bytes: Vec<u8> =
            [0xFEFF].into_iter().chain(xml.encode_utf16()).flat_map(u16::to_le_bytes).collect();
        let path = std::env::temp_dir().join("mouser-task.xml");
        std::fs::write(&path, bytes)?;
        let out = schtasks(&["/Create", "/TN", NAME, "/XML", &path.to_string_lossy(), "/F"]);
        let _ = std::fs::remove_file(&path);
        check(out?)
    }
}

#[cfg(windows)]
pub use task::{enabled, set, upgrade};
