//! The dialog that asks for one shortcut: Ctrl, Alt, Shift and Win check boxes and a field that
//! records the keys pressed in it.
//!
//! Adapted from wx-utils' Set Shortcut dialog (MIT), whose own copy isn't public. Differences:
//! the Win box is always there (every shortcut here is global), the captured chord is spoken
//! through the screen reader with `speech`, keys still held from opening the dialog are ignored,
//! and Tab, Escape, Enter and the arrows are only let through when pressed without Ctrl, Alt or
//! Shift, so Ctrl+Alt+Left can still be a shortcut.
//!
//! wx-utils (https://github.com/accessware/wx-utils) is under the MIT License; its notice:

// MIT License
//
// Copyright (c) 2026 Quin Gillespie
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::cell::RefCell;
use std::rc::Rc;

use key_chord::KeyChord;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use wxdragon::prelude::*;

use crate::speech;

const ID_CLEAR: i32 = ID_HIGHEST + 200;

/// Keys that keep the dialog working when pressed alone: Tab, Escape, Enter (twice: main and
/// numpad), and the arrows (main and numpad), which a screen reader uses to read the field.
const RESERVED_ALONE: [i32; 11] = [9, 27, 13, 370, 314, 315, 316, 317, 376, 377, 378];

/// What the user chose.
pub enum Answer {
    Cancelled,
    Cleared,
    Chord(KeyChord),
}

fn key_is_down(code: i32) -> bool {
    // Letters, digits, Enter and Space have the same code in wx as their virtual key.
    (8..=0xFE).contains(&code) && unsafe { GetAsyncKeyState(code) } as u16 & 0x8000 != 0
}

pub fn prompt(parent: &Dialog, action: &str, initial: Option<&KeyChord>) -> Answer {
    let dialog = Dialog::builder(parent, &format!("Shortcut for {action}")).build();
    let info = StaticText::builder(&dialog)
        .with_label(&format!(
            "Press the keys for \u{201c}{action}\u{201d} in the Key field. It works in every program, so use Ctrl, Alt or Win with it."
        ))
        .build();
    info.wrap(dialog.from_dip_int(420));
    let ctrl = CheckBox::builder(&dialog).with_label("&Ctrl").build();
    let alt = CheckBox::builder(&dialog).with_label("&Alt").build();
    let shift = CheckBox::builder(&dialog).with_label("&Shift").build();
    let win = CheckBox::builder(&dialog).with_label("&Win").build();
    let key_label = StaticText::builder(&dialog).with_label("&Key:").build();
    // Multi-line, or Tab skips a read-only field.
    let key = TextCtrl::builder(&dialog)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::DontWrap)
        .build();
    key.set_name("Key");
    let hint = StaticText::builder(&dialog)
        .with_label("Windows keeps most Win combinations for itself, so tick Win rather than pressing it.")
        .build();
    hint.wrap(dialog.from_dip_int(420));
    if let Some(chord) = initial {
        ctrl.set_value(chord.ctrl || chord.raw_ctrl);
        alt.set_value(chord.alt);
        shift.set_value(chord.shift);
        win.set_value(chord.win);
        key.set_value(&chord.key);
    }

    // Keys still down from opening the dialog (Enter on the button, or the S of Alt+S) would
    // otherwise auto-repeat into the field.
    let held: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(
        (0x08..=0xFE).filter(|&code| !(0x10..=0x12).contains(&code) && key_is_down(code)).collect(),
    ));
    key.on_key_down(move |event| {
        let WindowEventData::Keyboard(ref k) = event else { return };
        let Some(code) = k.get_key_code() else {
            event.skip(true);
            return;
        };
        {
            let mut held = held.borrow_mut();
            if held.contains(&code) {
                if key_is_down(code) {
                    event.skip(false);
                    return;
                }
                held.retain(|&c| c != code);
            }
        }
        let modified = k.control_down() || k.alt_down() || k.shift_down();
        if !modified && RESERVED_ALONE.contains(&code) {
            // A read-only multi-line field would swallow Enter and Escape: act on them here.
            match code {
                13 | 370 => {
                    event.skip(false);
                    dialog.end_modal(ID_OK);
                }
                27 => {
                    event.skip(false);
                    dialog.end_modal(ID_CANCEL);
                }
                _ => event.skip(true),
            }
            return;
        }
        // Shift+Tab is how to get back out of the field.
        if code == 9 && k.shift_down() && !k.control_down() && !k.alt_down() {
            event.skip(true);
            return;
        }
        match KeyChord::from_key_code(code, k.control_down(), k.alt_down(), k.shift_down()) {
            Some(chord) => {
                ctrl.set_value(chord.ctrl);
                alt.set_value(chord.alt);
                shift.set_value(chord.shift);
                key.set_value(&chord.key);
                let shown = chord.with_win(win.get_value()).to_display_string();
                speech::say(&shown);
                event.skip(false);
            }
            // A modifier on its own, on the way to a chord.
            None => event.skip(true),
        }
    });

    let ok = Button::builder(&dialog).with_id(ID_OK).with_label("OK").build();
    let cancel = Button::builder(&dialog).with_id(ID_CANCEL).with_label("Cancel").build();
    let clear = Button::builder(&dialog).with_id(ID_CLEAR).with_label("&Remove shortcut").build();
    clear.on_click(move |_| dialog.end_modal(ID_CLEAR));
    let buttons = StdDialogButtonSizerBuilder::new().build();
    buttons.add_button(&ok);
    buttons.add_button(&cancel);
    buttons.realize();
    ok.set_default();

    let modifiers = BoxSizer::builder(Orientation::Horizontal).build();
    for check in [ctrl, alt, shift, win] {
        modifiers.add(&check, 0, SizerFlag::Right, 12);
    }
    let key_row = BoxSizer::builder(Orientation::Horizontal).build();
    key_row.add(&key_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::Right, 8);
    key_row.add(&key, 1, SizerFlag::Expand, 0);
    let footer = BoxSizer::builder(Orientation::Horizontal).build();
    footer.add(&clear, 0, SizerFlag::AlignCenterVertical, 0);
    footer.add_stretch_spacer(1);
    footer.add_sizer(&buttons, 0, SizerFlag::Expand, 0);
    let outer = BoxSizer::builder(Orientation::Vertical).build();
    outer.add(&info, 0, SizerFlag::All | SizerFlag::Expand, 8);
    outer.add_sizer(&modifiers, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom, 8);
    outer.add_sizer(&key_row, 0, SizerFlag::All | SizerFlag::Expand, 8);
    outer.add(&hint, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Expand, 8);
    outer.add_sizer(&footer, 0, SizerFlag::All | SizerFlag::Expand, 8);
    dialog.set_sizer_and_fit(outer, true);
    dialog.centre();
    key.set_focus();

    let answer = match dialog.show_modal() {
        ID_CLEAR => Answer::Cleared,
        ID_OK => {
            let text = key.get_value();
            let text = text.trim();
            if text.is_empty() {
                Answer::Cleared
            } else {
                Answer::Chord(
                    KeyChord::new(ctrl.get_value(), alt.get_value(), shift.get_value(), text).with_win(win.get_value()),
                )
            }
        }
        _ => Answer::Cancelled,
    };
    dialog.destroy();
    answer
}
