//! Claude Code consoles open on this machine, read from the files Claude Code
//! keeps in `~/.claude/sessions/<pid>.json`, published as the `consoles.*`
//! bindings: `consoles.count`, then `consoles.<n>.name` and
//! `consoles.<n>.busy` for each slot, oldest console first.
//!
//! A file left behind by a console that crashed is skipped: its process must
//! still be running and have started when the file says it did.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use windows::core::BSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleTitleW, GetConsoleWindow,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, IsIconic, SetForegroundWindow, ShowWindow, GA_ROOTOWNER, SW_RESTORE,
};

/// Slots a theme lays out; consoles past the last one are not shown.
pub const MAX_CONSOLES: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeConsole {
    pub pid: u32,
    pub name: String,
    pub busy: bool,
}

static CONSOLES: Mutex<Vec<ClaudeConsole>> = Mutex::new(Vec::new());

#[derive(Deserialize)]
struct SessionFile {
    pid: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "startedAt", default)]
    started_at: u64,
    /// Process creation time as a FILETIME, written as a string.
    #[serde(rename = "procStart", default)]
    proc_start: String,
}

fn sessions_directory() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude").join("sessions"))
}

pub fn latest() -> Vec<ClaudeConsole> {
    CONSOLES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// Rereads the session files. A few small files: cheap enough for the
/// machine-load timer.
pub fn refresh() {
    let consoles = sessions_directory()
        .map(|dir| read_sessions(&dir, process_started_at))
        .unwrap_or_default();
    *CONSOLES.lock().unwrap_or_else(|poison| poison.into_inner()) = consoles;
}

fn read_sessions(dir: &Path, started_at: impl Fn(u32) -> Option<u64>) -> Vec<ClaudeConsole> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<SessionFile> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<SessionFile>(&bytes).ok())
        .filter(|session| {
            started_at(session.pid)
                .is_some_and(|start| session.proc_start.parse::<u64>().ok() == Some(start))
        })
        .collect();
    sessions.sort_by_key(|session| session.started_at);
    sessions
        .into_iter()
        .take(MAX_CONSOLES)
        .map(|session| ClaudeConsole {
            pid: session.pid,
            name: session.name,
            busy: session.status == "busy",
        })
        .collect()
}

/// Creation time of a running process, as the FILETIME Claude Code records.
fn process_started_at(pid: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let read = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        read.ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

/// Brings the console in `slot` to the front, and in Windows Terminal selects
/// its tab. Runs on its own thread: the accessibility calls go to another
/// process and may take a moment.
pub fn focus(slot: usize) {
    let Some(pid) = latest().get(slot).map(|console| console.pid) else {
        return;
    };
    std::thread::spawn(move || {
        if let Err(error) = focus_pid(pid) {
            crate::diagnose::log(format!("console {pid} could not be focused: {error}"));
        }
    });
}

fn focus_pid(pid: u32) -> windows::core::Result<()> {
    let (window, title) = unsafe {
        AttachConsole(pid)?;
        let console = GetConsoleWindow();
        let mut buffer = [0u16; 512];
        let len = GetConsoleTitleW(&mut buffer) as usize;
        let _ = FreeConsole();
        // Under Windows Terminal the console window is a hidden stand-in
        // owned by the terminal window; under the classic host it is the
        // window itself.
        let root = GetAncestor(console, GA_ROOTOWNER);
        let window = if root.is_invalid() { console } else { root };
        (window, String::from_utf16_lossy(&buffer[..len.min(buffer.len())]))
    };
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
    select_tab(window, &title)
}

/// Selects the Windows Terminal tab whose title is the console's title.
/// Nothing to do in a window without tabs.
fn select_tab(window: HWND, title: &str) -> windows::core::Result<()> {
    if title.is_empty() {
        return Ok(());
    }
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let root = automation.ElementFromHandle(window)?;
            let tabs = root.FindAll(TreeScope_Descendants, &automation.CreateTrueCondition()?)?;
            for index in 0..tabs.Length()? {
                let tab = tabs.GetElement(index)?;
                if tab.CurrentControlType()? == UIA_TabItemControlTypeId
                    && tab.CurrentName()? == BSTR::from(title)
                {
                    let item: IUIAutomationSelectionItemPattern =
                        tab.GetCurrentPatternAs(UIA_SelectionItemPatternId)?;
                    return item.Select();
                }
            }
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_live_consoles_oldest_first_and_skips_stale_files() {
        let dir = std::env::temp_dir().join(format!("claude-consoles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |pid: u32, body: &str| std::fs::write(dir.join(format!("{pid}.json")), body);
        write(1, r#"{"pid":1,"name":"usagecodewidget-21","status":"busy","startedAt":20,"procStart":"100"}"#).unwrap();
        write(2, r#"{"pid":2,"name":"coeur-fou","status":"idle","startedAt":10,"procStart":"200"}"#).unwrap();
        // Same pid reused by another process: the file is stale.
        write(3, r#"{"pid":3,"name":"gone","status":"busy","startedAt":5,"procStart":"300"}"#).unwrap();
        std::fs::write(dir.join("3.key"), "not a session").unwrap();

        let consoles = read_sessions(&dir, |pid| match pid {
            1 => Some(100),
            2 => Some(200),
            3 => Some(999),
            _ => None,
        });
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            consoles,
            vec![
                ClaudeConsole {
                    pid: 2,
                    name: "coeur-fou".into(),
                    busy: false,
                },
                ClaudeConsole {
                    pid: 1,
                    name: "usagecodewidget-21".into(),
                    busy: true,
                },
            ]
        );
    }
}
