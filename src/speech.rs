//! Speaking through the running screen reader (NVDA, JAWS, Narrator...) with prismer.
//!
//! Only for things the focus doesn't already say: an item ticked in one list showing up in
//! another, a rename, a peer connecting or dropping, a toggle made from the tray menu.
//! Screen readers only: if none is running, nothing is said (prism would otherwise fall back
//! to a plain Windows voice talking over the user's own setup).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use prismer::{Backend, Prism};

/// Plain voices prism offers besides screen readers.
const VOICES: [&str; 4] = ["SAPI", "OneCore", "AVSpeech", "SpeechDispatcher"];
const RESELECT_EVERY: Duration = Duration::from_secs(10);

struct Speaker {
    prism: &'static Prism,
    backend: Option<Backend<'static>>,
    last_attempt: Option<Instant>,
}

impl Speaker {
    fn select(&mut self) {
        self.last_attempt = Some(Instant::now());
        let mut ids = self.prism.backend_ids();
        ids.sort_by_key(|id| -self.prism.backend_priority(*id));
        self.backend = ids.into_iter().find_map(|id| {
            let name = self.prism.backend_name(id)?;
            if VOICES.iter().any(|v| name.eq_ignore_ascii_case(v)) {
                return None;
            }
            let backend = self.prism.create(id).ok()?;
            backend.initialize().ok()?;
            Some(backend)
        });
    }
}

thread_local! {
    static SPEAKER: RefCell<Option<Speaker>> = RefCell::new(
        Prism::new().ok().map(|prism| Speaker { prism: Box::leak(Box::new(prism)), backend: None, last_attempt: None })
    );
}

/// Queues `text` after whatever the screen reader is saying (speech and braille).
/// Call from the GUI thread.
pub fn say(text: &str) {
    SPEAKER.with(|speaker| {
        let mut speaker = speaker.borrow_mut();
        let Some(speaker) = speaker.as_mut() else { return };
        if speaker.backend.is_none() && speaker.last_attempt.is_none_or(|t| t.elapsed() >= RESELECT_EVERY) {
            speaker.select();
        }
        let failed = speaker.backend.as_ref().is_some_and(|b| b.output(text, false).is_err());
        if failed {
            // The screen reader quit or changed: pick again and try once more.
            speaker.select();
            if let Some(backend) = &speaker.backend {
                let _ = backend.output(text, false);
            }
        }
    });
}
