//! The window: two tabs (Connectivity, Audio), a File menu for profiles, the lock-screen
//! service and app settings, and a tray icon it hides to.
//!
//! Accessibility notes (see the rust-windows-accessible-gui skill):
//! * Only stock Win32 controls. Every label is created just before its control, so MSAA
//!   names the control after it, and every label carries an Alt mnemonic.
//! * Read-only fields are multi-line, so Tab reaches them.
//! * Those fields are rewritten under a focused screen reader only when something other than
//!   the ever-changing numbers changed.
//! * Check lists are never modified from inside their own toggle handler, are never labelled
//!   with `set_accessibility_label` (that drops their checked state) and get a placeholder
//!   row while empty (focusing an empty one asserts in wx).
//! * What the focus doesn't say (a tick showing up in the other list, a rename, a peer
//!   connecting or dropping, a tray toggle, a global shortcut) is spoken through the screen
//!   reader (`speech`).

mod capture;
mod settings;
mod text;
mod tray;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;

use wxdragon::prelude::*;

use crate::audio::devices::{self, DeviceList};
use crate::engine::{Engine, EngineConfig, Snapshot};
use crate::profile::{self, ARTEFACTS, Codec, PacketSize, Profile, ProfileStore, SavedDevice, SavedPeer};
use crate::protocol::discovery::DiscoveredPeer;
use crate::sysinfo::CpuMeter;
use crate::engine::Health;
use crate::service::Firewall;
use crate::shortcuts::{self, Action, KeyChord};
use crate::{presence, service, speech};
use wx_utils::global_hotkeys::GlobalHotkeys;

const ID_SAVE: i32 = ID_HIGHEST + 1;
const ID_SAVE_AS: i32 = ID_HIGHEST + 2;
const ID_SWITCH: i32 = ID_HIGHEST + 3;
const ID_DELETE: i32 = ID_HIGHEST + 4;
const ID_SERVICE_INSTALL: i32 = ID_HIGHEST + 5;
const ID_SERVICE_REMOVE: i32 = ID_HIGHEST + 6;
const ID_HIDE: i32 = ID_HIGHEST + 7;
const ID_QUIT: i32 = ID_HIGHEST + 8;
const ID_FIREWALL: i32 = ID_HIGHEST + 9;
const ID_RENAME: i32 = ID_HIGHEST + 10;
const ID_REMOVE_PEER: i32 = ID_HIGHEST + 11;
const ID_CONNECT: i32 = ID_HIGHEST + 12;
const ID_UPDATES: i32 = ID_HIGHEST + 13;
const ID_SETTINGS: i32 = ID_HIGHEST + 14;

#[derive(Clone, Copy)]
struct Ui {
    frame: Frame,
    connectivity: Panel,
    connected: CheckListBox,
    remove: Button,
    details: TextCtrl,
    discovered: CheckListBox,
    add: Button,
    auto_accept: CheckBox,
    password: TextCtrl,
    status: TextCtrl,
    send: CheckBox,
    input_sources: CheckListBox,
    output_sources: CheckListBox,
    receive: CheckBox,
    outputs: CheckListBox,
    codec: Choice,
    packet: ListBox,
    volume: Slider,
    jitter: SpinCtrl,
    auto_tune: CheckBox,
    latency: TextCtrl,
    smoothness: ListBox,
    artefact: ListBox,
}

#[derive(Clone, PartialEq)]
enum Detail {
    None,
    Connected(String),
    Candidate(Ipv4Addr),
}

/// A row of the discovered list: a computer you could connect to that isn't in your connected
/// peers, found by discovery, asking to connect (it ticked you), or both.
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    address: Ipv4Addr,
    name: String,
    /// From discovery: (sends audio, receives audio). `None` when it only asked to connect.
    offers: Option<(bool, bool)>,
    /// It has ticked you and proved it holds the password.
    wants: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerList {
    Connected,
    Discovered,
}

enum Deferred {
    RebuildConnected { select: Option<String>, focus: bool },
    Rename(PeerList),
    Connect,
    Remove,
    /// Someone ticked a placeholder row: take the tick back off.
    Uncheck(PeerList),
}

struct State {
    store: ProfileStore,
    profile_name: String,
    profile: Profile,
    saved: Profile,
    engine: Option<Engine>,
    devices: DeviceList,
    connected_labels: Vec<String>,
    discovered: Vec<DiscoveredPeer>,
    discovered_labels: Vec<String>,
    candidates: Vec<Candidate>,
    /// The addresses the discovered list's rows stand for, in order, as last built.
    candidates_shown: Vec<Ipv4Addr>,
    /// Peers whose request to connect has been announced already.
    announced_requests: HashSet<Ipv4Addr>,
    input_source_rows: Vec<SavedDevice>,
    output_source_rows: Vec<SavedDevice>,
    output_rows: Vec<SavedDevice>,
    detail: Detail,
    snapshot: Snapshot,
    last_bytes: (u64, u64, Instant),
    rates: (f64, f64),
    cpu: CpuMeter,
    details_key: String,
    status_key: String,
    latency_key: String,
    tooltip: String,
    elevated: Option<(mpsc::Receiver<Result<(), String>>, &'static str)>,
    firewall: Firewall,
    firewall_check: Option<mpsc::Receiver<Firewall>>,
    /// Whether each peer was last seen connected, for announcing changes.
    links: HashMap<String, bool>,
    /// Which list the open context menu belongs to.
    context: Option<PeerList>,
    ticks: u64,
    quitting: bool,
    /// Set from ship-shape's thread when a downloaded update is ready: quit on the next tick.
    update_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    update_notice: Option<mpsc::Receiver<String>>,
    /// "Version 1.2.0 of RemSoundLite is available." from the startup check.
    update_available: Option<String>,
    /// App-wide settings (start hidden, exit on close).
    settings: profile::Settings,
    /// The registered global shortcuts; dropping it releases them.
    hotkeys: Option<GlobalHotkeys>,
}

type Shared = Rc<RefCell<State>>;

impl State {
    fn dirty(&self) -> bool {
        self.profile != self.saved
    }

    fn apply(&self) {
        if let Some(engine) = &self.engine {
            engine.apply(EngineConfig::from_profile(&self.profile, false));
        }
    }
}

fn message(parent: &dyn WxWidget, text: &str, caption: &str, style: MessageDialogStyle) -> i32 {
    MessageDialog::builder(parent, text, caption).with_style(style).build().show_modal()
}

/// `&` is a mnemonic in labels and menu items: double it in anything that came from outside.
fn escape_mnemonic(text: &str) -> String {
    text.replace('&', "&&").replace('\t', " ")
}

pub fn run() {
    let Some(hold) = presence::acquire() else {
        already_running();
        return;
    };
    let _ = wxdragon::main(move |_| {
        // The hold lives as long as the GUI: leak it with the rest of the app's lifetime state.
        Box::leak(Box::new(hold));
        build();
    });
}

fn already_running() {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    use windows::core::w;
    unsafe {
        MessageBoxW(
            None,
            w!("RemSoundLite is already running. Look for it in the notification area (Windows+B)."),
            w!("RemSoundLite"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

// ================================================================== building

fn label<W: WxWidget>(parent: &W, text: &str) -> StaticText {
    StaticText::builder(parent).with_label(text).build()
}

fn build() {
    let frame = Frame::builder()
        .with_title("RemSoundLite")
        .with_size(Size::new(760, 900))
        .build();
    frame.set_menu_bar(
        MenuBar::builder()
            .append(
                Menu::builder()
                    .append_item(ID_SAVE, "&Save profile\tCtrl+S", "Save the settings into the current profile")
                    .append_item(ID_SAVE_AS, "Save profile &as\u{2026}\tCtrl+Shift+S", "Save the settings as a new profile")
                    .append_item(ID_SWITCH, "Switch &profile\u{2026}\tCtrl+P", "Load another profile")
                    .append_item(ID_DELETE, "&Delete profile\u{2026}", "Delete the current profile")
                    .append_separator()
                    .append_item(
                        ID_SERVICE_INSTALL,
                        "Send on the &lock screen with this profile\u{2026}",
                        "Install or update the service that sends from startup",
                    )
                    .append_item(ID_SERVICE_REMOVE, "&Remove the lock-screen service\u{2026}", "Uninstall the service")
                    .append_item(
                        ID_FIREWALL,
                        "Allow through Windows &Firewall\u{2026}",
                        "Let peers on the network reach RemSoundLite",
                    )
                    .append_separator()
                    .append_item(ID_SETTINGS, "Se&ttings\u{2026}\tCtrl+,", "Options for how RemSoundLite starts, closes and updates")
                    .append_item(ID_UPDATES, "Check for &updates\u{2026}", "Look for a newer RemSoundLite")
                    .append_separator()
                    .append_item(ID_HIDE, "&Hide to the notification area\tCtrl+H", "Hide this window")
                    .append_item(ID_QUIT, "E&xit", "Stop streaming and close RemSoundLite")
                    .build(),
                "&File",
            )
            .build(),
    );

    let notebook = Notebook::builder(&frame).build();
    let connectivity = Panel::builder(&notebook).build();
    // Scrolls, so nothing on it is ever cut off at the bottom of a small window.
    let audio = ScrolledWindow::builder(&notebook).build();
    audio.set_scroll_rate(0, 10);

    // ---- Connectivity
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let connected_label = label(&connectivity, "Connected p&eers:");
    let connected = CheckListBox::builder(&connectivity).with_size(Size::new(-1, 100)).build();
    connected.set_name("Connected peers");
    let remove = Button::builder(&connectivity).with_label("Re&move peer").build();
    let details_label = label(&connectivity, "Peer &details:");
    let details = TextCtrl::builder(&connectivity)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::DontWrap)
        .with_size(Size::new(-1, 130))
        .build();
    details.set_name("Peer details");
    let discovered_label = label(&connectivity, "D&iscovered peers:");
    let discovered = CheckListBox::builder(&connectivity).with_size(Size::new(-1, 90)).build();
    discovered.set_name("Discovered peers");
    let add = Button::builder(&connectivity).with_label("&Add peer by IP address\u{2026}").build();
    let auto_accept = CheckBox::builder(&connectivity)
        .with_label("Automatically a&ccept peers that connect to me")
        .build();
    let password_label = label(&connectivity, "Pass&word:");
    let password = TextCtrl::builder(&connectivity).with_style(TextCtrlStyle::Password).build();
    password.set_name("Password");
    let status_label = label(&connectivity, "Connection s&tatus:");
    let status = TextCtrl::builder(&connectivity)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::DontWrap)
        .with_size(Size::new(-1, 150))
        .build();
    status.set_name("Connection status");
    let edge = SizerFlag::Left | SizerFlag::Right | SizerFlag::Top;
    sizer.add(&connected_label, 0, edge, 8);
    sizer.add(&connected, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&remove, 0, edge, 8);
    sizer.add(&details_label, 0, edge, 8);
    sizer.add(&details, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&discovered_label, 0, edge, 8);
    sizer.add(&discovered, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&add, 0, edge, 8);
    sizer.add(&auto_accept, 0, edge, 8);
    sizer.add(&password_label, 0, edge, 8);
    sizer.add(&password, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&status_label, 0, edge, 8);
    sizer.add(&status, 1, SizerFlag::All | SizerFlag::Expand, 8);
    connectivity.set_sizer(sizer, true);

    // ---- Audio
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let send = CheckBox::builder(&audio).with_label("&Send my audio").build();
    let input_sources_label = label(&audio, "&Input devices to send:");
    let input_sources = CheckListBox::builder(&audio).with_size(Size::new(-1, 80)).build();
    input_sources.set_name("Input devices to send");
    let output_sources_label = label(&audio, "&Output devices to send:");
    let output_sources = CheckListBox::builder(&audio).with_size(Size::new(-1, 80)).build();
    output_sources.set_name("Output devices to send");
    let receive = CheckBox::builder(&audio).with_label("&Receive audio").build();
    let volume_label = label(&audio, "Volume of received audio, in percent (Alt+&V)");
    let volume = Slider::builder(&audio).with_min_value(0).with_max_value(100).with_value(100).build();
    volume.set_name("Volume of received audio, in percent");
    let outputs_label = label(&audio, "Play received audio o&n:");
    let outputs = CheckListBox::builder(&audio).with_size(Size::new(-1, 80)).build();
    outputs.set_name("Play received audio on");

    let send_box = StaticBox::builder(&audio).with_label("Audio send parameters").build();
    let send_sizer = StaticBoxSizerBuilder::new_with_box(&send_box, Orientation::Vertical).build();
    let codec_label = label(&send_box, "Audio codec (Alt+&C)");
    let codec = Choice::builder(&send_box)
        .with_choices(Codec::ALL.iter().map(|c| c.label().to_string()).collect())
        .build();
    codec.set_name("Audio codec");
    let packet_label = label(&send_box, "Packet size (Alt+&P)");
    let packet = ListBox::builder(&send_box)
        .with_choices(PacketSize::ALL.iter().map(|p| p.label().to_string()).collect())
        .with_size(Size::new(-1, 44))
        .build();
    packet.set_name("Packet size");
    send_sizer.add(&codec_label, 0, edge, 6);
    send_sizer.add(&codec, 0, edge | SizerFlag::Expand, 6);
    send_sizer.add(&packet_label, 0, edge, 6);
    send_sizer.add(&packet, 0, SizerFlag::All | SizerFlag::Expand, 6);

    let receive_box = StaticBox::builder(&audio).with_label("Audio receive parameters").build();
    let receive_sizer = StaticBoxSizerBuilder::new_with_box(&receive_box, Orientation::Vertical).build();
    let jitter_label = label(&receive_box, "Audio jitter buffer in milliseconds (Alt+&L)");
    let jitter = SpinCtrl::builder(&receive_box)
        .with_range(profile::MIN_JITTER_MS as i32, profile::MAX_JITTER_MS as i32)
        .build();
    jitter.set_name("Audio jitter buffer in milliseconds");
    let auto_tune = CheckBox::builder(&receive_box).with_label("Continuous auto-t&une jitter buffer").build();
    let latency_label = label(&receive_box, "Total latency (Alt+&M)");
    let latency = TextCtrl::builder(&receive_box)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::WordWrap)
        .with_size(Size::new(-1, 44))
        .build();
    latency.set_name("Total latency");
    let smoothness_label = label(&receive_box, "Buffer smoothness (Alt+&B)");
    let smoothness_items: Vec<String> = (1..=10u8).rev().map(smoothness_label_text).collect();
    let smoothness = ListBox::builder(&receive_box).with_choices(smoothness_items).with_size(Size::new(-1, 90)).build();
    smoothness.set_name("Buffer smoothness");
    let artefact_label = label(&receive_box, "Artefact sound type (Alt+&A) \u{2014} controls how audio gaps sound");
    let artefact = ListBox::builder(&receive_box)
        .with_choices(ARTEFACTS.iter().map(|(_, l)| l.to_string()).collect())
        .with_size(Size::new(-1, 44))
        .build();
    artefact.set_name("Artefact sound type");
    receive_sizer.add(&jitter_label, 0, edge, 6);
    receive_sizer.add(&jitter, 0, edge, 6);
    receive_sizer.add(&auto_tune, 0, edge, 6);
    receive_sizer.add(&latency_label, 0, edge, 6);
    receive_sizer.add(&latency, 0, edge | SizerFlag::Expand, 6);
    receive_sizer.add(&smoothness_label, 0, edge, 6);
    receive_sizer.add(&smoothness, 0, edge | SizerFlag::Expand, 6);
    receive_sizer.add(&artefact_label, 0, edge, 6);
    receive_sizer.add(&artefact, 0, SizerFlag::All | SizerFlag::Expand, 6);

    sizer.add(&send, 0, edge, 8);
    sizer.add(&input_sources_label, 0, edge, 8);
    sizer.add(&input_sources, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&output_sources_label, 0, edge, 8);
    sizer.add(&output_sources, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&receive, 0, edge, 8);
    sizer.add(&volume_label, 0, edge, 8);
    sizer.add(&volume, 0, edge | SizerFlag::Expand, 8);
    sizer.add(&outputs_label, 0, edge, 8);
    sizer.add(&outputs, 0, edge | SizerFlag::Expand, 8);
    sizer.add_sizer(&send_sizer, 0, edge | SizerFlag::Expand, 8);
    sizer.add_sizer(&receive_sizer, 0, SizerFlag::All | SizerFlag::Expand, 8);
    audio.set_sizer(sizer, true);

    notebook.add_page(&connectivity, "Connectivity", true, None);
    notebook.add_page(&audio, "Audio", false, None);
    let frame_sizer = BoxSizer::builder(Orientation::Vertical).build();
    frame_sizer.add(&notebook, 1, SizerFlag::Expand, 0);
    frame.set_sizer(frame_sizer, true);

    let ui = Ui {
        frame,
        connectivity,
        connected,
        remove,
        details,
        discovered,
        add,
        auto_accept,
        password,
        status,
        send,
        input_sources,
        output_sources,
        receive,
        outputs,
        codec,
        packet,
        volume,
        jitter,
        auto_tune,
        latency,
        smoothness,
        artefact,
    };

    // ---- State
    let store = ProfileStore::new();
    let profile_name = store.last_used().filter(|n| store.path_of(n).exists()).unwrap_or_else(|| {
        store.names().into_iter().next().unwrap_or_else(|| profile::DEFAULT_PROFILE.to_string())
    });
    let profile = match store.load(&profile_name) {
        Ok(profile) => profile,
        Err(_) => {
            let fresh = Profile::default();
            let _ = store.save(&profile_name, &fresh);
            fresh
        }
    };
    store.set_last_used(&profile_name);
    let settings = store.settings();
    let engine = Engine::start(EngineConfig::from_profile(&profile, false));
    let state: Shared = Rc::new(RefCell::new(State {
        store,
        profile_name,
        saved: profile.clone(),
        profile,
        engine: Some(engine),
        devices: devices::list(),
        connected_labels: Vec::new(),
        discovered: Vec::new(),
        discovered_labels: Vec::new(),
        candidates: Vec::new(),
        candidates_shown: Vec::new(),
        announced_requests: HashSet::new(),
        input_source_rows: Vec::new(),
        output_source_rows: Vec::new(),
        output_rows: Vec::new(),
        detail: Detail::None,
        snapshot: Snapshot::default(),
        last_bytes: (0, 0, Instant::now()),
        rates: (0.0, 0.0),
        cpu: CpuMeter::new(),
        details_key: String::new(),
        status_key: String::new(),
        latency_key: String::new(),
        tooltip: String::new(),
        elevated: None,
        firewall: Firewall::Unknown,
        firewall_check: Some(check_firewall()),
        links: HashMap::new(),
        context: None,
        ticks: 0,
        quitting: false,
        update_exit: Default::default(),
        update_notice: Some(crate::updates::check_in_background(settings.update_channel)),
        update_available: None,
        settings,
        hotkeys: None,
    }));

    load_controls(ui, &state);

    // Deferred work, run from a one-shot timer outside any control's own event.
    let deferred: Rc<RefCell<Vec<Deferred>>> = Rc::new(RefCell::new(Vec::new()));
    let defer_timer: &'static Timer<Frame> = Box::leak(Box::new(Timer::new(&frame)));
    {
        let (state, deferred) = (state.clone(), deferred.clone());
        let deferred_again = deferred.clone();
        defer_timer.on_tick(move |_| {
            let actions: Vec<Deferred> = deferred.borrow_mut().drain(..).collect();
            for action in actions {
                match action {
                    Deferred::RebuildConnected { select, focus } => {
                        rebuild_connected(ui, &state, select.as_deref());
                        rebuild_discovered(ui, &state);
                        if focus {
                            ui.connected.set_focus();
                        }
                    }
                    Deferred::Rename(list) => rename_peer(ui, &state, list),
                    Deferred::Connect => connect_candidate(ui, &state, &|action, delay| {
                        deferred_again.borrow_mut().push(action);
                        defer_timer.start(delay, true);
                    }),
                    Deferred::Remove => remove_peer(ui, &state, &|action, delay| {
                        deferred_again.borrow_mut().push(action);
                        defer_timer.start(delay, true);
                    }),
                    Deferred::Uncheck(list) => list_of(ui, list).check(0, false),
                }
            }
        });
    }
    let defer = {
        let deferred = deferred.clone();
        move |action: Deferred, delay_ms: i32| {
            deferred.borrow_mut().push(action);
            defer_timer.start(delay_ms, true);
        }
    };

    bind_connectivity(ui, &state, defer.clone());
    bind_audio(ui, &state);

    let main_timer: &'static Timer<Frame> = Box::leak(Box::new(Timer::new(&frame)));
    let tray = tray::create(ui, &state, main_timer);

    // ---- Menu
    {
        let state = state.clone();
        let defer = defer.clone();
        frame.on_menu(move |event: Event| match event.get_id() {
            // From a peer list's context menu: run once the menu has closed.
            ID_RENAME => {
                let list = state.borrow_mut().context.take();
                if let Some(list) = list {
                    defer(Deferred::Rename(list), 1);
                }
            }
            ID_REMOVE_PEER => {
                state.borrow_mut().context = None;
                defer(Deferred::Remove, 1);
            }
            ID_FIREWALL => allow_firewall(ui, &state),
            ID_CONNECT => {
                state.borrow_mut().context = None;
                defer(Deferred::Connect, 1);
            }
            ID_UPDATES => {
                let (exit, channel) = {
                    let st = state.borrow();
                    (st.update_exit.clone(), st.settings.update_channel)
                };
                crate::updates::check_now(&ui.frame, exit, channel);
            }
            ID_SAVE => {
                save_profile(ui, &state);
            }
            ID_SAVE_AS => save_profile_as(ui, &state),
            ID_SWITCH => choose_profile(ui, &state),
            ID_DELETE => delete_profile(ui, &state),
            ID_SERVICE_INSTALL => install_service(ui, &state),
            ID_SERVICE_REMOVE => remove_service(ui, &state),
            ID_SETTINGS => edit_settings(ui, &state),
            ID_HIDE => frame.show(false),
            ID_QUIT => quit(ui, &state, tray, main_timer),
            _ => event.skip(true),
        });
    }

    // ---- Closing hides to the tray (or quits, if the settings say so); File, Exit really quits.
    {
        let state = state.clone();
        frame.on_close(move |event| {
            let WindowEventData::General(event) = event else { return };
            let (quitting, exit_on_close) = {
                let st = state.borrow();
                (st.quitting, st.settings.exit_on_close)
            };
            if !event.can_veto() || quitting {
                return;
            }
            event.veto();
            event.skip(false);
            if exit_on_close {
                quit(ui, &state, tray, main_timer);
            } else {
                frame.show(false);
            }
        });
    }

    {
        let state = state.clone();
        main_timer.on_tick(move |_| tick(ui, &state));
    }
    main_timer.start(500, false);

    // ---- Global shortcuts
    {
        let state = state.clone();
        HOTKEY_HANDLER.with(|h| *h.borrow_mut() = Some(Rc::new(move |action| run_action(ui, &state, action))));
    }
    let failed = register_hotkeys(&state);

    refresh(ui, &state);
    frame.centre();
    let updated = ship_shape::relaunched_after_update();
    // Starting hidden leaves only the tray icon; after an update the window comes back regardless.
    if updated || !state.borrow().settings.start_in_tray {
        frame.show(true);
        ui.connected.set_focus();
    }
    if updated {
        ship_shape::ui::bring_to_front_after_update(&frame);
        speech::say(&format!("RemSoundLite updated to {}", crate::updates::build_name()));
    }
    if !failed.is_empty() {
        speech::say(&unregistered_text(&failed));
    }
}

/// Reads the firewall rules on a thread; the result is picked up by the timer.
fn check_firewall() -> mpsc::Receiver<Firewall> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(service::firewall_state());
    });
    rx
}

fn smoothness_label_text(value: u8) -> String {
    match value {
        10 => "10 \u{2014} smoothest, no clicks, longest delay".to_string(),
        3 => "3 \u{2014} default, brief clicks".to_string(),
        1 => "1 \u{2014} tightest delay, frequent clicks".to_string(),
        other => other.to_string(),
    }
}

// ================================================================== loading controls

fn load_controls(ui: Ui, state: &Shared) {
    {
        let st = state.borrow();
        let p = &st.profile;
        ui.password.change_value(&p.password);
        ui.send.set_value(p.send_enabled);
        ui.receive.set_value(p.receive_enabled);
        ui.codec.set_selection(text::codec_index(p.codec));
        ui.packet.set_selection(PacketSize::ALL.iter().position(|s| *s == p.packet_size).unwrap_or(0) as u32, true);
        ui.volume.set_value(p.volume as i32);
        ui.auto_accept.set_value(p.auto_accept);
        ui.jitter.set_value(p.jitter_ms as i32);
        ui.auto_tune.set_value(p.auto_tune);
        ui.smoothness.set_selection((10 - p.smoothness.clamp(1, 10)) as u32, true);
        ui.artefact.set_selection(ARTEFACTS.iter().position(|(a, _)| *a == p.artefact).unwrap_or(0) as u32, true);
    }
    state.borrow_mut().detail = Detail::None;
    rebuild_connected(ui, state, None);
    rebuild_discovered(ui, state);
    rebuild_devices(ui, state);
    update_title(ui, &state.borrow());
}

fn update_title(ui: Ui, st: &State) {
    let title = format!(
        "RemSoundLite \u{2014} {} profile{}",
        st.profile_name,
        if st.dirty() { " (unsaved changes)" } else { "" }
    );
    if ui.frame.get_title() != title {
        ui.frame.set_title(&title);
    }
}

/// What a peer is called: the name you gave it, else the name it announces, else its address.
fn peer_display_name(address: &str, saved: Option<&SavedPeer>, discovered: &[DiscoveredPeer]) -> String {
    if let Some(name) = saved.map(|p| p.name.trim()).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    discovered
        .iter()
        .find(|d| d.address.to_string() == address)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| address.to_string())
}

fn peer_label(peer: &SavedPeer, discovered: &[DiscoveredPeer]) -> String {
    peer_display_name(&peer.address, Some(peer), discovered)
}

const CONNECTED_PLACEHOLDER: &str = "No connected peers yet. Tick a discovered peer, or add one by IP address.";
const DISCOVERED_PLACEHOLDER: &str = "No peers found on the network yet.";

/// Fills a check list, or gives it one placeholder row while there is nothing to show: an
/// empty check list must never take focus (wx asserts), and a hidden one can't be found.
fn fill_peer_list(list: CheckListBox, rows: &[(String, bool)], placeholder: &str, select: Option<usize>) {
    let previous = list.get_selection();
    list.clear();
    if rows.is_empty() {
        list.append(placeholder);
        list.set_selection(0, true);
        return;
    }
    for (index, (label, ticked)) in rows.iter().enumerate() {
        list.append(label);
        list.check(index as u32, *ticked);
    }
    let index = select.or_else(|| previous.map(|i| (i as usize).min(rows.len() - 1)));
    if let Some(index) = index {
        list.set_selection(index as u32, true);
    }
}

fn rebuild_connected(ui: Ui, state: &Shared, select: Option<&str>) {
    let mut st = state.borrow_mut();
    let labels: Vec<String> = st.profile.peers.iter().map(|p| peer_label(p, &st.discovered)).collect();
    let previous_address = ui
        .connected
        .get_selection()
        .and_then(|i| st.profile.peers.get(i as usize))
        .map(|p| p.address.clone());
    let wanted = select.map(str::to_string).or(previous_address);
    let index = wanted.and_then(|a| st.profile.peers.iter().position(|p| p.address == a));
    let rows: Vec<(String, bool)> = labels.iter().cloned().zip(st.profile.peers.iter().map(|p| p.active)).collect();
    fill_peer_list(ui.connected, &rows, CONNECTED_PLACEHOLDER, index);
    st.connected_labels = labels;
    let empty = st.profile.peers.is_empty();
    drop(st);
    if empty && ui.remove.has_focus() {
        ui.connected.set_focus();
    }
    ui.remove.show(!empty);
    ui.connectivity.layout();
}

/// Everyone you could connect to: discovered computers and computers asking to connect,
/// minus anyone already in your connected peers (ticked or not).
fn candidates(st: &State) -> Vec<Candidate> {
    let known = |address: Ipv4Addr| st.profile.peers.iter().any(|p| p.address == address.to_string());
    let wants = |address: Ipv4Addr| st.snapshot.incoming.iter().any(|i| i.address == address);
    let mut out: Vec<Candidate> = st
        .discovered
        .iter()
        .filter(|d| !known(d.address))
        .map(|d| Candidate {
            address: d.address,
            name: d.name.clone(),
            offers: Some((d.can_send, d.can_receive)),
            wants: wants(d.address),
        })
        .collect();
    for incoming in &st.snapshot.incoming {
        if !known(incoming.address) && !out.iter().any(|c| c.address == incoming.address) {
            out.push(Candidate { address: incoming.address, name: incoming.address.to_string(), offers: None, wants: true });
        }
    }
    out
}

fn discovered_labels(st: &State) -> Vec<String> {
    st.candidates
        .iter()
        .map(|c| if c.wants { format!("{}, wants to connect", c.name) } else { c.name.clone() })
        .collect()
}

fn rebuild_discovered(ui: Ui, state: &Shared) {
    let mut st = state.borrow_mut();
    let selected_address = ui.discovered.get_selection().and_then(|i| st.candidates_shown.get(i as usize)).copied();
    st.candidates = candidates(&st);
    let labels = discovered_labels(&st);
    let rows: Vec<(String, bool)> = labels.iter().map(|l| (l.clone(), false)).collect();
    let index = selected_address.and_then(|a| st.candidates.iter().position(|c| c.address == a));
    fill_peer_list(ui.discovered, &rows, DISCOVERED_PLACEHOLDER, index);
    st.candidates_shown = st.candidates.iter().map(|c| c.address).collect();
    st.discovered_labels = labels;
}

fn rebuild_devices(ui: Ui, state: &Shared) {
    let mut st = state.borrow_mut();
    let mut input_rows: Vec<(SavedDevice, String)> =
        vec![(SavedDevice::default_input(), "Default input".to_string())];
    input_rows.extend(
        st.devices.inputs.iter().map(|d| (SavedDevice { id: d.id.clone(), name: d.name.clone() }, d.name.clone())),
    );
    let mut loopback_rows: Vec<(SavedDevice, String)> =
        vec![(SavedDevice::default_output(), "Default output".to_string())];
    loopback_rows.extend(st.devices.outputs.iter().map(|d| {
        (SavedDevice { id: d.id.clone(), name: d.name.clone() }, d.name.clone())
    }));
    // Ticked devices that aren't plugged in stay listed, under the kind their id says.
    for saved in &st.profile.sources {
        let rows = if saved.is_output() { &mut loopback_rows } else { &mut input_rows };
        if !rows.iter().any(|(d, _)| d.id == saved.id) {
            rows.push((saved.clone(), format!("{} (not connected)", saved.name)));
        }
    }
    let mut output_rows: Vec<(SavedDevice, String)> = vec![(SavedDevice::default_output(), "Default output".to_string())];
    output_rows.extend(
        st.devices.outputs.iter().map(|d| (SavedDevice { id: d.id.clone(), name: d.name.clone() }, d.name.clone())),
    );
    for saved in &st.profile.outputs {
        if !output_rows.iter().any(|(d, _)| d.id == saved.id) {
            output_rows.push((saved.clone(), format!("{} (not connected)", saved.name)));
        }
    }
    let fill = |list: CheckListBox, rows: &[(SavedDevice, String)], ticked: &[SavedDevice]| {
        let selected = list.get_selection();
        list.clear();
        for (index, (device, label)) in rows.iter().enumerate() {
            list.append(label);
            list.check(index as u32, ticked.iter().any(|t| t.id == device.id));
        }
        list.set_selection(selected.filter(|i| (*i as usize) < rows.len()).unwrap_or(0), true);
    };
    fill(ui.input_sources, &input_rows, &st.profile.sources);
    fill(ui.output_sources, &loopback_rows, &st.profile.sources);
    fill(ui.outputs, &output_rows, &st.profile.outputs);
    st.input_source_rows = input_rows.into_iter().map(|(d, _)| d).collect();
    st.output_source_rows = loopback_rows.into_iter().map(|(d, _)| d).collect();
    st.output_rows = output_rows.into_iter().map(|(d, _)| d).collect();
}

// ================================================================== events

fn changed(ui: Ui, state: &Shared) {
    let st = state.borrow();
    st.apply();
    update_title(ui, &st);
}

/// Turns sending on or off from outside the window (tray, shortcut), and says so: the check box
/// changing isn't something the focus would report.
fn set_sending(ui: Ui, state: &Shared, value: bool) {
    state.borrow_mut().profile.send_enabled = value;
    ui.send.set_value(value);
    changed(ui, state);
    tray::update_tooltip(state);
    speech::say(if value { "Sending on" } else { "Sending off" });
}

fn set_receiving(ui: Ui, state: &Shared, value: bool) {
    state.borrow_mut().profile.receive_enabled = value;
    ui.receive.set_value(value);
    changed(ui, state);
    tray::update_tooltip(state);
    speech::say(if value { "Receiving on" } else { "Receiving off" });
}

/// Loads a profile from outside the window, and says so.
fn load_profile_and_say(ui: Ui, state: &Shared, name: &str) {
    if state.borrow().profile_name == name {
        speech::say(&format!("{name} profile already loaded"));
        return;
    }
    switch_profile(ui, state, name);
    tray::update_tooltip(state);
    if state.borrow().profile_name == name {
        speech::say(&format!("{name} profile loaded"));
    }
}

/// The address and shown name behind the selected row of either list, if it's a real peer.
fn selected_peer(ui: Ui, st: &State, list: PeerList) -> Option<(String, String)> {
    match list {
        PeerList::Connected => {
            let peer = st.profile.peers.get(ui.connected.get_selection()? as usize)?;
            Some((peer.address.clone(), peer_label(peer, &st.discovered)))
        }
        PeerList::Discovered => {
            let index = ui.discovered.get_selection()? as usize;
            let address = st.candidates_shown.get(index)?;
            let candidate = st.candidates.iter().find(|c| c.address == *address)?;
            Some((address.to_string(), candidate.name.clone()))
        }
    }
}

fn list_of(ui: Ui, list: PeerList) -> CheckListBox {
    match list {
        PeerList::Connected => ui.connected,
        PeerList::Discovered => ui.discovered,
    }
}

fn rename_peer(ui: Ui, state: &Shared, list: PeerList) {
    let Some((address, shown)) = selected_peer(ui, &state.borrow(), list) else { return };
    let current = state
        .borrow()
        .profile
        .peers
        .iter()
        .find(|p| p.address == address)
        .map(|p| p.name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(shown);
    let dialog = TextEntryDialog::builder(
        &ui.frame,
        &format!("Name for the peer at {address}. Leave it empty to use the name the computer announces:"),
        "Rename peer",
    )
    .with_default_value(&current)
    .build();
    if dialog.show_modal() != ID_OK {
        list_of(ui, list).set_focus();
        return;
    }
    let name: String = dialog.get_value().unwrap_or_default().trim().chars().take(80).collect();
    {
        let mut st = state.borrow_mut();
        match st.profile.peers.iter_mut().find(|p| p.address == address) {
            Some(peer) => peer.name = name.clone(),
            // A discovered peer you haven't ticked: remember the name, unticked.
            None => st.profile.peers.push(SavedPeer { name: name.clone(), address: address.clone(), active: false }),
        }
    }
    changed(ui, state);
    rebuild_connected(ui, state, Some(&address));
    rebuild_discovered(ui, state);
    list_of(ui, list).set_focus();
    let shown = {
        let st = state.borrow();
        let saved = st.profile.peers.iter().find(|p| p.address == address);
        peer_display_name(&address, saved, &st.discovered)
    };
    // The focus stays on the row, so the screen reader wouldn't notice the new name.
    speech::say(&format!("Renamed to {shown}"));
}

/// Moves the selected discovered peer into the connected peers, ticked.
fn connect_candidate(ui: Ui, state: &Shared, defer: &impl Fn(Deferred, i32)) {
    let Some((address, name)) = selected_peer(ui, &state.borrow(), PeerList::Discovered) else { return };
    {
        let mut st = state.borrow_mut();
        match st.profile.peers.iter_mut().find(|p| p.address == address) {
            Some(existing) => existing.active = true,
            None => st.profile.peers.push(SavedPeer { name: String::new(), address: address.clone(), active: true }),
        }
        st.detail = Detail::Connected(address.clone());
    }
    changed(ui, state);
    defer(Deferred::RebuildConnected { select: Some(address), focus: false }, 1);
    // It leaves this list for the other one, which the screen reader isn't on.
    speech::say(&format!("{name} moved to connected peers"));
}

fn remove_peer(ui: Ui, state: &Shared, defer: &impl Fn(Deferred, i32)) {
    let Some(index) = ui.connected.get_selection().map(|i| i as usize) else { return };
    let removed = {
        let mut st = state.borrow_mut();
        if index >= st.profile.peers.len() {
            return;
        }
        let peer = st.profile.peers.remove(index);
        st.detail = Detail::None;
        peer_label(&peer, &st.discovered)
    };
    changed(ui, state);
    let next = {
        let st = state.borrow();
        st.profile.peers.get(index.min(st.profile.peers.len().saturating_sub(1))).map(|p| p.address.clone())
    };
    defer(Deferred::RebuildConnected { select: next, focus: true }, 1);
    speech::say(&format!("{removed} removed"));
}

fn bind_connectivity(ui: Ui, state: &Shared, defer: impl Fn(Deferred, i32) + Clone + 'static) {
    {
        let state = state.clone();
        ui.connected.on_selected(move |event| {
            let Some(index) = event.get_selection() else { return };
            let address = state.borrow().profile.peers.get(index as usize).map(|p| p.address.clone());
            state.borrow_mut().detail = address.map_or(Detail::None, Detail::Connected);
            refresh(ui, &state);
        });
    }
    {
        let state = state.clone();
        let defer = defer.clone();
        ui.connected.on_toggled(move |event| {
            let Some(index) = event.get_selection() else { return };
            let checked = ui.connected.is_checked(index);
            let address = state.borrow().profile.peers.get(index as usize).map(|p| p.address.clone());
            let Some(address) = address else {
                // The placeholder row: nothing to tick.
                defer(Deferred::Uncheck(PeerList::Connected), 1);
                return;
            };
            if let Some(peer) = state.borrow_mut().profile.peers.get_mut(index as usize) {
                peer.active = checked;
            }
            // Keep the discovered list's tick in step (another control, so safe to touch).
            let st = state.borrow();
            for (row, peer) in st.discovered.iter().enumerate() {
                if peer.address.to_string() == address {
                    ui.discovered.check(row as u32, checked);
                }
            }
            drop(st);
            state.borrow_mut().detail = Detail::Connected(address);
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        let defer = defer.clone();
        ui.remove.on_click(move |_| remove_peer(ui, &state, &defer));
    }
    for list in [PeerList::Connected, PeerList::Discovered] {
        let state = state.clone();
        let defer = defer.clone();
        list_of(ui, list).on_key_down(move |event| {
            if let WindowEventData::Keyboard(key) = &event {
                match key.get_key_code() {
                    Some(WXK_F2) if list == PeerList::Connected => {
                        key.event.skip(false);
                        defer(Deferred::Rename(list), 1);
                        return;
                    }
                    Some(WXK_DELETE) if list == PeerList::Connected => {
                        key.event.skip(false);
                        remove_peer(ui, &state, &defer);
                        return;
                    }
                    _ => {}
                }
            }
            event.skip(true);
        });
    }
    {
        // Both lists' context-menu events (right-click, the Menu key, Shift+F10) bubble up to
        // the panel; the menu's commands reach the frame's menu handler.
        let state = state.clone();
        ui.connectivity.on_context_menu(move |event| {
            let id = event.get_id();
            let list = if id == ui.connected.get_id() {
                PeerList::Connected
            } else if id == ui.discovered.get_id() {
                PeerList::Discovered
            } else {
                event.skip(true);
                return;
            };
            if selected_peer(ui, &state.borrow(), list).is_none() {
                return;
            }
            state.borrow_mut().context = Some(list);
            let mut menu = match list {
                PeerList::Connected => Menu::builder()
                    .append_item(ID_RENAME, "&Rename\u{2026}\tF2", "Give this peer a name")
                    .append_item(ID_REMOVE_PEER, "Re&move\tDelete", "Remove this peer")
                    .build(),
                PeerList::Discovered => Menu::builder()
                    .append_item(ID_CONNECT, "&Connect\tSpace", "Move it to your connected peers")
                    .build(),
            };
            list_of(ui, list).popup_menu(&mut menu, None);
        });
    }
    {
        let state = state.clone();
        ui.discovered.on_selected(move |event| {
            let Some(index) = event.get_selection() else { return };
            let address = state.borrow().candidates_shown.get(index as usize).copied();
            state.borrow_mut().detail = address.map_or(Detail::None, Detail::Candidate);
            refresh(ui, &state);
        });
    }
    {
        let state = state.clone();
        let defer = defer.clone();
        ui.discovered.on_toggled(move |_| {
            // Ticking a discovered peer connects it, and it moves to the connected list. The
            // list itself is rebuilt from a timer, never inside its own toggle event.
            if selected_peer(ui, &state.borrow(), PeerList::Discovered).is_some() {
                connect_candidate(ui, &state, &defer);
            } else {
                defer(Deferred::Uncheck(PeerList::Discovered), 1);
            }
        });
    }
    {
        let state = state.clone();
        let defer = defer.clone();
        ui.add.on_click(move |_| {
            let dialog = TextEntryDialog::builder(
                &ui.frame,
                "IP address of the peer (for example 100.110.55.121):",
                "Add peer by IP address",
            )
            .build();
            if dialog.show_modal() != ID_OK {
                return;
            }
            let text = dialog.get_value().unwrap_or_default().trim().to_string();
            let Ok(address) = text.parse::<Ipv4Addr>() else {
                message(
                    &ui.frame,
                    &format!("\"{text}\" is not an IPv4 address. Type four numbers separated by dots, such as 192.168.1.20."),
                    "Add peer by IP address",
                    MessageDialogStyle::OK | MessageDialogStyle::IconWarning,
                );
                return;
            };
            let address = address.to_string();
            {
                let mut st = state.borrow_mut();
                match st.profile.peers.iter_mut().find(|p| p.address == address) {
                    Some(existing) => existing.active = true,
                    None => st.profile.peers.push(SavedPeer { name: String::new(), address: address.clone(), active: true }),
                }
                st.detail = Detail::Connected(address.clone());
            }
            changed(ui, &state);
            defer(Deferred::RebuildConnected { select: Some(address), focus: true }, 1);
        });
    }
    {
        let state = state.clone();
        let password_timer: &'static Timer<Frame> = Box::leak(Box::new(Timer::new(&ui.frame)));
        {
            let state = state.clone();
            password_timer.on_tick(move |_| {
                let st = state.borrow();
                st.apply();
                update_title(ui, &st);
            });
        }
        let accept_state = state.clone();
        ui.auto_accept.on_toggled(move |event| {
            accept_state.borrow_mut().profile.auto_accept = event.is_checked();
            changed(ui, &accept_state);
        });
        ui.password.on_text_changed(move |_| {
            state.borrow_mut().profile.password = ui.password.get_value();
            update_title(ui, &state.borrow());
            // Deriving the key is slow on purpose; wait until typing pauses.
            password_timer.start(700, true);
        });
    }
}

fn device_rows_ticked(list: CheckListBox, rows: &[SavedDevice]) -> Vec<SavedDevice> {
    rows.iter().enumerate().filter(|(i, _)| list.is_checked(*i as u32)).map(|(_, d)| d.clone()).collect()
}

fn bind_audio(ui: Ui, state: &Shared) {
    {
        let state = state.clone();
        ui.send.on_toggled(move |event| {
            state.borrow_mut().profile.send_enabled = event.is_checked();
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.receive.on_toggled(move |event| {
            state.borrow_mut().profile.receive_enabled = event.is_checked();
            changed(ui, &state);
        });
    }
    // Both send lists feed the profile's one list of sources: inputs first, then loopback.
    for list in [ui.input_sources, ui.output_sources] {
        let state = state.clone();
        list.on_toggled(move |_| {
            let mut st = state.borrow_mut();
            let mut sources = device_rows_ticked(ui.input_sources, &st.input_source_rows);
            sources.extend(device_rows_ticked(ui.output_sources, &st.output_source_rows));
            st.profile.sources = sources;
            drop(st);
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.outputs.on_toggled(move |_| {
            let mut st = state.borrow_mut();
            st.profile.outputs = device_rows_ticked(ui.outputs, &st.output_rows);
            drop(st);
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.codec.on_selection_changed(move |event| {
            let Some(index) = event.get_selection() else { return };
            state.borrow_mut().profile.codec = Codec::ALL[(index as usize).min(Codec::ALL.len() - 1)];
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.packet.on_selection_changed(move |event| {
            let Some(index) = event.get_selection() else { return };
            state.borrow_mut().profile.packet_size = PacketSize::ALL[(index as usize).min(1)];
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.jitter.on_value_changed(move |event| {
            let value = (event.get_value() as u32).clamp(profile::MIN_JITTER_MS, profile::MAX_JITTER_MS);
            state.borrow_mut().profile.jitter_ms = value;
            changed(ui, &state);
            refresh(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.volume.on_slider(move |event| {
            state.borrow_mut().profile.volume = event.get_value().clamp(0, 100) as u8;
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.auto_tune.on_toggled(move |event| {
            state.borrow_mut().profile.auto_tune = event.is_checked();
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.smoothness.on_selection_changed(move |event| {
            let Some(index) = event.get_selection() else { return };
            state.borrow_mut().profile.smoothness = (10 - index.clamp(0, 9)) as u8;
            changed(ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.artefact.on_selection_changed(move |event| {
            let Some(index) = event.get_selection() else { return };
            state.borrow_mut().profile.artefact = ARTEFACTS[(index as usize).min(ARTEFACTS.len() - 1)].0;
            changed(ui, &state);
        });
    }
}

// ================================================================== refreshing

fn set_text(ctrl: TextCtrl, text: &str, key: &str, last_key: &mut String) {
    if ctrl.get_value() == text {
        *last_key = key.to_string();
        return;
    }
    if ctrl.has_focus() && *last_key == key {
        return;
    }
    let point = ctrl.get_insertion_point();
    ctrl.change_value(text);
    ctrl.set_insertion_point(point.min(ctrl.get_last_position()));
    *last_key = key.to_string();
}

fn tick(ui: Ui, state: &Shared) {
    let finished = {
        let mut st = state.borrow_mut();
        st.ticks += 1;
        match st.elevated.as_ref().map(|(rx, what)| (rx.try_recv(), *what)) {
            Some((Ok(result), what)) => {
                st.elevated = None;
                Some((result, what))
            }
            _ => None,
        }
    };
    let firewall = state.borrow().firewall_check.as_ref().and_then(|rx| rx.try_recv().ok());
    if let Some(firewall) = firewall {
        let mut st = state.borrow_mut();
        st.firewall = firewall;
        st.firewall_check = None;
    }
    if let Some((result, what)) = finished {
        // Whatever ran elevated may have changed the firewall rules.
        state.borrow_mut().firewall_check = Some(check_firewall());
        match result {
            Ok(()) => message(&ui.frame, what, "Lock-screen service", MessageDialogStyle::OK | MessageDialogStyle::IconInformation),
            Err(e) => message(&ui.frame, &e, "Lock-screen service", MessageDialogStyle::OK | MessageDialogStyle::IconError),
        };
    }
    if state.borrow().update_exit.load(std::sync::atomic::Ordering::Relaxed) {
        // A downloaded update is ready and waits for us to quit; no questions asked.
        state.borrow_mut().saved = state.borrow().profile.clone();
        ui.frame.close(true);
        if let Some(app) = get_app_instance() {
            app.exit_main_loop();
        }
        return;
    }
    let notice = state.borrow().update_notice.as_ref().and_then(|rx| rx.try_recv().ok());
    if let Some(notice) = notice {
        state.borrow_mut().update_notice = None;
        state.borrow_mut().update_available = Some(notice.clone());
        speech::say(&format!("{notice} Use File, Check for updates to install it."));
    }
    let ticks = state.borrow().ticks;
    if ticks.is_multiple_of(10) {
        let fresh = devices::list();
        let differs = state.borrow().devices != fresh;
        if differs {
            state.borrow_mut().devices = fresh;
            rebuild_devices(ui, state);
        }
    }
    if ticks.is_multiple_of(2) && ui.frame.is_shown() {
        refresh(ui, state);
    } else if ticks.is_multiple_of(2) {
        // Hidden: keep the snapshot (and the tray's status) current, without touching controls.
        let snapshot = state.borrow().engine.as_ref().map(Engine::snapshot);
        if let Some(snapshot) = snapshot {
            sync_peer_lists(ui, state, snapshot);
        }
        announce_links(state);
        tray::update_tooltip(state);
    }
}

fn refresh(ui: Ui, state: &Shared) {
    let Some(snapshot) = state.borrow().engine.as_ref().map(Engine::snapshot) else { return };
    {
        let mut st = state.borrow_mut();
        let now = Instant::now();
        let (rx, tx, at) = st.last_bytes;
        let seconds = (now - at).as_secs_f64();
        if seconds >= 0.5 {
            st.rates = (
                snapshot.bytes_received.saturating_sub(rx) as f64 / seconds,
                snapshot.bytes_sent.saturating_sub(tx) as f64 / seconds,
            );
            st.last_bytes = (snapshot.bytes_received, snapshot.bytes_sent, now);
            st.cpu.update();
        }
    }

    sync_peer_lists(ui, state, snapshot);

    {
    let mut guard = state.borrow_mut();
    let st = &mut *guard;
    let (details, details_key) = match &st.detail {
        Detail::Connected(address) => match st.snapshot.peers.iter().find(|p| p.address.to_string() == *address) {
            Some(peer) => text::peer_details(peer, &st.profile, &st.snapshot),
            None => {
                let text = format!("Address: {address}\nPaused: tick it in the list to connect.");
                (text.clone(), text)
            }
        },
        Detail::Candidate(address) => match st.candidates.iter().find(|c| c.address == *address) {
            Some(c) => text::candidate_details(&c.name, c.address, c.offers, c.wants),
            None => match st.profile.peers.iter().find(|p| p.address == address.to_string()) {
                Some(_) => ("It's in your connected peers now.".to_string(), "moved".to_string()),
                None => ("That peer is no longer on the network.".to_string(), "gone".to_string()),
            },
        },
        Detail::None => {
            let text = "Select a peer in either list to see its details here.".to_string();
            (text.clone(), text)
        }
    };
    set_text(ui.details, &details, &details_key, &mut st.details_key);
    let rates = text::Rates { received_per_second: st.rates.0, sent_per_second: st.rates.1 };
    let (status, status_key) =
        text::connection_status(
        &st.profile,
        &st.snapshot,
        &rates,
        st.cpu.percent,
        crate::sysinfo::memory_bytes(),
        st.firewall,
        st.update_available.as_deref(),
    );
    set_text(ui.status, &status, &status_key, &mut st.status_key);
    let (latency, latency_key) = text::total_latency(&st.profile, &st.snapshot);
    set_text(ui.latency, &latency, &latency_key, &mut st.latency_key);
    update_title(ui, st);
    }
    announce_links(state);
    tray::update_tooltip(state);
}

/// Takes in a fresh snapshot: accepts or lists peers asking to connect, and rebuilds the peer
/// lists when what they show has changed. Runs while the window is hidden too, so automatic
/// accepting works from the tray.
fn sync_peer_lists(ui: Ui, state: &Shared, snapshot: Snapshot) {
    let mut accepted = Vec::new();
    let mut requests = Vec::new();
    {
        let mut st = state.borrow_mut();
        st.discovered = snapshot.discovered.clone();
        st.snapshot = snapshot;
        let incoming = st.snapshot.incoming.clone();
        for request in incoming {
            let address = request.address.to_string();
            if st.profile.peers.iter().any(|p| p.address == address) {
                continue; // in your list already, ticked or deliberately not
            }
            let saved = None;
            let name = peer_display_name(&address, saved, &st.discovered);
            if st.profile.auto_accept {
                st.profile.peers.push(SavedPeer { name: String::new(), address, active: true });
                accepted.push(name);
            } else if st.announced_requests.insert(request.address) {
                requests.push(name);
            }
        }
    }
    if !accepted.is_empty() {
        changed(ui, state);
    }
    let lists_changed = {
        let st = state.borrow();
        let labels: Vec<String> = st.profile.peers.iter().map(|p| peer_label(p, &st.discovered)).collect();
        labels != st.connected_labels || candidates(&st) != st.candidates
    };
    if lists_changed || !accepted.is_empty() {
        rebuild_connected(ui, state, None);
        rebuild_discovered(ui, state);
    }
    for name in accepted {
        speech::say(&format!("{name} connected to you and was accepted"));
    }
    for name in requests {
        speech::say(&format!("{name} wants to connect. Tick it in the discovered peers to accept."));
    }
}

/// Says when a ticked peer connects or stops answering: it happens while you're elsewhere,
/// and nothing on screen would tell you.
fn announce_links(state: &Shared) {
    let mut lines = Vec::new();
    {
        let mut guard = state.borrow_mut();
        let st = &mut *guard;
        let mut seen = HashMap::new();
        for peer in &st.snapshot.peers {
            let connected = matches!(peer.health, Health::Healthy | Health::Stale);
            let key = peer.address.to_string();
            if st.links.get(&key).is_some_and(|was| *was != connected) {
                lines.push(if connected {
                    format!("{} connected", peer.name)
                } else {
                    format!("{} is not answering", peer.name)
                });
            }
            seen.insert(key, connected);
        }
        st.links = seen;
    }
    for line in lines {
        speech::say(&line);
    }
}

// ================================================================== profiles

/// Asks whether to keep unsaved changes. `false` means the user cancelled.
fn settle_unsaved(ui: Ui, state: &Shared, doing: &str) -> bool {
    let (dirty, name) = {
        let st = state.borrow();
        (st.dirty(), st.profile_name.clone())
    };
    if !dirty {
        return true;
    }
    let answer = message(
        &ui.frame,
        &format!("Save the changes to the {name} profile before {doing}?"),
        "Unsaved changes",
        MessageDialogStyle::YesNo | MessageDialogStyle::Cancel | MessageDialogStyle::IconQuestion,
    );
    match answer {
        ID_YES => save_profile(ui, state),
        ID_NO => true,
        _ => false,
    }
}

fn save_profile(ui: Ui, state: &Shared) -> bool {
    let result = {
        let st = state.borrow();
        st.store.save(&st.profile_name, &st.profile)
    };
    match result {
        Ok(()) => {
            let mut st = state.borrow_mut();
            st.saved = st.profile.clone();
            update_title(ui, &st);
            true
        }
        Err(e) => {
            message(&ui.frame, &format!("The profile could not be saved: {e}"), "Save profile", MessageDialogStyle::OK | MessageDialogStyle::IconError);
            false
        }
    }
}

fn save_profile_as(ui: Ui, state: &Shared) {
    let dialog = TextEntryDialog::builder(&ui.frame, "Name of the new profile:", "Save profile as").build();
    if dialog.show_modal() != ID_OK {
        return;
    }
    let name = dialog.get_value().unwrap_or_default().trim().to_string();
    if !profile::valid_profile_name(&name) {
        message(
            &ui.frame,
            "That name can't be used. Avoid these characters: < > : \" / \\ | ? *",
            "Save profile as",
            MessageDialogStyle::OK | MessageDialogStyle::IconWarning,
        );
        return;
    }
    let exists = state.borrow().store.path_of(&name).exists();
    if exists
        && message(
            &ui.frame,
            &format!("A profile called {name} already exists. Replace it?"),
            "Save profile as",
            MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion,
        ) != ID_YES
    {
        return;
    }
    let previous = std::mem::replace(&mut state.borrow_mut().profile_name, name.clone());
    if save_profile(ui, state) {
        state.borrow().store.set_last_used(&name);
        register_hotkeys(state);
    } else {
        state.borrow_mut().profile_name = previous;
    }
    update_title(ui, &state.borrow());
}

fn switch_profile(ui: Ui, state: &Shared, name: &str) {
    if state.borrow().profile_name == name || !settle_unsaved(ui, state, "switching profiles") {
        return;
    }
    let loaded = state.borrow().store.load(name);
    match loaded {
        Ok(profile) => {
            {
                let mut st = state.borrow_mut();
                st.profile_name = name.to_string();
                st.saved = profile.clone();
                st.profile = profile;
                st.store.set_last_used(name);
                st.apply();
            }
            load_controls(ui, state);
            refresh(ui, state);
        }
        Err(e) => {
            message(&ui.frame, &e, "Switch profile", MessageDialogStyle::OK | MessageDialogStyle::IconError);
        }
    }
}

fn choose_profile(ui: Ui, state: &Shared) {
    let (names, current) = {
        let st = state.borrow();
        (st.store.names(), st.profile_name.clone())
    };
    let choices: Vec<&str> = names.iter().map(String::as_str).collect();
    let dialog = SingleChoiceDialog::builder(&ui.frame, "Profile to load:", "Switch profile", &choices).build();
    if let Some(index) = names.iter().position(|n| *n == current) {
        dialog.set_selection(index as i32);
    }
    if dialog.show_modal() != ID_OK {
        return;
    }
    if let Some(name) = names.get(dialog.get_selection().max(0) as usize) {
        switch_profile(ui, state, name);
    }
}

fn delete_profile(ui: Ui, state: &Shared) {
    let name = state.borrow().profile_name.clone();
    if message(
        &ui.frame,
        &format!("Delete the {name} profile? This can't be undone."),
        "Delete profile",
        MessageDialogStyle::YesNo | MessageDialogStyle::NoDefault | MessageDialogStyle::IconWarning,
    ) != ID_YES
    {
        return;
    }
    let deleted = state.borrow().store.delete(&name);
    if let Err(e) = deleted {
        message(&ui.frame, &format!("The profile could not be deleted: {e}"), "Delete profile", MessageDialogStyle::OK | MessageDialogStyle::IconError);
        return;
    }
    let next = state.borrow().store.names().into_iter().next();
    let next = next.unwrap_or_else(|| {
        let _ = state.borrow().store.save(profile::DEFAULT_PROFILE, &Profile::default());
        profile::DEFAULT_PROFILE.to_string()
    });
    // The deleted profile's unsaved changes go with it.
    {
        let mut st = state.borrow_mut();
        st.saved = st.profile.clone();
        st.profile_name.clear();
    }
    switch_profile(ui, state, &next);
    register_hotkeys(state);
}

// ================================================================== service

fn install_service(ui: Ui, state: &Shared) {
    if state.borrow().elevated.is_some() {
        return;
    }
    if !settle_unsaved(ui, state, "handing it to the lock-screen service") {
        return;
    }
    let (name, path) = {
        let st = state.borrow();
        (st.profile_name.clone(), st.store.path_of(&st.profile_name))
    };
    let update = service::is_installed();
    let text = format!(
        "{} the lock-screen service with the {name} profile?\n\n\
         It starts with Windows and sends this profile's audio to its ticked peers from the sign-in \
         screen onwards. It never plays anything, and it steps aside whenever this window is open.\n\n\
         Windows will ask for administrator permission.",
        if update { "Update" } else { "Install" }
    );
    if message(&ui.frame, &text, "Lock-screen service", MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion) != ID_YES {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let arguments = format!("--install-service \"{}\"", path.display());
    std::thread::spawn(move || {
        let _ = tx.send(service::run_elevated(&arguments));
    });
    state.borrow_mut().elevated = Some((
        rx,
        "The lock-screen service is installed and running. It will send whenever this window is closed.",
    ));
}

fn remove_service(ui: Ui, state: &Shared) {
    if state.borrow().elevated.is_some() {
        return;
    }
    if !service::is_installed() {
        message(&ui.frame, "The lock-screen service isn't installed.", "Lock-screen service", MessageDialogStyle::OK | MessageDialogStyle::IconInformation);
        return;
    }
    if message(
        &ui.frame,
        "Remove the lock-screen service? Windows will ask for administrator permission.",
        "Lock-screen service",
        MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion,
    ) != ID_YES
    {
        return;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(service::run_elevated("--uninstall-service"));
    });
    state.borrow_mut().elevated = Some((rx, "The lock-screen service has been removed."));
}

fn allow_firewall(ui: Ui, state: &Shared) {
    if state.borrow().elevated.is_some() {
        return;
    }
    let text = "Let other computers reach RemSoundLite through Windows Firewall?\n\n\
                This removes any rule blocking it (Windows makes one when its own \"allow access\" question \
                is dismissed) and adds one allowing it on all networks. Windows will ask for administrator permission.";
    if message(&ui.frame, text, "Windows Firewall", MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion) != ID_YES {
        return;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(service::run_elevated("--allow-firewall"));
    });
    state.borrow_mut().elevated = Some((rx, "Windows Firewall now lets peers reach RemSoundLite."));
}

// ================================================================== settings

fn edit_settings(ui: Ui, state: &Shared) {
    // Read afresh, so the last-used profile written since startup is kept.
    let (current, profiles) = {
        let st = state.borrow();
        (st.store.settings(), st.store.names())
    };
    // Release the shortcuts while the dialog is open: the shortcut field has to receive keys
    // that are already bound, and none should fire behind the dialog.
    let hotkeys = state.borrow_mut().hotkeys.take();
    drop(hotkeys);
    let edited = settings::show(&ui.frame, &current, &profiles);
    let Some(edited) = edited else {
        register_hotkeys(state);
        return;
    };
    let saved = state.borrow().store.save_settings(&edited);
    match saved {
        Ok(()) => {
            let mut st = state.borrow_mut();
            // An "update available" from the other channel no longer holds: look again.
            if edited.update_channel != st.settings.update_channel {
                st.update_available = None;
                st.update_notice = Some(crate::updates::check_in_background(edited.update_channel));
            }
            st.settings = edited;
        }
        Err(e) => {
            message(
                &ui.frame,
                &format!("The settings could not be saved: {e}"),
                "Settings",
                MessageDialogStyle::OK | MessageDialogStyle::IconError,
            );
        }
    }
    let failed = register_hotkeys(state);
    if !failed.is_empty() {
        message(&ui.frame, &unregistered_text(&failed), "Keyboard shortcuts", MessageDialogStyle::OK | MessageDialogStyle::IconWarning);
    }
}

// ================================================================== global shortcuts

type ActionHandler = Rc<dyn Fn(Action)>;

thread_local! {
    /// What a pressed shortcut runs, on the GUI thread. An `Rc`, so it's called outside the
    /// borrow: an action can open a dialog, whose loop can deliver the next shortcut.
    static HOTKEY_HANDLER: RefCell<Option<ActionHandler>> = RefCell::new(None);
    /// An action is running (perhaps waiting on its "save changes?" question): ignore others.
    static RUNNING_ACTION: Cell<bool> = const { Cell::new(false) };
}

/// Registers the shortcuts in the settings, releasing the old ones first. Returns those that
/// couldn't be registered, nearly always because Windows or another program holds them.
fn register_hotkeys(state: &Shared) -> Vec<KeyChord> {
    let old = state.borrow_mut().hotkeys.take();
    drop(old);
    let bindings = {
        let st = state.borrow();
        shortcuts::bindings(&st.settings.shortcuts, &st.store.names())
    };
    if bindings.is_empty() {
        return Vec::new();
    }
    let (hotkeys, failed) = GlobalHotkeys::register(bindings, |action: Action| {
        // On the hotkey thread: hand the action to the GUI thread.
        wx_utils::post_to_ui(move || hotkey_pressed(action));
    });
    state.borrow_mut().hotkeys = hotkeys;
    failed
}

fn hotkey_pressed(action: Action) {
    if RUNNING_ACTION.get() {
        return;
    }
    let Some(handler) = HOTKEY_HANDLER.with(|h| h.borrow().clone()) else { return };
    RUNNING_ACTION.set(true);
    handler(action);
    RUNNING_ACTION.set(false);
}

fn run_action(ui: Ui, state: &Shared, action: Action) {
    if state.borrow().quitting {
        return;
    }
    match action {
        Action::ShowWindow => show_window(ui),
        Action::ToggleSend => {
            let value = !state.borrow().profile.send_enabled;
            set_sending(ui, state, value);
        }
        Action::SendOn => set_sending(ui, state, true),
        Action::SendOff => set_sending(ui, state, false),
        Action::ToggleReceive => {
            let value = !state.borrow().profile.receive_enabled;
            set_receiving(ui, state, value);
        }
        Action::ReceiveOn => set_receiving(ui, state, true),
        Action::ReceiveOff => set_receiving(ui, state, false),
        Action::Profile(name) => load_profile_and_say(ui, state, &name),
    }
}

fn unregistered_text(failed: &[KeyChord]) -> String {
    let (verb, pronoun) = if failed.len() == 1 { ("is", "it") } else { ("are", "them") };
    format!(
        "{} {verb} in use by Windows or another program, so RemSoundLite can't use {pronoun}. \
         Pick other shortcuts in Settings, Keyboard shortcuts.",
        shortcuts::describe(failed)
    )
}

// ================================================================== window and quitting

fn show_window(ui: Ui) {
    ui.frame.iconize(false);
    ui.frame.show(true);
    ui.frame.raise();
}

fn quit(ui: Ui, state: &Shared, tray: &'static TaskBarIcon, timer: &'static Timer<Frame>) {
    if !settle_unsaved(ui, state, "exiting") {
        return;
    }
    state.borrow_mut().quitting = true;
    timer.stop();
    let hotkeys = state.borrow_mut().hotkeys.take();
    drop(hotkeys);
    let engine = state.borrow_mut().engine.take();
    drop(engine);
    tray.remove_icon();
    ui.frame.destroy();
    if let Some(app) = get_app_instance() {
        app.exit_main_loop();
    }
}
