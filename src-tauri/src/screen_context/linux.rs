//! The same reading on Linux, through the accessibility bus (AT-SPI over D-Bus): the active
//! window's title, the selected text and, when allowed, the app, page address and text around it.
//! It works on X11 and on Wayland, but only with apps that publish their accessibility tree:
//! GTK and Qt apps do; Chrome, Firefox and Electron apps only once the desktop's accessibility
//! is on (Xplainer never turns it on: it is a setting for the whole desktop).

use super::{clean_around, page_address, ScreenContext, LINES_AROUND, MAX_CHARS, TIMEOUT};
use crate::linux::Selected;
use atspi::{
    proxy::{
        accessible::AccessibleProxyBlocking, bus::BusProxyBlocking, collection::CollectionProxyBlocking,
        document::DocumentProxyBlocking, hyperlink::HyperlinkProxyBlocking, hypertext::HypertextProxyBlocking,
        text::TextProxyBlocking,
    },
    Interface, MatchType, ObjectMatchRule, ObjectRefOwned, Role, SortOrder, State,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use zbus::{
    blocking::{connection, proxy::Builder, Connection},
    proxy::{CacheProperties, Defaults},
};

/// The most each call may wait for an app: a hung app would otherwise hold the read for 25 s.
const CALL_TIMEOUT: Duration = Duration::from_millis(700);
/// Where the registry lists every app on the accessibility bus.
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
/// The character an app puts in its text for each embedded object (a link, a paragraph...).
const OBJECT: char = '\u{FFFC}';

/// What was read from the window the text was selected in.
pub struct Window {
    /// Its title, for the history's exclusion list.
    pub title: String,
    pub selected: Selected,
    pub context: Option<ScreenContext>,
}

/// The selection, with the lines before and after it in the same object.
struct Found {
    selected: String,
    before: String,
    after: String,
}

pub fn read(with_context: bool) -> Option<Window> {
    let deadline = Instant::now() + TIMEOUT;
    let session = connection::Builder::session().ok()?.method_timeout(CALL_TIMEOUT).build().ok()?;
    // The accessibility bus is its own bus; the session bus tells where it is (and starts it).
    let address = BusProxyBlocking::new(&session).ok()?.get_address().ok()?;
    let bus = connection::Builder::address(address.as_str()).ok()?.method_timeout(CALL_TIMEOUT).build().ok()?;

    let (app, frame) = active_frame(&bus, deadline)?;
    let title = frame.name().unwrap_or_default();
    let focused = focused_in(&bus, &frame);
    let document = focused.as_ref().and_then(|focused| document_of(&bus, focused, deadline));
    // Where the selection may be: the focused object itself, or the page it is in (on a web
    // page the selection usually belongs to the document, not to what has the focus).
    let answers: Vec<Option<Found>> = [focused.as_ref(), document.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|start| selection(&bus, start, deadline))
        .collect();
    let found = answers.into_iter().reduce(|first, second| first.or(second));
    let selected = match &found {
        Some(Some(found)) => Selected::Text(found.selected.clone()),
        Some(None) => Selected::Nothing,
        None => Selected::Unknown,
    };
    let found = found.flatten();

    let context = with_context.then(|| {
        let url = document.as_ref().and_then(|document| address_of(&bus, document)).unwrap_or_default();
        // Only what is inside a page or a document: never the address bar or a one-line field.
        let in_document = document.is_some() || focused.as_ref().is_some_and(is_multi_line);
        let around = match &found {
            Some(found) if in_document => clean_around(&found.before, &found.selected, &found.after),
            _ => String::new(),
        };
        ScreenContext { app: app.name().unwrap_or_default(), url, around }
    });
    let context = context.filter(|context| !context.url.is_empty() || !context.around.is_empty());
    Some(Window { title, selected, context })
}

/// A proxy for one interface of an object on the bus, with no property cache: each read is fresh.
fn proxy<'a, P>(bus: &'a Connection, object: &ObjectRefOwned) -> Option<P>
where
    P: From<zbus::Proxy<'a>> + Defaults,
{
    Builder::<P>::new(bus)
        .destination(object.name()?.clone())
        .ok()?
        .path(object.path().clone())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .ok()
}

/// Another interface of the same object.
fn same<'a, P>(bus: &'a Connection, object: &AccessibleProxyBlocking) -> Option<P>
where
    P: From<zbus::Proxy<'a>> + Defaults,
{
    Builder::<P>::new(bus)
        .destination(object.inner().destination().to_owned())
        .ok()?
        .path(object.inner().path().to_owned())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .ok()
}

/// The app and the window that has the focus: the one window whose state says "active".
fn active_frame<'a>(
    bus: &'a Connection,
    deadline: Instant,
) -> Option<(AccessibleProxyBlocking<'a>, AccessibleProxyBlocking<'a>)> {
    let root = Builder::<AccessibleProxyBlocking>::new(bus)
        .destination(REGISTRY)
        .ok()?
        .path(ROOT)
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .ok()?;
    for app in root.get_children().ok()? {
        if Instant::now() > deadline {
            return None; // The caller has stopped waiting anyway.
        }
        let Some(app) = proxy::<AccessibleProxyBlocking>(bus, &app) else { continue };
        let Ok(windows) = app.get_children() else { continue };
        for window in windows {
            let Some(window) = proxy::<AccessibleProxyBlocking>(bus, &window) else { continue };
            if window.get_state().is_ok_and(|state| state.contains(State::Active)) {
                return Some((app, window));
            }
        }
    }
    None
}

/// The object with the keyboard focus inside `frame`, found in one call.
fn focused_in<'a>(bus: &'a Connection, frame: &AccessibleProxyBlocking<'a>) -> Option<AccessibleProxyBlocking<'a>> {
    let collection = same::<CollectionProxyBlocking>(bus, frame)?;
    // The fields left out must say "not applicable": GTK matches nothing when they are "invalid",
    // the builder's default.
    let rule = ObjectMatchRule::builder()
        .states([State::Focused], MatchType::All)
        .attributes(HashMap::new(), MatchType::NA)
        .roles(&[], MatchType::NA)
        .interfaces([] as [Interface; 0], MatchType::NA)
        .build();
    let focused = collection.get_matches(rule, SortOrder::Canonical, 1, true).ok()?.into_iter().next()?;
    proxy(bus, &focused)
}

fn is_document(object: &AccessibleProxyBlocking) -> bool {
    object.get_role().is_ok_and(|role| {
        matches!(role, Role::DocumentWeb | Role::DocumentFrame | Role::DocumentText | Role::DocumentEmail)
    })
}

/// A text box of several lines (an editor, a terminal) counts as a document of its own.
fn is_multi_line(object: &AccessibleProxyBlocking) -> bool {
    object.get_state().is_ok_and(|state| state.contains(State::MultiLine))
}

/// The object itself or its closest ancestor that is a document (a web page, a text file).
fn document_of<'a>(
    bus: &'a Connection,
    object: &AccessibleProxyBlocking<'a>,
    deadline: Instant,
) -> Option<AccessibleProxyBlocking<'a>> {
    let mut object = object.clone();
    for _ in 0..40 {
        if Instant::now() > deadline {
            return None;
        }
        if is_document(&object) {
            return Some(object);
        }
        if object.get_role().is_ok_and(|role| role == Role::Frame) {
            return None; // Above the page is the app's own window.
        }
        object = proxy(bus, &object.parent().ok()?)?;
    }
    None
}

/// The page's address: Firefox calls it "DocURL" and Chromium "URI".
fn address_of<'a>(bus: &'a Connection, document: &AccessibleProxyBlocking) -> Option<String> {
    let document = same::<DocumentProxyBlocking>(bus, document)?;
    ["DocURL", "URI"]
        .into_iter()
        .filter_map(|name| document.get_attribute_value(name).ok())
        .find_map(|value| page_address(&value))
}

/// The selection under `start`: `None` when it has no text to ask (the app cannot say),
/// `Some(None)` when its text has nothing selected. A web page shows each child object as one
/// U+FFFC in its parent's text, so a selection made only of those is followed down into the
/// first one, as Orca does.
fn selection<'a>(bus: &'a Connection, start: &AccessibleProxyBlocking<'a>, deadline: Instant) -> Option<Option<Found>> {
    let mut object = start.clone();
    for _ in 0..20 {
        if Instant::now() > deadline {
            return None;
        }
        let text = same::<TextProxyBlocking>(bus, &object)?;
        if text.get_n_selections().ok()? == 0 {
            return Some(None);
        }
        let (start, end) = text.get_selection(0).ok()?;
        let selected = text.get_text(start, end).ok()?;
        if selected.chars().any(|c| c != OBJECT && !c.is_whitespace()) {
            let count = text.character_count().unwrap_or(end);
            let wide = MAX_CHARS as i32;
            let before = text.get_text((start - wide).max(0), start).unwrap_or_default();
            let after = text.get_text(end, (end + wide).min(count)).unwrap_or_default();
            let selected = with_objects(bus, &object, start, &selected, deadline);
            return Some(Some(Found { selected, before: last_lines(&before), after: first_lines(&after) }));
        }
        if !selected.contains(OBJECT) {
            return Some(None); // Just a caret, or blank space.
        }
        let first = start + selected.chars().position(|c| c == OBJECT)? as i32;
        object = child_at(bus, &same::<HypertextProxyBlocking>(bus, &object)?, first)?;
    }
    None
}

/// The child object that the U+FFFC at `offset` stands for.
fn child_at<'a>(bus: &'a Connection, hypertext: &HypertextProxyBlocking, offset: i32) -> Option<AccessibleProxyBlocking<'a>> {
    let link = hypertext.get_link(hypertext.get_link_index(offset).ok()?).ok()?;
    let child = proxy::<HyperlinkProxyBlocking>(bus, &link)?.get_object(0).ok()?;
    proxy(bus, &child)
}

/// The selected text with each U+FFFC replaced by the text of the object it stands for: a link
/// inside a sentence would otherwise drop its words ("un  de transporte").
fn with_objects<'a>(
    bus: &'a Connection,
    object: &AccessibleProxyBlocking<'a>,
    start: i32,
    selected: &str,
    deadline: Instant,
) -> String {
    if !selected.contains(OBJECT) {
        return selected.to_string();
    }
    let hypertext = same::<HypertextProxyBlocking>(bus, object);
    let text_of = |offset: i32| -> Option<String> {
        if Instant::now() > deadline {
            return None;
        }
        let child = child_at(bus, hypertext.as_ref()?, offset)?;
        let text = same::<TextProxyBlocking>(bus, &child)?;
        Some(text.get_text(0, text.character_count().ok()?).ok()?.replace(OBJECT, ""))
    };
    // AT-SPI counts offsets in characters, as `chars` does.
    selected
        .chars()
        .enumerate()
        .map(|(index, c)| if c == OBJECT { text_of(start + index as i32).unwrap_or_default() } else { c.to_string() })
        .collect()
}

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LINES_AROUND as usize)..].join("\n")
}

fn first_lines(text: &str) -> String {
    text.lines().take(LINES_AROUND as usize).collect::<Vec<_>>().join("\n")
}
