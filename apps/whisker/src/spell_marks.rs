//! The spell check on screen: red wavy lines under misspelled words in the
//! message box, and the suggestions in its right-click menu.
//!
//! Typing only restarts a short timer, so nothing slows typing down: the
//! words are checked (a few lookups in the word list) and the lines placed
//! once typing pauses. The places come from Slint's own text layout, so
//! they follow wrapping and scrolling.

use crate::{AppWindow, SpellMark, spell};
use i_slint_core::item_tree::{ItemRc, ItemWeak};
use i_slint_core::items::{TextCursorAffinity, TextInput};
use i_slint_core::lengths::{LogicalPoint, LogicalRect};
#[allow(unused_imports)]
use i_slint_core::renderer::RendererSealed;
#[allow(unused_imports)]
use i_slint_core::window::{WindowAdapter, WindowInner};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long typing pauses before words are checked.
const PAUSE: Duration = Duration::from_millis(700);

static ON: AtomicBool = AtomicBool::new(true);

#[derive(Default)]
struct State {
    timer: slint::Timer,
    /// The message box, once found.
    composer: Option<ItemWeak>,
    /// The word right-clicked: its place in the draft, and the word.
    clicked: Option<(usize, usize, String)>,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

/// Turns the spell check on or off.
pub fn set_on(ui: &AppWindow, on: bool) {
    ON.store(on, Ordering::Relaxed);
    if on {
        check(ui);
    } else {
        ui.set_spell_marks(ModelRc::default());
    }
}

/// The draft changed (or the box was resized): hides the lines, which may
/// no longer fit the words, and checks again once typing pauses.
pub fn soon(ui: &AppWindow) {
    if ui.get_spell_marks().row_count() > 0 {
        ui.set_spell_marks(ModelRc::default());
    }
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let weak = ui.as_weak();
    ST.with(|s| {
        s.borrow()
            .timer
            .start(slint::TimerMode::SingleShot, PAUSE, move || {
                if let Some(ui) = weak.upgrade() {
                    check(&ui);
                }
            })
    });
}

/// The message box's text item, found by its mark in `app.slint`.
fn composer(ui: &AppWindow) -> Option<ItemRc> {
    if let Some(item) = ST.with(|s| s.borrow().composer.as_ref().and_then(ItemWeak::upgrade)) {
        return Some(item);
    }
    let root = ItemRc::new_root(WindowInner::from_pub(ui.window()).component());
    let mut found = None;
    root.visit_descendants(|item| {
        if item.downcast::<TextInput>().is_some()
            && item
                .accessible_string_property(
                    i_slint_core::accessibility::AccessibleStringProperty::Description,
                )
                .is_some_and(|d| d == "composer")
        {
            found = Some(item.clone());
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    ST.with(|s| s.borrow_mut().composer = found.as_ref().map(ItemRc::downgrade));
    found
}

/// Checks the draft and places a line under each misspelled word.
fn check(ui: &AppWindow) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let Some(item) = composer(ui) else { return };
    let Some(ti) = item.downcast::<TextInput>() else {
        return;
    };
    let ti = ti.as_pin_ref();
    let text = ti.text();
    if text.trim().is_empty() {
        ui.set_spell_marks(ModelRc::default());
        return;
    }
    // Typing has paused, so every word counts, the last one too (lines are
    // hidden again as soon as typing starts).
    let bad = spell::misspelled(&text, None);
    tracing::debug!(words = bad.len(), "spell check");
    let adapter = WindowInner::from_pub(ui.window()).window_adapter();
    let renderer = adapter.renderer();
    let at = |offset: usize, affinity| {
        renderer.text_input_cursor_rect_for_byte_offset(ti, &item, offset, affinity)
    };
    let (width, height) = (item.geometry().width(), item.geometry().height());
    let mut marks = Vec::new();
    let mut line = |x: f32, right: f32, r: LogicalRect| {
        // Scrolled out of the box.
        if r.min_y() < -1.0 || r.max_y() > height + 1.0 {
            return;
        }
        let w = (right - x).max(2.0);
        marks.push(SpellMark {
            x: x.into(),
            y: (r.max_y() - 2.0).into(),
            w: w.into(),
            path: spell::squiggle(w).into(),
        });
    };
    for (start, end) in bad {
        let a = at(start, TextCursorAffinity::NextCharacter);
        let b = at(end, TextCursorAffinity::PreviousCharacter);
        if (a.min_y() - b.min_y()).abs() < 1.0 {
            line(a.min_x(), b.min_x(), a);
        } else {
            // Wrapped across two lines.
            line(a.min_x(), width, a);
            line(0.0, b.min_x(), b);
        }
    }
    tracing::debug!(box_w = width, box_h = height, marks = ?marks.iter().map(|m| (m.x, m.y, m.w)).collect::<Vec<_>>(), "spell lines");
    ui.set_spell_marks(ModelRc::new(VecModel::from(marks)));
}

/// Before the message box's menu opens: the misspelled word under the
/// mouse (window point `p`), and its suggestions.
pub fn right_click(ui: &AppWindow, p: LogicalPoint) {
    let mut picked = None;
    if ON.load(Ordering::Relaxed)
        && let Some(item) = composer(ui)
        && let Some(ti) = item.downcast::<TextInput>()
    {
        let g = item.geometry();
        let origin = item.map_to_window(g.origin);
        let rect = LogicalRect::new(origin, g.size);
        if rect.contains(p) {
            let adapter = WindowInner::from_pub(ui.window()).window_adapter();
            let local = LogicalPoint::new(p.x - origin.x, p.y - origin.y);
            let offset = ti
                .as_pin_ref()
                .byte_offset_for_position(local, &adapter, &item)
                .0;
            let text = ti.as_pin_ref().text();
            picked = spell::misspelled_at(&text, offset)
                .map(|(s, e)| (s, e, text.as_str()[s..e].to_owned()));
        }
    }
    let (word, suggestions) = match &picked {
        Some((_, _, w)) => (w.clone(), spell::suggestions(w)),
        None => (String::new(), Vec::new()),
    };
    ui.set_spell_word(word.into());
    ui.set_spell_suggestions(ModelRc::new(VecModel::from(
        suggestions
            .into_iter()
            .map(Into::into)
            .collect::<Vec<slint::SharedString>>(),
    )));
    ST.with(|s| s.borrow_mut().clicked = picked);
}

/// Puts `with` in place of the word right-clicked.
pub fn replace(ui: &AppWindow, with: &str) {
    let Some((start, end, word)) = ST.with(|s| s.borrow_mut().clicked.take()) else {
        return;
    };
    let draft = ui.get_draft().to_string();
    if draft.get(start..end) != Some(word.as_str()) {
        return;
    }
    let new = format!("{}{with}{}", &draft[..start], &draft[end..]);
    ui.set_draft(new.as_str().into());
    ui.set_cursor_at((start + with.len()) as i32);
    ui.set_cursor_request(ui.get_cursor_request() + 1);
    ui.invoke_draft_edited(new.into());
}

/// The word right-clicked, for "Add to dictionary".
pub fn take_clicked_word() -> Option<String> {
    ST.with(|s| s.borrow_mut().clicked.take())
        .map(|(_, _, w)| w)
}

/// Looks again now (after a word was added to the dictionary).
pub fn recheck(ui: &AppWindow) {
    check(ui);
}
