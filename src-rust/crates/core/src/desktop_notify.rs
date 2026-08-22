//! Desktop notifications for the moments a session needs the user back.
//!
//! A long turn runs while the user is in another window. The events below are
//! the points where the session either stops and waits, or has nothing left to
//! do, and the terminal alone gives no sign of it.
//!
//! Delivery is best-effort. A machine with no notification daemon, or a
//! terminal without notification permission, must not stall a turn, so a
//! failed send is logged and dropped rather than propagated.

use crate::config::Settings;
use tracing::debug;

/// Longest body we hand to the notification server.
///
/// A plan is thousands of characters and every backend truncates somewhere
/// of its own accord; cutting here keeps that cut predictable.
const MAX_BODY_CHARS: usize = 180;

/// The sound played alongside a notification, when the user asked for one.
///
/// Each platform reads the name through a different vocabulary, and a name it
/// does not recognise leaves the notification silent rather than falling back:
/// macOS resolves it against `/System/Library/Sounds`, Windows parses it into
/// a `tauri-winrt-notification` `Sound`, and the XDG backend passes it as the
/// freedesktop `sound-name` hint.
///
/// So each name is the one that platform calls its own default, rather than a
/// sound picked here: whatever the user set as their alert sound is what they
/// already recognise as a notification. The freedesktop spec has no "default"
/// token, so the generic message sound stands in for one there.
#[cfg(target_os = "macos")]
const NOTIFY_SOUND: &str = "NSUserNotificationDefaultSoundName";
#[cfg(target_os = "windows")]
const NOTIFY_SOUND: &str = "Default";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const NOTIFY_SOUND: &str = "message-new-instant";

/// A moment worth interrupting the user for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEvent {
    /// The model asked a question and the turn is blocked on the answer.
    QuestionAsked,
    /// A plan is waiting for approval.
    PlanReady,
    /// A tool is waiting for permission and the turn is blocked on the answer.
    PermissionRequested,
    /// The turn finished and the prompt is free again.
    TurnComplete,
}

impl NotifyEvent {
    /// Every event, for a caller that has to cover all of them.
    ///
    /// A new variant belongs here as well as in the two matches below. The
    /// matches are exhaustive, so the compiler stops on them first.
    pub const ALL: [Self; 4] = [
        Self::QuestionAsked,
        Self::PlanReady,
        Self::PermissionRequested,
        Self::TurnComplete,
    ];

    /// The notification title.
    fn summary(self) -> &'static str {
        match self {
            Self::QuestionAsked => "MikMik is waiting on an answer",
            Self::PlanReady => "MikMik has a plan ready",
            Self::PermissionRequested => "MikMik is waiting for permission",
            Self::TurnComplete => "MikMik finished",
        }
    }

    /// Whether this event's own setting is on.
    fn enabled_in(self, settings: &Settings) -> bool {
        match self {
            Self::QuestionAsked => settings.notify_on_question,
            Self::PlanReady => settings.notify_on_plan_ready,
            Self::PermissionRequested => settings.notify_on_permission,
            Self::TurnComplete => settings.notify_on_turn_complete,
        }
    }

    /// Switch this event off, for a test that needs one silenced.
    #[cfg(test)]
    fn disable_in(self, settings: &mut Settings) {
        match self {
            Self::QuestionAsked => settings.notify_on_question = false,
            Self::PlanReady => settings.notify_on_plan_ready = false,
            Self::PermissionRequested => settings.notify_on_permission = false,
            Self::TurnComplete => settings.notify_on_turn_complete = false,
        }
    }
}

/// Whether `event` should reach the desktop under `settings`.
///
/// The master switch wins: with `notifications` off, nothing is sent however
/// the per-event settings read.
pub fn should_notify(settings: &Settings, event: NotifyEvent) -> bool {
    settings.notifications && event.enabled_in(settings)
}

/// Whether `event` should also make a sound.
///
/// A sub-setting of [`should_notify`]: an event that is not sent cannot be
/// heard either, and turning the sound off leaves the banner alone.
pub fn should_play_sound(settings: &Settings, event: NotifyEvent) -> bool {
    settings.notify_sound && should_notify(settings, event)
}

/// Send one notification, if the settings allow it.
///
/// Returns without touching the notification server when the event is
/// switched off, so the caller does not have to ask first.
pub fn notify(settings: &Settings, event: NotifyEvent, body: &str) {
    if !should_notify(settings, event) {
        return;
    }

    let summary = event.summary();
    let body = trim_body(body);
    let with_sound = should_play_sound(settings, event);

    // Off the caller's thread: `show()` talks to D-Bus or the platform's
    // notification service, and the caller is usually the TUI event loop,
    // where a blocked frame is visible as a stutter.
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(summary).body(&body);
        if with_sound {
            notification.sound_name(NOTIFY_SOUND);
        }
        if let Err(error) = notification.show() {
            // Not an error the user can act on mid-turn: no daemon, no
            // permission, no session bus. Logged so it is still diagnosable.
            debug!(%error, summary, "desktop notification was not delivered");
        }
    });
}

/// Cut `body` to [`MAX_BODY_CHARS`], on a character boundary, with an ellipsis.
fn trim_body(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let cut: String = body
        .chars()
        .take(MAX_BODY_CHARS.saturating_sub(1))
        .collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings with every notification switch on, sound included.
    fn all_on() -> Settings {
        Settings {
            notifications: true,
            notify_on_question: true,
            notify_on_plan_ready: true,
            notify_on_permission: true,
            notify_on_turn_complete: true,
            notify_sound: true,
            ..Default::default()
        }
    }

    const EVENTS: [NotifyEvent; NotifyEvent::ALL.len()] = NotifyEvent::ALL;

    #[test]
    fn the_master_switch_silences_every_event() {
        let settings = Settings {
            notifications: false,
            ..all_on()
        };
        for event in EVENTS {
            assert!(
                !should_notify(&settings, event),
                "{event:?} escaped the master switch"
            );
        }
    }

    #[test]
    fn each_event_is_switched_off_on_its_own() {
        for event in EVENTS {
            let mut settings = all_on();
            event.disable_in(&mut settings);
            assert!(!should_notify(&settings, event), "{event:?} stayed on");
            // The others are untouched: one switch must not silence a
            // sibling event.
            for other in EVENTS.into_iter().filter(|other| *other != event) {
                assert!(
                    should_notify(&settings, other),
                    "turning off {event:?} also silenced {other:?}"
                );
            }
        }
    }

    #[test]
    fn the_sound_switch_is_on_for_every_event_at_once() {
        let settings = all_on();
        for event in EVENTS {
            assert!(
                should_play_sound(&settings, event),
                "{event:?} was sent without a sound"
            );
        }
    }

    #[test]
    fn silencing_the_sound_leaves_the_notification_alone() {
        let settings = Settings {
            notify_sound: false,
            ..all_on()
        };
        for event in EVENTS {
            assert!(
                !should_play_sound(&settings, event),
                "{event:?} still made a sound"
            );
            assert!(
                should_notify(&settings, event),
                "turning the sound off also silenced {event:?} entirely"
            );
        }
    }

    #[test]
    fn the_master_switch_silences_the_sound_too() {
        // Sound on, notifications off: nothing is delivered, so there is
        // nothing left to make a noise.
        let settings = Settings {
            notifications: false,
            ..all_on()
        };
        for event in EVENTS {
            assert!(
                !should_play_sound(&settings, event),
                "{event:?} made a sound with notifications switched off"
            );
        }
    }

    #[test]
    fn an_event_switched_off_makes_no_sound_of_its_own() {
        for event in EVENTS {
            let mut settings = all_on();
            event.disable_in(&mut settings);
            assert!(
                !should_play_sound(&settings, event),
                "{event:?} was switched off but still made a sound"
            );
            for other in EVENTS.into_iter().filter(|other| *other != event) {
                assert!(
                    should_play_sound(&settings, other),
                    "turning off {event:?} also silenced {other:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_body_is_cut_on_a_character_boundary() {
        // Multi-byte on purpose: a byte-wise cut would panic here.
        let body = "ş".repeat(MAX_BODY_CHARS * 2);
        let trimmed = trim_body(&body);

        assert_eq!(trimmed.chars().count(), MAX_BODY_CHARS);
        assert!(trimmed.ends_with('…'));
    }

    #[test]
    fn a_short_body_is_passed_through_trimmed() {
        assert_eq!(trim_body("  keep me  "), "keep me");
    }
}
