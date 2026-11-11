//! Reads where the selected text was: the app, the page address and the text around it. A word
//! alone ("negotiations") is ambiguous; its surroundings rarely are. With UI Automation on
//! Windows; with the accessibility bus (AT-SPI) on Linux, in `screen_context/linux.rs`.

use serde::Serialize;
#[cfg(windows)]
use std::ffi::c_void;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(windows)]
use windows::{
    core::{BSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, POINT},
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
            Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION},
        },
        UI::{
            Accessibility::*,
            WindowsAndMessaging::{
                GetAncestor, GetClassNameW, GetCursorPos, GetWindowThreadProcessId, WindowFromPoint, GA_ROOT,
            },
        },
    },
};

#[cfg(target_os = "linux")]
mod linux;

/// "Usar el contexto de la ventana" in the settings; the popup sets it at start.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// Enough for the surroundings of a title or a sentence, and cheap in tokens.
const MAX_CHARS: usize = 1500;
/// Lines read before and after the selection.
const LINES_AROUND: i32 = 8;
/// Names read in all near the cursor when there is no text to read around.
#[cfg(windows)]
const MAX_ELEMENTS: i32 = 200;
/// A busy or hung app answers UI Automation slowly: explain without context rather than wait.
const TIMEOUT: Duration = Duration::from_millis(800);
/// The most each UI Automation call may wait for the app (its defaults are 2 and 20 seconds).
#[cfg(windows)]
const CALL_TIMEOUT_MS: u32 = 700;
/// How long a Chromium window takes to build its accessibility tree after the first question
/// (measured: an empty pane at once, the page 300 ms later).
#[cfg(windows)]
const CHROMIUM_WAKE_UP: Duration = Duration::from_millis(300);
/// A read that is still running, stuck in a hung app: no new one starts until it ends, so
/// pressing Alt+X again and again cannot pile up threads.
static READING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScreenContext {
    /// Executable name without ".exe": "chrome", "Code", "WINWORD".
    pub app: String,
    /// Scheme, host and path of the web page, without anything that looks like a secret
    /// (`page_address`); empty outside a browser.
    pub url: String,
    /// The selection with the lines around it, or the names of the elements near it.
    pub around: String,
}

/// A read started when the shortcut was pressed; it runs while the selection is copied.
pub struct Pending<T> {
    started: Instant,
    result: Receiver<Option<T>>,
}

/// Starts reading `window`, the one the text was selected in, on its own thread.
#[cfg(windows)]
pub fn start(window: HWND) -> Option<Pending<ScreenContext>> {
    if !ENABLED.load(Ordering::SeqCst) {
        return None;
    }
    let raw = window.0 as isize; // A window handle is just a number, but HWND is not Send.
    spawn_read(move || read(HWND(raw as *mut c_void)))
}

/// Starts reading the active window on its own thread: its title and selection always (the
/// title decides what the history keeps), its context only if the settings allow it.
#[cfg(target_os = "linux")]
pub fn start() -> Option<Pending<linux::Window>> {
    let with_context = ENABLED.load(Ordering::SeqCst);
    spawn_read(move || linux::read(with_context))
}

fn spawn_read<T: Send + 'static>(read: impl FnOnce() -> Option<T> + Send + 'static) -> Option<Pending<T>> {
    if READING.swap(true, Ordering::SeqCst) {
        return None;
    }
    let (sender, result) = mpsc::channel();
    thread::spawn(move || {
        let found = read();
        READING.store(false, Ordering::SeqCst);
        let _ = sender.send(found);
    });
    Some(Pending { started: Instant::now(), result })
}

impl<T> Pending<T> {
    /// What was read, or `None` if it found nothing useful or did not finish in time.
    pub fn finish(self) -> Option<T> {
        let left = TIMEOUT.saturating_sub(self.started.elapsed());
        self.result.recv_timeout(left).ok().flatten()
    }
}

#[cfg(windows)]
fn read(window: HWND) -> Option<ScreenContext> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let context = read_with_com(window);
        CoUninitialize();
        context
    }
}

#[cfg(windows)]
unsafe fn read_with_com(window: HWND) -> Option<ScreenContext> {
    unsafe {
        let deadline = Instant::now() + TIMEOUT;
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);

        // The Windows 8 client: unlike the first one, its calls give up on an app that hangs.
        let automation: IUIAutomation2 = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = automation.SetConnectionTimeout(CALL_TIMEOUT_MS);
        let _ = automation.SetTransactionTimeout(CALL_TIMEOUT_MS);
        let walker = automation.ControlViewWalker().ok()?;
        // Only elements of the app the text came from: the cursor may rest on another window.
        let own = |element: &IUIAutomationElement| element.CurrentProcessId().is_ok_and(|id| id as u32 == pid);
        // Chromium (Chrome, Edge, Electron apps such as VS Code) builds its accessibility tree
        // only when a client first asks, and that first answer is an empty pane: ask once more.
        let mut tries = if is_chromium(window) { 2 } else { 1 };
        let (focused, hovered, document) = loop {
            let focused = automation.GetFocusedElement().ok().filter(own);
            // The cursor counts only over that same window: one browser shows many windows.
            let over_window = GetAncestor(WindowFromPoint(cursor), GA_ROOT) == window;
            let hovered = if over_window { automation.ElementFromPoint(cursor).ok().filter(own) } else { None };
            // Only what is inside a page or document: never the browser's own address bar or tabs.
            let document = focused.as_ref().and_then(|element| document_of(&walker, element));
            tries -= 1;
            if document.is_some() || tries == 0 || Instant::now() + CHROMIUM_WAKE_UP > deadline {
                break (focused, hovered, document);
            }
            thread::sleep(CHROMIUM_WAKE_UP);
        };
        let url = document
            .as_ref()
            .and_then(|document| document.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok())
            .map(|value| text(value.CurrentValue()))
            .and_then(|value| page_address(&value))
            .unwrap_or_default();

        let mut around = match (&focused, &document) {
            (Some(element), Some(_)) => text_around_selection(&walker, element).unwrap_or_default(),
            _ => String::new(),
        };
        if around.is_empty() {
            // No text with a selection (a table, a list, a custom control): what the elements
            // near the cursor are called, inside the page only, so browser tabs never leak.
            if let Some(start) = hovered.as_ref().or(focused.as_ref()) {
                around = names_near(&automation, &walker, start, deadline).unwrap_or_default();
            }
        }

        let context = ScreenContext { app: process_name(pid), url, around };
        (!context.url.is_empty() || !context.around.is_empty()).then_some(context)
    }
}

#[cfg(windows)]
fn text(value: windows::core::Result<BSTR>) -> String {
    value.map(|b| b.to_string()).unwrap_or_default()
}

/// Chrome, Edge and Electron apps all draw their windows with this class.
#[cfg(windows)]
fn is_chromium(window: HWND) -> bool {
    let mut buffer = [0u16; 64];
    let len = unsafe { GetClassNameW(window, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize]).starts_with("Chrome_WidgetWin_")
}

#[cfg(windows)]
fn is_document(element: &IUIAutomationElement) -> bool {
    unsafe { element.CurrentControlType() }.is_ok_and(|kind| kind == UIA_DocumentControlTypeId)
}

/// The element itself or its closest ancestor that is a document (a web page, a Word file).
#[cfg(windows)]
fn document_of(walker: &IUIAutomationTreeWalker, element: &IUIAutomationElement) -> Option<IUIAutomationElement> {
    let mut element = element.clone();
    for _ in 0..40 {
        if is_document(&element) {
            return Some(element);
        }
        element = unsafe { walker.GetParentElement(&element) }.ok()?;
    }
    None
}

/// The selection with a few lines before and after, from the closest element that exposes text.
#[cfg(windows)]
fn text_around_selection(walker: &IUIAutomationTreeWalker, focused: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let mut element = focused.clone();
        let mut pattern = None;
        for _ in 0..40 {
            if let Ok(found) = element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
                pattern = Some(found);
                break;
            }
            if is_document(&element) {
                return None; // Above the document is the app's own frame.
            }
            element = walker.GetParentElement(&element).ok()?;
        }
        let pattern = pattern?;
        let selections = pattern.GetSelection().ok()?;
        if selections.Length().ok()? == 0 {
            return None;
        }
        let selection = selections.GetElement(0).ok()?;
        let selected = text(selection.GetText(MAX_CHARS as i32));
        if selected.trim().is_empty() {
            return None; // Just a caret.
        }
        let before = selection.Clone().ok()?;
        before.MoveEndpointByRange(TextPatternRangeEndpoint_End, &selection, TextPatternRangeEndpoint_Start).ok()?;
        let _ = before.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Line, -LINES_AROUND);
        let after = selection.Clone().ok()?;
        after.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &selection, TextPatternRangeEndpoint_End).ok()?;
        let _ = after.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Line, LINES_AROUND);
        // An app without lines may move to the start of the document: keep the end of `before`.
        let before = text(before.GetText(20 * MAX_CHARS as i32));
        let after = text(after.GetText(MAX_CHARS as i32));
        Some(clean_around(&before, &selected, &after))
    }
}

/// The names of the elements in the closest container of `start` that holds some text, always
/// below the page or document it is in: the whole page would start with its menus, which say
/// nothing about the selection. Nothing outside a document, at most `MAX_ELEMENTS`, and only
/// until `deadline`, when the caller has stopped waiting anyway.
#[cfg(windows)]
fn names_near(
    automation: &IUIAutomation,
    walker: &IUIAutomationTreeWalker,
    start: &IUIAutomationElement,
    deadline: Instant,
) -> Option<String> {
    if is_document(start) {
        return None;
    }
    document_of(walker, start)?;
    // The same view as the walker: controls and content, not layout containers.
    let condition = unsafe { automation.ControlViewCondition() }.ok()?;
    let mut element = start.clone();
    let mut names = Vec::new();
    let mut left = MAX_ELEMENTS; // For the whole read, not for each level.
    for _ in 0..6 {
        names.clear();
        let found = unsafe { element.FindAll(TreeScope_Subtree, &condition) }.ok()?;
        let count = unsafe { found.Length() }.unwrap_or(0).min(left);
        left -= count;
        for i in 0..count {
            if Instant::now() > deadline {
                return None;
            }
            let Ok(child) = (unsafe { found.GetElement(i) }) else { continue };
            // The app stopped answering: each further call would wait for it again.
            let name = unsafe { child.CurrentName() }.ok()?;
            names.push(name.to_string());
        }
        if names.iter().map(String::len).sum::<usize>() >= 400 || left == 0 {
            break;
        }
        let parent = unsafe { walker.GetParentElement(&element) }.ok()?;
        if is_document(&parent) {
            break;
        }
        element = parent;
    }
    let joined = clean_names(&names);
    (!joined.is_empty()).then_some(joined)
}

#[cfg(windows)]
fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buffer = [0u16; 512];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        let file = path.rsplit('\\').next().unwrap_or_default();
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    }
}

/// Only web addresses, and only what tells what the page is about: no `user:password@`, no
/// `?query` or `#fragment`, and `…` for every path piece that is not a plain word
/// (`/reset/Xk29fa83`, `/invoices/1234`, a shared document's key). A `file:` path would show the user's folders.
pub(crate) fn page_address(value: &str) -> Option<String> {
    let (scheme, rest) = value.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    let path: Vec<&str> = path.split('/').map(|piece| if is_word(piece) { piece } else { "…" }).collect();
    let path = path.join("/");
    Some(if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") })
}

/// Only pieces that read like words (`negotiations`, `data-catalog`) say what the page is about;
/// anything else may be an id or a token (`1234`, `a1b2c3`, `AbCdEfGh`, a long key). Hiding a
/// harmless `v2` or `About` costs nothing.
fn is_word(piece: &str) -> bool {
    piece.len() <= 24 && piece.chars().all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
}

/// One line without the icons of icon fonts (private use area), object placeholders and extra spaces.
fn clean_line(line: &str) -> String {
    let kept: String = line
        .chars()
        .filter(|c| !matches!(*c, '\u{E000}'..='\u{F8FF}' | '\u{FFFC}' | '\u{FFFD}'))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_lines(text: &str) -> Vec<String> {
    text.lines().map(clean_line).filter(|line| !line.is_empty()).collect()
}

/// The selection in the middle of what surrounds it, cut to `MAX_CHARS`: half before, half after.
pub(crate) fn clean_around(before: &str, selected: &str, after: &str) -> String {
    let half = MAX_CHARS / 2;
    let before = clean_lines(before).join("\n");
    let after = clean_lines(after).join("\n");
    let before: String = before.chars().rev().take(half).collect::<Vec<_>>().into_iter().rev().collect();
    let after: String = after.chars().take(half).collect();
    let selected = clean_lines(selected).join("\n");
    [before, selected, after]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Names in order, without empty or repeated neighbours, cut to `MAX_CHARS`.
#[cfg_attr(not(windows), allow(dead_code))] // Only Windows reads the names near the cursor.
pub(crate) fn clean_names(names: &[String]) -> String {
    let mut kept: Vec<String> = Vec::new();
    for name in names.iter().map(|name| clean_line(name)) {
        if !name.is_empty() && kept.last() != Some(&name) {
            kept.push(name);
        }
    }
    kept.join(" | ").chars().take(MAX_CHARS).collect()
}
