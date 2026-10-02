//! Start at sign-in, via a per-user Task Scheduler logon task.
//!
//! The classic HKCU\...\Run value turned out to be unreliable: on some
//! Windows 11 builds Explorer silently skips newly added entries at logon.
//! A logon task is started by the Task Scheduler service instead, needs no
//! admin rights for the current user, and runs at normal priority with no
//! time limit. The task is registered through schtasks.exe so the launcher
//! never loads the Task Scheduler COM API.

use crate::util::{reg_dword, wide};
use std::os::windows::process::CommandExt;
use windows_sys::Win32::System::Registry::*;

const TASK: &str = "Indicative";
const LEGACY_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Marker so the UI can tell whether autostart is on without spawning schtasks.
const STATE_KEY: &str = "Software\\Indicative";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn task_xml(exe: &str, user: &str) -> String {
    let (exe, user) = (xml_escape(exe), xml_escape(user));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Author>Indicative</Author><Description>Starts Indicative (Win+Space launcher) when you sign in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>"{exe}"</Command><Arguments>--background</Arguments></Exec></Actions>
</Task>
"#
    )
}

fn schtasks(args: &[&str]) -> bool {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn set_marker(on: bool) {
    let (key, name) = (wide(STATE_KEY), wide("Autostart"));
    let v: u32 = on as u32;
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_DWORD, &v as *const u32 as _, 4) };
}

fn remove_legacy_run_value() {
    let (key, name) = (wide(LEGACY_RUN_KEY), wide("Indicative"));
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
}

pub fn enabled() -> bool {
    reg_dword(HKEY_CURRENT_USER, STATE_KEY, "Autostart") == Some(1)
}

/// Register or remove the logon task. Returns false if schtasks failed.
pub fn set(on: bool) -> bool {
    remove_legacy_run_value();
    let ok = if on {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) => format!("{d}\\{u}"),
            (_, Ok(u)) => u,
            _ => return false,
        };
        // schtasks reads the XML as UTF-16 (BOM included).
        let path = std::env::temp_dir().join(format!("indicative-task-{}.xml", std::process::id()));
        let mut bytes = vec![0xFF, 0xFE];
        for u in task_xml(&exe, &user).encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let ok = std::fs::write(&path, bytes).is_ok() && schtasks(&["/Create", "/TN", TASK, "/XML", &path.to_string_lossy(), "/F"]);
        let _ = std::fs::remove_file(&path);
        ok
    } else {
        schtasks(&["/Delete", "/TN", TASK, "/F"]) || !schtasks(&["/Query", "/TN", TASK])
    };
    if ok {
        set_marker(on);
    }
    ok
}

/// 0.1.0/0.1.1 used a Run value; move those installs to the logon task.
pub fn migrate_legacy() {
    if crate::util::reg_string(HKEY_CURRENT_USER, LEGACY_RUN_KEY, "Indicative").is_some() {
        set(true);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn task_xml_escapes_paths() {
        let x = super::task_xml(r"C:\Users\A&B\indicative.exe", r"PC\me");
        assert!(x.contains(r#"<Command>"C:\Users\A&amp;B\indicative.exe"</Command>"#));
        assert!(x.contains(r"<UserId>PC\me</UserId>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }
}
