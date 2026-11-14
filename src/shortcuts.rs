//! System-wide keyboard shortcuts: which action each one runs, and how they're stored.
//!
//! Stored in the app-wide settings as `{ "action id": "Ctrl+Alt+S" }`. Nothing is bound by
//! default: a global hotkey takes its key from every other program, so only the user picks one.
//! An action with no entry has no shortcut. Profiles get one "switch to" action each, keyed by
//! name. An entry for a profile that's gone is left in the file, never shown or registered, and
//! comes back if a profile with that name is made again.

use std::collections::BTreeMap;

pub use key_chord::KeyChord;

pub type Shortcuts = BTreeMap<String, String>;

const PROFILE_PREFIX: &str = "profile:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    ShowWindow,
    ToggleSend,
    SendOn,
    SendOff,
    ToggleReceive,
    ReceiveOn,
    ReceiveOff,
    /// Load the profile with this name.
    Profile(String),
}

impl Action {
    const FIXED: [Action; 7] = [
        Action::ToggleSend,
        Action::SendOn,
        Action::SendOff,
        Action::ToggleReceive,
        Action::ReceiveOn,
        Action::ReceiveOff,
        Action::ShowWindow,
    ];

    /// Every action there is with these profiles, in the order the settings list shows them.
    pub fn all(profiles: &[String]) -> Vec<Action> {
        let mut actions = Self::FIXED.to_vec();
        actions.extend(profiles.iter().cloned().map(Action::Profile));
        actions
    }

    /// The key it's stored under.
    pub fn id(&self) -> String {
        match self {
            Action::ShowWindow => "show_window".into(),
            Action::ToggleSend => "toggle_send".into(),
            Action::SendOn => "send_on".into(),
            Action::SendOff => "send_off".into(),
            Action::ToggleReceive => "toggle_receive".into(),
            Action::ReceiveOn => "receive_on".into(),
            Action::ReceiveOff => "receive_off".into(),
            Action::Profile(name) => format!("{PROFILE_PREFIX}{name}"),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Action::ShowWindow => "Show the RemSoundLite window".into(),
            Action::ToggleSend => "Toggle sending".into(),
            Action::SendOn => "Turn sending on".into(),
            Action::SendOff => "Turn sending off".into(),
            Action::ToggleReceive => "Toggle receiving".into(),
            Action::ReceiveOn => "Turn receiving on".into(),
            Action::ReceiveOff => "Turn receiving off".into(),
            Action::Profile(name) => format!("Switch to the {name} profile"),
        }
    }
}

pub fn chord_of(shortcuts: &Shortcuts, action: &Action) -> Option<KeyChord> {
    shortcuts.get(&action.id()).and_then(|text| KeyChord::parse(text))
}

/// Binds `action` to `chord`, or unbinds it for `None`.
pub fn set(shortcuts: &mut Shortcuts, action: &Action, chord: Option<&KeyChord>) {
    match chord {
        Some(chord) => {
            shortcuts.insert(action.id(), chord.to_shortcut_string());
        }
        None => {
            shortcuts.remove(&action.id());
        }
    }
}

/// The bound shortcuts of the actions that exist with these profiles, ready to register.
/// A chord already taken by an earlier action is left out, so two can never fight over a key.
pub fn bindings(shortcuts: &Shortcuts, profiles: &[String]) -> Vec<(KeyChord, Action)> {
    let mut bound: Vec<(KeyChord, Action)> = Vec::new();
    for action in Action::all(profiles) {
        if let Some(chord) = chord_of(shortcuts, &action)
            && !bound.iter().any(|(other, _)| other.conflicts_with(&chord))
        {
            bound.push((chord, action));
        }
    }
    bound
}

/// The other action (of those that exist) already on `chord`.
pub fn holder(shortcuts: &Shortcuts, profiles: &[String], chord: &KeyChord, except: &Action) -> Option<Action> {
    Action::all(profiles)
        .into_iter()
        .filter(|a| a != except)
        .find(|a| chord_of(shortcuts, a).is_some_and(|c| c.conflicts_with(chord)))
}

/// A global hotkey needs Ctrl, Alt or Win, or it would take that key from every other program.
pub fn has_modifier(chord: &KeyChord) -> bool {
    chord.ctrl || chord.raw_ctrl || chord.alt || chord.win
}

/// "Ctrl+Alt+S and Win+F7", or "3 shortcuts" when there are more.
pub fn describe(chords: &[KeyChord]) -> String {
    let names: Vec<String> = chords.iter().map(KeyChord::to_display_string).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second} and {third}"),
        _ => format!("{} shortcuts", names.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> Vec<String> {
        vec!["Studio".into(), "Home".into()]
    }

    #[test]
    fn ids_are_distinct() {
        let ids: std::collections::HashSet<String> = Action::all(&profiles()).iter().map(Action::id).collect();
        assert_eq!(ids.len(), Action::all(&profiles()).len());
    }

    #[test]
    fn set_stores_and_removes() {
        let mut map = Shortcuts::new();
        let chord = KeyChord::parse("Ctrl+Alt+S").unwrap();
        set(&mut map, &Action::ToggleSend, Some(&chord));
        assert_eq!(map.get("toggle_send").map(String::as_str), Some("Ctrl+Alt+S"));
        assert_eq!(chord_of(&map, &Action::ToggleSend), Some(chord));
        set(&mut map, &Action::ToggleSend, None);
        assert!(map.is_empty());
    }

    #[test]
    fn bindings_skip_missing_profiles_and_duplicates() {
        let mut map = Shortcuts::new();
        map.insert("profile:Studio".into(), "Ctrl+Alt+1".into());
        map.insert("profile:Gone".into(), "Ctrl+Alt+2".into());
        map.insert("toggle_send".into(), "Ctrl+Alt+S".into());
        map.insert("send_on".into(), "ctrl+alt+s".into());
        let bound = bindings(&map, &profiles());
        let actions: Vec<Action> = bound.into_iter().map(|(_, a)| a).collect();
        assert_eq!(actions, vec![Action::ToggleSend, Action::Profile("Studio".into())]);
    }

    #[test]
    fn holder_finds_the_other_action() {
        let mut map = Shortcuts::new();
        map.insert("receive_off".into(), "Alt+Win+R".into());
        let chord = KeyChord::parse("Win+Alt+R").unwrap();
        assert_eq!(holder(&map, &profiles(), &chord, &Action::ToggleSend), Some(Action::ReceiveOff));
        assert_eq!(holder(&map, &profiles(), &chord, &Action::ReceiveOff), None);
    }

    #[test]
    fn modifiers_are_required() {
        assert!(!has_modifier(&KeyChord::parse("Shift+F5").unwrap()));
        assert!(has_modifier(&KeyChord::parse("Win+F5").unwrap()));
        assert!(has_modifier(&KeyChord::parse("Ctrl+F5").unwrap()));
    }
}
