//! The Settings dialog: app-wide options (not per profile), in tabs like the main window.
//! Only stock controls; each check box names itself, and OK / Cancel answer Enter / Escape.

use wxdragon::prelude::*;

use crate::profile::Settings;

/// Shows the dialog; the edited settings when the user pressed OK.
pub fn show(parent: &Frame, current: &Settings) -> Option<Settings> {
    let dialog = Dialog::builder(parent, "Settings").build();
    let notebook = Notebook::builder(&dialog).build();

    // ---- General
    let general = Panel::builder(&notebook).build();
    let start_in_tray = CheckBox::builder(&general)
        .with_label("Start &hidden, with only the notification-area icon")
        .build();
    start_in_tray.set_value(current.start_in_tray);
    let exit_on_close = CheckBox::builder(&general)
        .with_label("&Exit when the window is closed (Alt+F4) instead of hiding to the notification area")
        .build();
    exit_on_close.set_value(current.exit_on_close);
    let edge = SizerFlag::Left | SizerFlag::Right | SizerFlag::Top;
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&start_in_tray, 0, edge, 8);
    sizer.add(&exit_on_close, 0, SizerFlag::All, 8);
    general.set_sizer(sizer, true);
    notebook.add_page(&general, "General", true, None);

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
    start_in_tray.set_focus();

    let accepted = dialog.show_modal() == ID_OK;
    let result = accepted.then(|| Settings {
        start_in_tray: start_in_tray.get_value(),
        exit_on_close: exit_on_close.get_value(),
        ..current.clone()
    });
    dialog.destroy();
    result
}
