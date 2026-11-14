//! The Settings dialog: app-wide options (not per profile), in tabs like the main window.
//! Only stock controls; each check box names itself, and OK / Cancel answer Enter / Escape.

use std::cell::RefCell;
use std::rc::Rc;

use wxdragon::prelude::*;

use super::capture::{self, Answer};
use crate::profile::{Settings, UpdateChannel};
use crate::shortcuts::{self, Action, Shortcuts};

/// What the user chose with OK.
pub struct Edited {
    pub settings: Settings,
    /// "Start automatically after logging in to Windows". Kept by Windows, not in `Settings`.
    pub start_at_login: bool,
}

/// Shows the dialog; the edited settings when the user pressed OK. `profiles` are the profile
/// names, each of which gets a "switch to" shortcut. `start_at_login` is whether Windows starts
/// the app at login now.
pub fn show(parent: &Frame, current: &Settings, profiles: &[String], start_at_login: bool) -> Option<Edited> {
    let dialog = Dialog::builder(parent, "Settings").build();
    let notebook = Notebook::builder(&dialog).build();

    // ---- General
    let general = Panel::builder(&notebook).build();
    let at_login = CheckBox::builder(&general).with_label("Start &automatically after logging in to Windows").build();
    at_login.set_value(start_at_login);
    let start_in_tray = CheckBox::builder(&general)
        .with_label("Start &hidden, with only the notification-area icon")
        .build();
    start_in_tray.set_value(current.start_in_tray);
    let exit_on_close = CheckBox::builder(&general)
        .with_label("&Exit when the window is closed (Alt+F4) instead of hiding to the notification area")
        .build();
    exit_on_close.set_value(current.exit_on_close);
    // The first choice is "whichever was open last"; the rest are the profiles by name.
    let startup_label = StaticText::builder(&general).with_label("&Profile to open at launch:").build();
    let mut startup_choices = vec!["The last one used".to_string()];
    startup_choices.extend(profiles.iter().cloned());
    let startup_profile = Choice::builder(&general).with_choices(startup_choices).build();
    startup_profile.set_name("Profile to open at launch");
    // A profile deleted since it was chosen shows as "the last one used", which is what launch does.
    let startup_index = profiles.iter().position(|p| *p == current.startup_profile).map_or(0, |i| i + 1);
    startup_profile.set_selection(startup_index as u32);
    let edge = SizerFlag::Left | SizerFlag::Right | SizerFlag::Top;
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&at_login, 0, edge, 8);
    sizer.add(&start_in_tray, 0, edge, 8);
    sizer.add(&exit_on_close, 0, edge, 8);
    sizer.add(&startup_label, 0, edge, 8);
    sizer.add(&startup_profile, 0, SizerFlag::All | SizerFlag::Expand, 8);
    general.set_sizer(sizer, true);
    notebook.add_page(&general, "General", true, None);

    // ---- Keyboard shortcuts
    let keys = Panel::builder(&notebook).build();
    let model = Rc::new(RefCell::new(current.shortcuts.clone()));
    let opener = shortcuts_page(&dialog, &keys, &model, profiles);
    notebook.add_page(&keys, "Keyboard shortcuts", false, None);

    // ---- Updates
    let updates = Panel::builder(&notebook).build();
    let check_at_startup = CheckBox::builder(&updates)
        .with_label("Check for updates at &startup, and offer to install a new version")
        .build();
    check_at_startup.set_value(current.check_updates_at_startup);
    let channel_label = StaticText::builder(&updates).with_label("Update &channel:").build();
    let channel = Choice::builder(&updates)
        .with_choices(UpdateChannel::ALL.iter().map(|c| c.label().to_string()).collect())
        .build();
    channel.set_name("Update channel");
    let selected = UpdateChannel::ALL.iter().position(|c| *c == current.update_channel).unwrap_or(0);
    channel.set_selection(selected as u32);
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&check_at_startup, 0, edge, 8);
    sizer.add(&channel_label, 0, edge, 8);
    sizer.add(&channel, 0, SizerFlag::All | SizerFlag::Expand, 8);
    updates.set_sizer(sizer, true);
    notebook.add_page(&updates, "Updates", false, None);

    let buttons = StdDialogButtonSizerBuilder::new().build();
    let ok = Button::builder(&dialog).with_id(ID_OK).with_label("OK").build();
    let cancel = Button::builder(&dialog).with_id(ID_CANCEL).with_label("Cancel").build();
    buttons.add_button(&ok);
    buttons.add_button(&cancel);
    buttons.realize();
    ok.set_default();

    let outer = BoxSizer::builder(Orientation::Vertical).build();
    outer.add(&notebook, 1, SizerFlag::All | SizerFlag::Expand, 8);
    outer.add_sizer(&buttons, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom | SizerFlag::Expand, 8);
    dialog.set_sizer_and_fit(outer, true);
    dialog.centre();
    notebook.set_focus();

    let accepted = dialog.show_modal() == ID_OK;
    let settings = accepted.then(|| Settings {
        start_in_tray: start_in_tray.get_value(),
        exit_on_close: exit_on_close.get_value(),
        startup_profile: startup_profile
            .get_selection()
            .and_then(|i| (i as usize).checked_sub(1))
            .and_then(|i| profiles.get(i).cloned())
            .unwrap_or_default(),
        update_channel: channel
            .get_selection()
            .and_then(|i| UpdateChannel::ALL.get(i as usize).copied())
            .unwrap_or_default(),
        check_updates_at_startup: check_at_startup.get_value(),
        shortcuts: model.borrow().clone(),
        ..current.clone()
    });
    let result = settings.map(|settings| Edited { settings, start_at_login: at_login.get_value() });
    opener.stop();
    dialog.destroy();
    result
}

/// The Keyboard shortcuts tab: every action with its shortcut, one per row, and buttons to set
/// or remove the selected one's. Enter on the list sets, Delete removes. Returns the timer that
/// opens the shortcut dialog, which has to live as long as the dialog.
fn shortcuts_page(dialog: &Dialog, page: &Panel, model: &Rc<RefCell<Shortcuts>>, profiles: &[String]) -> Rc<Timer<Dialog>> {
    let dialog = *dialog;
    let actions = Rc::new(Action::all(profiles));
    let profiles: Rc<Vec<String>> = Rc::new(profiles.to_vec());
    let row = {
        let model = model.clone();
        move |action: &Action| {
            let chord = shortcuts::chord_of(&model.borrow(), action);
            let shown = chord.map_or_else(|| "no shortcut".to_string(), |c| c.to_display_string());
            format!("{}: {shown}", action.label())
        }
    };
    let intro = StaticText::builder(page)
        .with_label(
            "These work in every program, even with this window hidden. \
             Your screen reader says what each one did.",
        )
        .build();
    intro.wrap(dialog.from_dip_int(460));
    let list_label = StaticText::builder(page).with_label("&Actions:").build();
    let list = ListBox::builder(page).build();
    list.set_name("Actions");
    for action in actions.iter() {
        list.append(&row(action));
    }
    list.set_selection(0, true);
    let set = Button::builder(page).with_label("&Set shortcut\u{2026}").build();
    let remove = Button::builder(page).with_label("&Remove shortcut").build();

    // Rows are rewritten in place, so the selection and the screen reader's place survive.
    let refresh = {
        let actions = actions.clone();
        move || {
            for (index, action) in actions.iter().enumerate() {
                list.set_string(index as u32, &row(action));
            }
        }
    };
    let selected = {
        let actions = actions.clone();
        move || list.get_selection().and_then(|i| actions.get(i as usize).cloned())
    };
    let set_selected = {
        let (model, refresh, selected) = (model.clone(), refresh.clone(), selected.clone());
        move || {
            let Some(action) = selected() else { return };
            let current = shortcuts::chord_of(&model.borrow(), &action);
            let chord = match capture::prompt(&dialog, &action.label(), current.as_ref()) {
                Answer::Cancelled => return,
                Answer::Cleared => None,
                Answer::Chord(chord) => Some(chord),
            };
            if let Some(chord) = &chord {
                if !shortcuts::has_modifier(chord) {
                    MessageDialog::builder(
                        &dialog,
                        &format!(
                            "{} can't be used. A shortcut that works in every program needs Ctrl, Alt or Win, \
                             or it would take that key from all of them.",
                            chord.to_display_string()
                        ),
                        "Shortcut not allowed",
                    )
                    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
                    .build()
                    .show_modal();
                    return;
                }
                let holder = shortcuts::holder(&model.borrow(), &profiles, chord, &action);
                if let Some(other) = holder {
                    let answer = MessageDialog::builder(
                        &dialog,
                        &format!(
                            "{} is already the shortcut for \u{201c}{}\u{201d}. Move it to \u{201c}{}\u{201d}?",
                            chord.to_display_string(),
                            other.label(),
                            action.label()
                        ),
                        "Shortcut in use",
                    )
                    .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
                    .build()
                    .show_modal();
                    if answer != ID_YES {
                        return;
                    }
                    shortcuts::set(&mut model.borrow_mut(), &other, None);
                }
            }
            shortcuts::set(&mut model.borrow_mut(), &action, chord.as_ref());
            refresh();
            list.set_focus();
        }
    };
    let remove_selected = {
        let (model, refresh) = (model.clone(), refresh.clone());
        move || {
            let Some(action) = selected() else { return };
            shortcuts::set(&mut model.borrow_mut(), &action, None);
            refresh();
        }
    };

    {
        let set_selected = set_selected.clone();
        set.on_click(move |_| set_selected());
    }
    {
        let set_selected = set_selected.clone();
        list.on_item_double_clicked(move |_| set_selected());
    }
    {
        let remove_selected = remove_selected.clone();
        remove.on_click(move |_| remove_selected());
    }
    // Enter on the list would press OK, the dialog's default button: catch it first. The
    // shortcut dialog then opens from a timer, not inside the keyboard hook that raised the key.
    let opener = Rc::new(Timer::new(&dialog));
    opener.on_tick(move |_| set_selected());
    let opener_ref = opener.clone();
    dialog.on_char_hook(move |event| {
        let WindowEventData::Keyboard(ref key) = event else { return };
        let plain = !key.control_down() && !key.alt_down() && !key.shift_down();
        match key.get_key_code() {
            Some(WXK_RETURN | WXK_NUMPAD_ENTER) if plain && list.has_focus() => {
                event.skip(false);
                opener_ref.start(1, true);
            }
            Some(WXK_DELETE | WXK_NUMPAD_DELETE) if plain && list.has_focus() => {
                event.skip(false);
                remove_selected();
            }
            _ => event.skip(true),
        }
    });

    let edge = SizerFlag::Left | SizerFlag::Right | SizerFlag::Top;
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    buttons.add(&set, 0, SizerFlag::Right, 8);
    buttons.add(&remove, 0, SizerFlag::Right, 8);
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&intro, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&list_label, 0, edge, 8);
    sizer.add(&list, 1, SizerFlag::All | SizerFlag::Expand, 8);
    sizer.add_sizer(&buttons, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom, 8);
    page.set_sizer(sizer, true);
    page.set_min_size(Size::new(dialog.from_dip_int(480), dialog.from_dip_int(320)));
    opener
}
