//! The notification-area icon and its menu: status, show the window, toggle sending and
//! receiving, switch profile, exit.
//!
//! wx never sends NIM_SETVERSION, so keyboard activation (Windows+B, then Enter or Space)
//! arrives as emulated clicks: both left-up and right-up open the menu. The menu is built
//! fresh each time, and its command runs after `popup_menu` returns, outside any borrow.

use std::cell::Cell;
use std::rc::Rc;

use wxdragon::prelude::*;

use super::{Shared, Ui, escape_mnemonic, load_profile_and_say, quit, set_receiving, set_sending, show_window};
use crate::engine::Health;
use crate::gui::text::plural;

const TRAY_SHOW: i32 = ID_HIGHEST + 50;
const TRAY_SEND: i32 = ID_HIGHEST + 51;
const TRAY_RECEIVE: i32 = ID_HIGHEST + 52;
const TRAY_EXIT: i32 = ID_HIGHEST + 53;
const TRAY_STATUS: i32 = ID_HIGHEST + 54;
const TRAY_PROFILE_BASE: i32 = ID_HIGHEST + 100;

/// A plain round icon: a teal disc with a lighter centre, drawn at 32 × 32.
fn icon() -> Bitmap {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let distance = (dx * dx + dy * dy).sqrt();
            let (r, g, b, a) = if distance > 15.0 {
                (0, 0, 0, 0)
            } else if distance < 6.0 {
                (230, 250, 250, 255)
            } else {
                (0, 128, 128, 255)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    Bitmap::from_rgba(&rgba, size, size).expect("building the tray icon")
}

/// One line on what is happening, for the tooltip and the first menu item.
fn summary(state: &Shared) -> String {
    let st = state.borrow();
    let snapshot = &st.snapshot;
    let connected = snapshot.peers.iter().filter(|p| matches!(p.health, Health::Healthy | Health::Stale)).count();
    let peers = if snapshot.peers.is_empty() {
        "no peers ticked".to_string()
    } else {
        format!("connected to {connected} of {}", plural(snapshot.peers.len(), "peer", "peers"))
    };
    let send = if !st.profile.send_enabled {
        "not sending"
    } else if snapshot.sending {
        "sending"
    } else {
        "nothing sent"
    };
    let receive = if st.profile.receive_enabled { "receiving" } else { "not receiving" };
    format!("{} profile: {peers}; {send}; {receive}", st.profile_name)
}

pub fn update_tooltip(state: &Shared) {
    let line = summary(state);
    let tooltip: String = format!("RemSoundLite, {line}").chars().take(127).collect();
    let mut st = state.borrow_mut();
    if st.tooltip != tooltip {
        st.tooltip = tooltip.clone();
        drop(st);
        TRAY.with(|t| {
            if let Some(tray) = t.get() {
                tray.set_icon(&icon(), &tooltip);
            }
        });
    }
}

thread_local! {
    static TRAY: Cell<Option<&'static TaskBarIcon>> = const { Cell::new(None) };
}

pub fn create(ui: Ui, state: &Shared, timer: &'static Timer<Frame>) -> &'static TaskBarIcon {
    let tray: &'static TaskBarIcon =
        Box::leak(Box::new(TaskBarIcon::builder().with_icon(icon()).with_tooltip("RemSoundLite").build()));
    TRAY.with(|t| t.set(Some(tray)));
    let chosen: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    {
        let chosen = chosen.clone();
        tray.on_menu(move |event: Event| chosen.set(Some(event.get_id())));
    }
    let open = {
        let state = state.clone();
        move || {
            let names = state.borrow().store.names();
            let mut menu = build_menu(&state, &names);
            chosen.set(None);
            tray.popup_menu(&mut menu);
            if let Some(id) = chosen.take() {
                run_command(ui, &state, tray, timer, &names, id);
            }
        }
    };
    let open_right = open.clone();
    tray.on_left_up(move |_| open());
    tray.on_right_up(move |_| open_right());
    tray
}

fn build_menu(state: &Shared, names: &[String]) -> Menu {
    let line = escape_mnemonic(&summary(state));
    let (send, receive, current) = {
        let st = state.borrow();
        (st.profile.send_enabled, st.profile.receive_enabled, st.profile_name.clone())
    };
    let menu = Menu::builder().build();
    // Disabled, but read by screen readers: opening the menu says what's going on.
    menu.append(TRAY_STATUS, &line, "", ItemKind::Normal);
    menu.enable_item(TRAY_STATUS, false);
    menu.append(TRAY_SHOW, "&Show main window", "", ItemKind::Normal);
    menu.append_separator();
    menu.append(TRAY_SEND, "Send my &audio", "", ItemKind::Check);
    menu.check_item(TRAY_SEND, send);
    menu.append(TRAY_RECEIVE, "&Receive audio", "", ItemKind::Check);
    menu.check_item(TRAY_RECEIVE, receive);
    let profiles = Menu::builder().build();
    for (index, name) in names.iter().enumerate() {
        let id = TRAY_PROFILE_BASE + index as i32;
        profiles.append(id, &escape_mnemonic(name), "", ItemKind::Radio);
        if *name == current {
            profiles.check_item(id, true);
        }
    }
    menu.append_submenu(profiles, "&Profile", "");
    menu.append_separator();
    menu.append(TRAY_EXIT, "E&xit", "", ItemKind::Normal);
    menu
}

fn run_command(ui: Ui, state: &Shared, tray: &'static TaskBarIcon, timer: &'static Timer<Frame>, names: &[String], id: i32) {
    match id {
        TRAY_SHOW => show_window(ui),
        // The menu has closed and nothing on screen changed: these say what happened.
        TRAY_SEND => {
            let value = !state.borrow().profile.send_enabled;
            set_sending(ui, state, value);
        }
        TRAY_RECEIVE => {
            let value = !state.borrow().profile.receive_enabled;
            set_receiving(ui, state, value);
        }
        TRAY_EXIT => quit(ui, state, tray, timer),
        id if id >= TRAY_PROFILE_BASE => {
            if let Some(name) = names.get((id - TRAY_PROFILE_BASE) as usize) {
                load_profile_and_say(ui, state, name);
            }
        }
        _ => {}
    }
}
