//! The open Claude Code consoles, published as the `consoles.*` bindings:
//! `consoles.count`, then `consoles.<n>.name`, `consoles.<n>.live` and
//! `consoles.<n>.busy` for each slot, oldest console first.
//!
//! Which consoles are open, and their project names, come from NOBRAIN alone
//! (`local_services::consoles`). The widget only adds what NOBRAIN does not
//! keep: for each of those sessions, the file Claude Code writes in
//! `~/.claude/sessions/<pid>.json` gives its process, to bring the window to
//! the front, and whether it is working. A session without a running process
//! there (a console that crashed) is shown, but not live.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
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

use crate::local_services::OpenSession;

pub use crate::local_services::MAX_CONSOLES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeConsole {
    pub name: String,
    /// The running Claude Code process of the session; `None` when there is
    /// none, so the console is not live.
    pub pid: Option<u32>,
    pub busy: bool,
}

static CONSOLES: Mutex<Vec<ClaudeConsole>> = Mutex::new(Vec::new());

#[derive(Deserialize)]
struct SessionFile {
    pid: u32,
    #[serde(rename = "sessionId", default)]
    session_id: String,
    #[serde(default)]
    status: String,
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

/// Takes NOBRAIN's last list and rereads the session files: a few small
/// files, cheap enough for the machine-load timer, so the working state
/// follows within a second or two.
pub fn refresh() {
    // Keeps the NOBRAIN reader asking `jour` while a theme shows consoles.
    crate::local_services::NOBRAIN.latest();
    let sessions = crate::local_services::consoles();
    let dir = sessions_directory().unwrap_or_default();
    let consoles = enrich(&sessions, &dir, process_started_at);
    *CONSOLES.lock().unwrap_or_else(|poison| poison.into_inner()) = consoles;
}

fn enrich(
    sessions: &[OpenSession],
    dir: &Path,
    started_at: impl Fn(u32) -> Option<u64>,
) -> Vec<ClaudeConsole> {
    // A file left by a crashed console is skipped: its process must still be
    // running and have started when the file says it did.
    let files: Vec<SessionFile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<SessionFile>(&bytes).ok())
        .filter(|file| {
            started_at(file.pid)
                .is_some_and(|start| file.proc_start.parse::<u64>().ok() == Some(start))
        })
        .collect();
    sessions
        .iter()
        .map(|session| {
            let file = files.iter().find(|file| file.session_id == session.id);
            ClaudeConsole {
                name: session.projet.clone(),
                pid: file.map(|file| file.pid),
                busy: file.is_some_and(|file| file.status == "busy"),
            }
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

/// One focus at a time: attaching to a console is process-wide, and two
/// clicks racing through `AttachConsole`/`FreeConsole` are the main suspect
/// for the v2.18.0 heap corruption.
static FOCUSING: AtomicBool = AtomicBool::new(false);

/// Brings the console in `slot` to the front, and in Windows Terminal selects
/// its tab, so typing goes straight to its prompt. Runs on its own thread:
/// the accessibility calls go to another process and may take a moment. A
/// click while one is under way is ignored.
pub fn focus(slot: usize) {
    let Some(pid) = latest().get(slot).and_then(|console| console.pid) else {
        return;
    };
    if FOCUSING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("console-focus".into())
        .spawn(move || {
            if let Err(error) = focus_pid(pid) {
                crate::diagnose::log(format!("console {pid} could not be focused: {error}"));
            }
            FOCUSING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        FOCUSING.store(false, Ordering::Release);
    }
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
            let found = root.FindAll(TreeScope_Descendants, &automation.CreateTrueCondition()?)?;
            let mut tabs = Vec::new();
            let mut names = Vec::new();
            for index in 0..found.Length()? {
                let tab = found.GetElement(index)?;
                if tab.CurrentControlType()? == UIA_TabItemControlTypeId {
                    names.push(tab.CurrentName()?.to_string());
                    tabs.push(tab);
                }
            }
            if tabs.is_empty() {
                return Ok(());
            }
            let Some(index) = matching_tab(title, &names) else {
                crate::diagnose::log(format!(
                    "no single terminal tab matches console title {title:?}; tabs: {names:?}"
                ));
                return Ok(());
            };
            let item: IUIAutomationSelectionItemPattern =
                tabs[index].GetCurrentPatternAs(UIA_SelectionItemPatternId)?;
            item.Select()
        })();
        CoUninitialize();
        result
    }
}

/// The one tab named after the console title. Claude Code puts a status
/// glyph in front of its title, a spinner that changes while it works, so
/// the glyph is ignored; failing an exact match, the tab whose name contains
/// the title. `None` when no tab, or more than one, matches.
fn matching_tab(title: &str, names: &[String]) -> Option<usize> {
    fn bare(text: &str) -> &str {
        text.trim_start_matches(|c: char| !c.is_alphanumeric())
    }
    let title = bare(title);
    if title.is_empty() {
        return None;
    }
    let single = |found: Vec<usize>| (found.len() == 1).then(|| found[0]);
    let exact: Vec<usize> = (0..names.len()).filter(|&i| bare(&names[i]) == title).collect();
    if !exact.is_empty() {
        return single(exact);
    }
    single((0..names.len()).filter(|&i| names[i].contains(title)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nobrain_sessions_get_their_process_and_state_from_claude_code() {
        let dir = std::env::temp_dir().join(format!("claude-consoles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |pid: u32, body: &str| std::fs::write(dir.join(format!("{pid}.json")), body);
        write(1, r#"{"pid":1,"sessionId":"a","status":"busy","procStart":"100"}"#).unwrap();
        write(2, r#"{"pid":2,"sessionId":"b","status":"idle","procStart":"200"}"#).unwrap();
        // Same pid reused by another process: the file is stale.
        write(3, r#"{"pid":3,"sessionId":"c","status":"busy","procStart":"300"}"#).unwrap();
        // A console NOBRAIN does not list is not shown.
        write(4, r#"{"pid":4,"sessionId":"d","status":"busy","procStart":"400"}"#).unwrap();
        std::fs::write(dir.join("3.key"), "not a session").unwrap();

        let session = |id: &str, projet: &str| OpenSession {
            id: id.into(),
            projet: projet.into(),
        };
        let consoles = enrich(
            &[session("b", "coeur-fou"), session("a", "nobrain"), session("c", "-")],
            &dir,
            |pid| match pid {
                1 => Some(100),
                2 => Some(200),
                3 => Some(999),
                4 => Some(400),
                _ => None,
            },
        );
        std::fs::remove_dir_all(&dir).unwrap();

        let console = |name: &str, pid: Option<u32>, busy: bool| ClaudeConsole {
            name: name.into(),
            pid,
            busy,
        };
        assert_eq!(
            consoles,
            vec![
                console("coeur-fou", Some(2), false),
                console("nobrain", Some(1), true),
                console("-", None, false),
            ]
        );
    }

    #[test]
    fn without_the_session_files_the_consoles_are_shown_not_live() {
        let sessions = [OpenSession {
            id: "a".into(),
            projet: "nobrain".into(),
        }];
        let consoles = enrich(&sessions, Path::new("Z:\\no\\such\\dir"), |_| Some(1));
        assert_eq!(consoles.len(), 1);
        assert_eq!(consoles[0].pid, None);
    }

    #[test]
    fn console_title_finds_its_own_terminal_tab() {
        let tabs = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let names = tabs(&["✳ DUST - DUST > AO", "◐ Perso - Console Claude > dev Usage widget"]);
        // Equal titles.
        assert_eq!(matching_tab("✳ DUST - DUST > AO", &names), Some(0));
        // The spinner moved between reading the title and the tabs.
        assert_eq!(matching_tab("◓ Perso - Console Claude > dev Usage widget", &names), Some(1));
        assert_eq!(matching_tab("Perso - Console Claude > dev Usage widget", &names), Some(1));
        // A terminal that adds to the tab name.
        assert_eq!(matching_tab("DUST - DUST > AO", &tabs(&["Admin: DUST - DUST > AO"])), Some(0));
        // None, or more than one.
        assert_eq!(matching_tab("Windows PowerShell", &names), None);
        assert_eq!(matching_tab("✳", &names), None);
        assert_eq!(matching_tab("✳ DUST", &tabs(&["✳ DUST", "◐ DUST"])), None);
        assert_eq!(matching_tab("DUST", &tabs(&["✳ DUST - AO", "◐ DUST - BO"])), None);
    }
}
