//! Selecting text across messages by dragging, as in a web browser: from
//! the middle of one message, over names and whole messages, to the middle
//! of another. Slint selects inside a single text box only, so this watches
//! the mouse itself, finds the message text under it through Slint's item
//! tree, and sets the selection of every piece in between.
//!
//! Selectable pieces are marked in `app.slint` with an accessible
//! description: `sel|list|event id|part` on read-only text boxes (part 0 is
//! the sender's name, 1… the message's blocks) and `selbox|…` on the
//! shading laid over styled text, which can't show a selection itself.

use crate::{AppWindow, Block, MessageItem, TextTools};
use i_slint_core::accessibility::AccessibleStringProperty;
use i_slint_core::item_tree::{ItemRc, ParentItemTraversalMode};
use i_slint_core::items::{Flickable, TextInput};
use i_slint_core::lengths::{LogicalLength, LogicalPoint, LogicalRect};
use i_slint_core::window::WindowInner;
use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
use slint::winit_030::winit::keyboard::{Key, ModifiersState};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::Duration;

/// How far the mouse moves with the button down before it's a drag.
const DRAG_PX: f32 = 4.0;

/// A place in the text: a piece of one message, and a byte offset in it
/// (`usize::MAX` for "its end").
#[derive(Clone, Debug, PartialEq)]
struct Spot {
    id: String,
    part: usize,
    offset: usize,
}

/// A marked piece on screen.
struct Piece {
    item: ItemRc,
    list: String,
    id: String,
    part: usize,
    /// Shading over styled text, rather than a text box.
    shade: bool,
}

#[derive(Default)]
struct State {
    /// The mouse, in window coordinates.
    mouse: LogicalPoint,
    /// Where the left button went down, while it's down.
    pressed: Option<LogicalPoint>,
    /// Dragging a selection (the button is down and has moved).
    dragging: bool,
    /// The list the selection is in, and its two ends.
    list: String,
    anchor: Option<Spot>,
    focus: Option<Spot>,
    /// A selection made by dragging is showing.
    active: bool,
    /// A drag just ended: the click that ends it isn't a click.
    just_dragged: bool,
    modifiers: ModifiersState,
    /// Pieces this module selected or shaded, to clear later.
    touched: Vec<ItemRc>,
    scroller: Option<slint::Timer>,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
    /// Styled pieces under the selection ("list|id|part"), for `TextTools.shaded`.
    static SHADED: RefCell<std::collections::HashSet<String>> = RefCell::default();
}

/// Whether a drag selection just ended (so a click handler shouldn't act).
pub fn take_just_dragged() -> bool {
    ST.with(|s| std::mem::take(&mut s.borrow_mut().just_dragged))
}

/// Watches the window's mouse and keyboard for drag selections.
pub fn install(ui: &AppWindow) {
    let weak = ui.as_weak();
    let copy_weak = ui.as_weak();
    ui.global::<TextTools>().on_shaded(|key, _| SHADED.with(|s| s.borrow().contains(key.as_str())));
    ui.global::<TextTools>().on_copy_selection(move |fallback| {
        let Some(ui) = copy_weak.upgrade() else { return };
        let text = selected_text().unwrap_or_else(|| fallback.to_string());
        ui.invoke_copy_text(text.into());
    });
    ui.window().on_winit_window_event(move |window, event| {
        let Some(ui) = weak.upgrade() else { return EventResult::Propagate };
        on_event(&ui, window, event)
    });
}

fn on_event(ui: &AppWindow, window: &slint::Window, event: &WindowEvent) -> EventResult {
    match event {
        WindowEvent::ModifiersChanged(m) => {
            ST.with(|s| s.borrow_mut().modifiers = m.state());
            EventResult::Propagate
        }
        WindowEvent::CursorMoved { position, .. } => {
            let p = position.to_logical::<f32>(window.scale_factor() as f64);
            let mouse = LogicalPoint::new(p.x, p.y);
            let start = ST.with(|s| {
                let mut s = s.borrow_mut();
                s.mouse = mouse;
                match s.pressed {
                    Some(at) if !s.dragging && (mouse - at).length() >= DRAG_PX => Some(at),
                    _ => None,
                }
            });
            if let Some(at) = start {
                begin(ui, window, at);
            }
            if ST.with(|s| s.borrow().dragging) {
                extend(ui, window);
                // The text box the press landed in would select too.
                return EventResult::PreventDefault;
            }
            EventResult::Propagate
        }
        // The message box's menu offers spellings for the word under the mouse.
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Right,
            ..
        } => {
            if WindowInner::from_pub(window).active_popups().is_empty() {
                crate::spell_marks::right_click(ui, ST.with(|s| s.borrow().mouse));
            }
            EventResult::Propagate
        }
        WindowEvent::MouseInput {
            state,
            button: MouseButton::Left,
            ..
        } => {
            match state {
                ElementState::Pressed => {
                    // A menu is open (Slint draws it inside this window): the
                    // press is on it, e.g. its Copy, or closes it. Keep the
                    // selection for Copy, and don't start a drag from there.
                    if !WindowInner::from_pub(window).active_popups().is_empty() {
                        ST.with(|s| s.borrow_mut().pressed = None);
                        return EventResult::Propagate;
                    }
                    let active = ST.with(|s| {
                        let mut s = s.borrow_mut();
                        s.pressed = Some(s.mouse);
                        s.just_dragged = false;
                        s.active
                    });
                    if active {
                        clear(ui);
                    }
                }
                ElementState::Released => {
                    ST.with(|s| {
                        let mut s = s.borrow_mut();
                        s.pressed = None;
                        if s.dragging {
                            s.dragging = false;
                            s.just_dragged = true;
                            s.scroller = None;
                        }
                    });
                }
            }
            EventResult::Propagate
        }
        WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
            let (ctrl, active) = ST.with(|s| {
                let s = s.borrow();
                (s.modifiers.control_key() || s.modifiers.super_key(), s.active)
            });
            let copy = matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("c"));
            if active
                && ctrl
                && copy
                && let Some(text) = selected_text()
            {
                ui.invoke_copy_text(text.into());
                return EventResult::PreventDefault;
            }
            EventResult::Propagate
        }
        _ => EventResult::Propagate,
    }
}

/// Starts a drag selection if the button went down on message text.
fn begin(ui: &AppWindow, window: &slint::Window, at: LogicalPoint) {
    let pieces = pieces(window);
    let Some(hit) = pieces.iter().find(|p| p.item.is_visible() && window_rect(&p.item).contains(at)) else {
        // Not on text: an ordinary drag (scrollbar, window…). Forget the press.
        ST.with(|s| s.borrow_mut().pressed = None);
        return;
    };
    let list = hit.list.clone();
    let anchor = spot_at(window, &pieces, &list, at);
    clear(ui);
    ST.with(|s| {
        let mut s = s.borrow_mut();
        s.dragging = true;
        s.active = true;
        s.list = list;
        s.anchor = anchor;
        s.focus = None;
    });
    // Scrolls while the mouse is held above or below the messages.
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), move || {
        if let Some(ui) = weak.upgrade() {
            auto_scroll(&ui);
        }
    });
    ST.with(|s| s.borrow_mut().scroller = Some(timer));
    ui.global::<TextTools>().set_spanning(true);
}

/// Moves the loose end of the selection to the mouse and redraws it.
fn extend(ui: &AppWindow, window: &slint::Window) {
    let (list, mouse) = ST.with(|s| {
        let s = s.borrow();
        (s.list.clone(), s.mouse)
    });
    let pieces = pieces(window);
    let focus = spot_at(window, &pieces, &list, mouse);
    if focus.is_some() {
        ST.with(|s| s.borrow_mut().focus = focus);
    }
    paint(ui, &pieces);
}

/// Scrolls the list the selection is in when the mouse is past its top or
/// bottom edge, then extends the selection to what came into view.
fn auto_scroll(ui: &AppWindow) {
    let window = ui.window();
    let (list, mouse, dragging) = ST.with(|s| {
        let s = s.borrow();
        (s.list.clone(), s.mouse, s.dragging)
    });
    if !dragging {
        return;
    }
    let pieces = pieces(window);
    let Some(piece) = pieces.iter().find(|p| p.list == list) else { return };
    let Some(flick) = scroller_of(&piece.item) else { return };
    let rect = window_rect(&flick);
    let delta = if mouse.y < rect.min_y() {
        rect.min_y() - mouse.y
    } else if mouse.y > rect.max_y() {
        rect.max_y() - mouse.y
    } else {
        return;
    };
    let Some(f) = flick.downcast::<Flickable>() else { return };
    let f = f.as_pin_ref();
    let step = delta.signum() * delta.abs().min(120.0) / 3.0;
    let lowest = (rect.height() - f.content_height().get()).min(0.0);
    let y = (f.content_y().get() + step).clamp(lowest, 0.0);
    f.content_y.set(LogicalLength::new(y));
    extend(ui, window);
}

/// The scrolling area holding `item`.
fn scroller_of(item: &ItemRc) -> Option<ItemRc> {
    let mut at = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(i) = at {
        if i.downcast::<Flickable>().is_some() {
            return Some(i);
        }
        at = i.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    None
}

/// Every marked piece in the window.
fn pieces(window: &slint::Window) -> Vec<Piece> {
    let root = ItemRc::new_root(WindowInner::from_pub(window).component());
    let mut out = Vec::new();
    root.visit_descendants(|item| {
        if let Some(desc) = item.accessible_string_property(AccessibleStringProperty::Description)
            && let Some(p) = parse_mark(&desc)
        {
            out.push(Piece { item: item.clone(), list: p.0, id: p.1, part: p.2, shade: p.3 });
        }
        ControlFlow::<()>::Continue(())
    });
    out
}

/// (list, event id, part, is shading) from a piece's mark.
fn parse_mark(desc: &str) -> Option<(String, String, usize, bool)> {
    let (shade, rest) = if let Some(r) = desc.strip_prefix("selbox|") {
        (true, r)
    } else {
        (false, desc.strip_prefix("sel|")?)
    };
    let (list, rest) = rest.split_once('|')?;
    let (id, part) = rest.rsplit_once('|')?;
    Some((list.to_owned(), id.to_owned(), part.parse().ok()?, shade))
}

/// Where an item is, in window coordinates.
fn window_rect(item: &ItemRc) -> LogicalRect {
    let g = item.geometry();
    LogicalRect::new(item.map_to_window(g.origin), g.size)
}

/// The place in `list`'s text nearest the window point `p`.
fn spot_at(window: &slint::Window, pieces: &[Piece], list: &str, p: LogicalPoint) -> Option<Spot> {
    let shown: Vec<(&Piece, LogicalRect)> =
        pieces.iter().filter(|x| x.list == list && x.item.is_visible()).map(|x| (x, window_rect(&x.item))).collect();
    if shown.is_empty() {
        return None;
    }
    // On a piece, or level with one (beside it on its line).
    let level = shown
        .iter()
        .filter(|(_, r)| r.min_y() <= p.y && p.y < r.max_y())
        .min_by(|a, b| distance_x(&a.1, p.x).total_cmp(&distance_x(&b.1, p.x)));
    if let Some((piece, rect)) = level {
        return Some(offset_in(window, piece, rect, p));
    }
    // Between lines: the end of the last piece above, else the start of the first.
    let above = shown.iter().filter(|(_, r)| r.max_y() <= p.y).max_by(|a, b| a.1.max_y().total_cmp(&b.1.max_y()).then(a.1.min_x().total_cmp(&b.1.min_x())));
    if let Some((piece, _)) = above {
        return Some(Spot { id: piece.id.clone(), part: piece.part, offset: usize::MAX });
    }
    let first = shown.iter().min_by(|a, b| a.1.min_y().total_cmp(&b.1.min_y()).then(a.1.min_x().total_cmp(&b.1.min_x())))?;
    Some(Spot { id: first.0.id.clone(), part: first.0.part, offset: 0 })
}

fn distance_x(r: &LogicalRect, x: f32) -> f32 {
    if x < r.min_x() {
        r.min_x() - x
    } else if x > r.max_x() {
        x - r.max_x()
    } else {
        0.0
    }
}

/// The byte offset in `piece` at window point `p`.
fn offset_in(window: &slint::Window, piece: &Piece, rect: &LogicalRect, p: LogicalPoint) -> Spot {
    let offset = match piece.item.downcast::<TextInput>() {
        Some(ti) if !piece.shade => {
            let adapter = WindowInner::from_pub(window).window_adapter();
            let local = LogicalPoint::new(p.x - rect.min_x(), p.y - rect.min_y());
            ti.as_pin_ref().byte_offset_for_position(local, &adapter, &piece.item).0
        }
        // Styled text goes all or nothing: whichever end is nearer.
        _ if p.x < rect.center().x => 0,
        _ => usize::MAX,
    };
    Spot { id: piece.id.clone(), part: piece.part, offset }
}

/// The rows of a list, and where each message is in it.
fn rows(list: &str) -> Option<(slint::ModelRc<MessageItem>, HashMap<String, usize>)> {
    let model = crate::ui::with(|m| if list == "t" { m.thread.clone() } else { m.messages.clone() })?;
    let index = model.iter().enumerate().map(|(i, m)| (m.id.to_string(), i)).collect();
    Some((slint::ModelRc::from(model), index))
}

/// The selection's ends in reading order: ((row, part, offset), …).
type Ends = ((usize, usize, usize), (usize, usize, usize));

fn ends(index: &HashMap<String, usize>) -> Option<Ends> {
    let (a, f) = ST.with(|s| {
        let s = s.borrow();
        (s.anchor.clone(), s.focus.clone())
    });
    let (a, f) = (a?, f?);
    let pos = |s: &Spot| Some((*index.get(&s.id)?, s.part, s.offset));
    let (a, f) = (pos(&a)?, pos(&f)?);
    Some(if a <= f { (a, f) } else { (f, a) })
}

/// The byte range of piece (row, part) of length `len` the selection covers.
fn covered(ends: &Ends, row: usize, part: usize, len: usize) -> Option<(usize, usize)> {
    let (start, end) = *ends;
    let at = (row, part);
    if at < (start.0, start.1) || at > (end.0, end.1) {
        return None;
    }
    let from = if at == (start.0, start.1) { start.2.min(len) } else { 0 };
    let to = if at == (end.0, end.1) { end.2.min(len) } else { len };
    (from < to).then_some((from, to))
}

/// Shows the selection on every piece, and clears it from the rest.
fn paint(ui: &AppWindow, pieces: &[Piece]) {
    let list = ST.with(|s| s.borrow().list.clone());
    let Some((_, index)) = rows(&list) else { return };
    let ends = ends(&index);
    let mut touched = Vec::new();
    let mut shaded = std::collections::HashSet::new();
    for p in pieces {
        let range = match (&ends, index.get(&p.id)) {
            (Some(e), Some(&row)) if p.list == list => {
                let len = p.item.downcast::<TextInput>().map_or(usize::MAX, |t| t.as_pin_ref().text().len());
                covered(e, row, p.part, len)
            }
            _ => None,
        };
        if p.shade {
            if range.is_some() {
                shaded.insert(format!("{}|{}|{}", p.list, p.id, p.part));
            }
        } else {
            set_piece(p, range);
            if range.is_some() {
                touched.push(p.item.clone());
            }
        }
    }
    ST.with(|s| s.borrow_mut().touched = touched);
    set_shaded(ui, shaded);
}

fn set_shaded(ui: &AppWindow, shaded: std::collections::HashSet<String>) {
    let changed = SHADED.with(|s| {
        let mut s = s.borrow_mut();
        let changed = *s != shaded;
        *s = shaded;
        changed
    });
    if changed {
        let t = ui.global::<TextTools>();
        t.set_shade_version(t.get_shade_version().wrapping_add(1));
    }
}

fn set_piece(p: &Piece, range: Option<(usize, usize)>) {
    if let Some(t) = p.item.downcast::<TextInput>() {
        let t = t.as_pin_ref();
        let (from, to) = range.map_or((0, 0), |(a, b)| (a as i32, b as i32));
        if t.anchor_position_byte_offset() != from {
            t.anchor_position_byte_offset.set(from);
        }
        if t.cursor_position_byte_offset() != to {
            t.cursor_position_byte_offset.set(to);
        }
    }
}

/// Removes the dragged selection.
fn clear(ui: &AppWindow) {
    let touched = ST.with(|s| {
        let mut s = s.borrow_mut();
        s.active = false;
        s.anchor = None;
        s.focus = None;
        s.scroller = None;
        std::mem::take(&mut s.touched)
    });
    for item in touched {
        let desc = item.accessible_string_property(AccessibleStringProperty::Description).unwrap_or_default();
        if let Some((list, id, part, shade)) = parse_mark(&desc) {
            set_piece(&Piece { item, list, id, part, shade }, None);
        }
    }
    set_shaded(ui, Default::default());
    ui.global::<TextTools>().set_spanning(false);
}

/// The selected text: names on their own line, then the message text.
fn selected_text() -> Option<String> {
    let (active, list) = ST.with(|s| {
        let s = s.borrow();
        (s.active, s.list.clone())
    });
    if !active {
        return None;
    }
    let (model, index) = rows(&list)?;
    let e = ends(&index)?;
    let mut out: Vec<String> = Vec::new();
    for row in e.0.0..=e.1.0 {
        let Some(m) = model.row_data(row) else { continue };
        if m.notice {
            continue;
        }
        let mut lines = Vec::new();
        let mut take = |part: usize, text: &str| {
            if let Some((a, b)) = covered(&e, row, part, text.len()) {
                lines.push(text.get(a..b).unwrap_or(text).to_owned());
            }
        };
        if m.first {
            take(0, &m.name);
        }
        if m.media == 0 && m.big_emoji.row_count() == 0 {
            for (i, b) in m.blocks.iter().enumerate() {
                let b: Block = b;
                take(i + 1, &b.plain);
            }
        }
        if !lines.is_empty() {
            out.push(lines.join("\n"));
        }
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// Drags a selection from one window point to another without a mouse
/// (for driving the app in tests).
#[doc(hidden)]
pub fn drag_for_tests(ui: &AppWindow, from: (f32, f32), to: (f32, f32)) -> Option<String> {
    let window = ui.window();
    let at = LogicalPoint::new(from.0, from.1);
    ST.with(|s| s.borrow_mut().mouse = at);
    begin(ui, window, at);
    ST.with(|s| s.borrow_mut().mouse = LogicalPoint::new(to.0, to.1));
    extend(ui, window);
    ST.with(|s| {
        let mut s = s.borrow_mut();
        s.dragging = false;
        s.scroller = None;
    });
    selected_text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_marks() {
        assert_eq!(parse_mark("sel|m|$a|b:c|2"), Some(("m".into(), "$a|b:c".into(), 2, false)));
        assert_eq!(parse_mark("selbox|t|$x|1"), Some(("t".into(), "$x".into(), 1, true)));
        assert_eq!(parse_mark("Message"), None);
    }

    #[test]
    fn covers_from_one_message_to_another() {
        let e = ((2, 1, 5), (4, 0, 3));
        assert_eq!(covered(&e, 1, 1, 10), None);
        assert_eq!(covered(&e, 2, 1, 10), Some((5, 10)));
        assert_eq!(covered(&e, 2, 2, 10), Some((0, 10)));
        assert_eq!(covered(&e, 3, 0, 4), Some((0, 4)));
        assert_eq!(covered(&e, 4, 0, 10), Some((0, 3)));
        assert_eq!(covered(&e, 4, 1, 10), None);
        // An end past the text ("its end") stops at the text.
        assert_eq!(covered(&((0, 1, 2), (0, 1, usize::MAX)), 0, 1, 6), Some((2, 6)));
    }
}
