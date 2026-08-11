//! macOS keyboard event construction.
//!
//! V2 builds the signed-helper-compatible sequence here and posts it directly
//! with `CGEventPostToPid`. Legacy helpers below retain their existing
//! SkyLight-first transport until a separate cutover.

use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use cua_driver_core::api::{
    contracts::{KeyStroke, Modifier},
    errors::{ErrorCode, ErrorPhase, NativeError},
};
use foreign_types::ForeignType;

/// A normalized physical modifier key. V2 keeps this typed through cleanup so
/// a partial chord can release every modifier whose down event may have
/// reached the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NormalizedModifier {
    pub modifier: Modifier,
    pub key_code: u16,
    pub flag: CGEventFlags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedChord {
    pub key_code: u16,
    pub modifiers: Vec<NormalizedModifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetedKeyEventKind {
    FlagsChangedRequested,
    KeyDown,
    KeyUp,
    FlagsChangedRestore,
    UnicodeDown,
    UnicodeUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetedKeyEvent {
    pub kind: TargetedKeyEventKind,
    pub key_code: u16,
    pub key_down: bool,
    pub flags: CGEventFlags,
    pub text: Option<String>,
}

pub(crate) struct PreparedTargetedKeyEvent {
    kind: TargetedKeyEventKind,
    native: CGEvent,
}

pub(crate) struct PreparedKeySequence {
    events: Vec<PreparedTargetedKeyEvent>,
}

impl PreparedKeySequence {
    pub(crate) fn events(&self) -> &[PreparedTargetedKeyEvent] {
        &self.events
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }
}

impl PreparedTargetedKeyEvent {
    pub(crate) fn kind(&self) -> TargetedKeyEventKind {
        self.kind
    }

    pub(crate) fn set_fresh_timestamp(&self) {
        unsafe {
            crate::input::synthesized_event::set_event_timestamp(
                self.native.as_ptr() as *mut std::ffi::c_void,
                crate::input::synthesized_event::fresh_event_timestamp(),
            );
        }
    }
}

/// Normalize the complete chord before any interaction lease is acquired.
/// The mapping is the physical US ANSI key vocabulary used by the existing
/// macOS tools. Shifted printable glyphs add Shift explicitly instead of
/// depending on ambient keyboard state.
pub(crate) fn normalize_chord(stroke: &KeyStroke) -> Result<NormalizedChord, NativeError> {
    let (key_code, implied_shift) = normalized_key_code(&stroke.key).ok_or_else(|| {
        NativeError::new(
            ErrorCode::InvalidRequest,
            ErrorPhase::Preflight,
            false,
            format!("unknown macOS key name: {}", stroke.key),
        )
        .with_detail("key", stroke.key.clone())
    })?;

    let mut requested = stroke.modifiers.clone();
    if implied_shift && !requested.contains(&Modifier::Shift) {
        requested.push(Modifier::Shift);
    }
    requested.sort();
    requested.dedup();

    let modifiers: Vec<_> = requested.into_iter().map(normalized_modifier).collect();
    if modifiers
        .iter()
        .any(|modifier| modifier.key_code == key_code)
    {
        return Err(NativeError::new(
            ErrorCode::InvalidRequest,
            ErrorPhase::Preflight,
            false,
            "the main key cannot also be present in the chord modifiers",
        )
        .with_detail("key", stroke.key.clone()));
    }
    Ok(NormalizedChord {
        key_code,
        modifiers,
    })
}

pub(crate) fn chord_events(
    chord: &NormalizedChord,
    restore_flags: CGEventFlags,
) -> [TargetedKeyEvent; 4] {
    let requested_flags = chord
        .modifiers
        .iter()
        .fold(CGEventFlags::CGEventFlagNull, |flags, modifier| {
            flags | modifier.flag
        });
    [
        TargetedKeyEvent {
            kind: TargetedKeyEventKind::FlagsChangedRequested,
            key_code: 0,
            key_down: false,
            flags: requested_flags,
            text: None,
        },
        TargetedKeyEvent {
            kind: TargetedKeyEventKind::KeyDown,
            key_code: chord.key_code,
            key_down: true,
            flags: requested_flags,
            text: None,
        },
        TargetedKeyEvent {
            kind: TargetedKeyEventKind::KeyUp,
            key_code: chord.key_code,
            key_down: false,
            flags: requested_flags,
            text: None,
        },
        TargetedKeyEvent {
            kind: TargetedKeyEventKind::FlagsChangedRestore,
            key_code: 0,
            key_down: false,
            flags: restore_flags,
            text: None,
        },
    ]
}

pub(crate) fn unicode_events(text: &str, restore_flags: CGEventFlags) -> Vec<TargetedKeyEvent> {
    text.chars()
        .flat_map(|character| {
            let text = character.to_string();
            let (key_code, implied_shift) = printable_key_code(character).unwrap_or((0, false));
            let requested_flags = if implied_shift {
                CGEventFlags::CGEventFlagShift
            } else {
                CGEventFlags::CGEventFlagNull
            };
            [
                TargetedKeyEvent {
                    kind: TargetedKeyEventKind::FlagsChangedRequested,
                    key_code: 0,
                    key_down: false,
                    flags: requested_flags,
                    text: None,
                },
                TargetedKeyEvent {
                    kind: TargetedKeyEventKind::UnicodeDown,
                    key_code,
                    key_down: true,
                    flags: requested_flags,
                    text: Some(text.clone()),
                },
                TargetedKeyEvent {
                    kind: TargetedKeyEventKind::UnicodeUp,
                    key_code,
                    key_down: false,
                    flags: requested_flags,
                    text: Some(text),
                },
                TargetedKeyEvent {
                    kind: TargetedKeyEventKind::FlagsChangedRestore,
                    key_code: 0,
                    key_down: false,
                    flags: restore_flags,
                    text: None,
                },
            ]
        })
        .collect()
}

/// Construct the complete native keyboard event without posting it. V2 uses
/// this split so all fallible construction precedes the controller's explicit
/// first-native-side-effect boundary.
pub(crate) fn prepare_targeted_event(
    event: &TargetedKeyEvent,
) -> Result<PreparedTargetedKeyEvent, NativeError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
        NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "CGEventSource creation failed for targeted keyboard dispatch",
        )
    })?;
    let native = match event.kind {
        TargetedKeyEventKind::FlagsChangedRequested | TargetedKeyEventKind::FlagsChangedRestore => {
            let native = CGEvent::new(source).map_err(|_| {
                NativeError::new(
                    ErrorCode::DispatchFailed,
                    ErrorPhase::Dispatch,
                    false,
                    "CGEvent flags-changed event creation failed",
                )
            })?;
            native.set_type(CGEventType::FlagsChanged);
            native
        }
        TargetedKeyEventKind::KeyDown
        | TargetedKeyEventKind::KeyUp
        | TargetedKeyEventKind::UnicodeDown
        | TargetedKeyEventKind::UnicodeUp => {
            CGEvent::new_keyboard_event(source, event.key_code, event.key_down).map_err(|_| {
                NativeError::new(
                    ErrorCode::DispatchFailed,
                    ErrorPhase::Dispatch,
                    false,
                    "CGEvent keyboard event creation failed",
                )
            })?
        }
    };
    native.set_flags(event.flags);
    if let Some(text) = &event.text {
        native.set_string(text);
    }
    Ok(PreparedTargetedKeyEvent {
        kind: event.kind,
        native,
    })
}

pub(crate) fn prepare_chord_sequence(
    chord: &NormalizedChord,
) -> Result<PreparedKeySequence, NativeError> {
    let restore_flags = combined_session_flags();
    prepare_sequence(chord_events(chord, restore_flags))
}

pub(crate) fn prepare_unicode_sequence(text: &str) -> Result<PreparedKeySequence, NativeError> {
    prepare_sequence(unicode_events(text, combined_session_flags()))
}

fn prepare_sequence(
    events: impl IntoIterator<Item = TargetedKeyEvent>,
) -> Result<PreparedKeySequence, NativeError> {
    let events = events
        .into_iter()
        .map(|event| prepare_targeted_event(&event))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedKeySequence { events })
}

/// Post an already-constructed event to one pid. No fallible operation occurs
/// before the posting primitive is attempted.
pub(crate) fn post_prepared_targeted_event(pid: i32, event: &PreparedTargetedKeyEvent) {
    event.native.post_to_pid(pid as libc::pid_t);
}

fn combined_session_flags() -> CGEventFlags {
    let bits = unsafe { CGEventSourceFlagsState(CGEventSourceStateID::CombinedSessionState) };
    CGEventFlags::from_bits_truncate(bits)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: CGEventSourceStateID) -> u64;
}

fn normalized_modifier(modifier: Modifier) -> NormalizedModifier {
    let (key_code, flag) = match modifier {
        Modifier::Shift => (56, CGEventFlags::CGEventFlagShift),
        Modifier::Control => (59, CGEventFlags::CGEventFlagControl),
        Modifier::Alt => (58, CGEventFlags::CGEventFlagAlternate),
        Modifier::Meta => (55, CGEventFlags::CGEventFlagCommand),
    };
    NormalizedModifier {
        modifier,
        key_code,
        flag,
    }
}

fn normalized_key_code(key: &str) -> Option<(u16, bool)> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    if key.eq_ignore_ascii_case("plus") {
        return Some((24, true));
    }
    let named = match key.to_ascii_lowercase().as_str() {
        "return" | "enter" => Some(36),
        "tab" => Some(48),
        "space" => Some(49),
        "delete" | "backspace" => Some(51),
        "escape" | "esc" => Some(53),
        "command" | "cmd" | "meta" => Some(55),
        "shift" => Some(56),
        "capslock" | "caps_lock" => Some(57),
        "option" | "alt" => Some(58),
        "control" | "ctrl" => Some(59),
        "fn" => Some(63),
        "home" => Some(115),
        "pageup" | "page_up" => Some(116),
        "del" | "forward_delete" => Some(117),
        "end" => Some(119),
        "pagedown" | "page_down" => Some(121),
        "left" | "left_arrow" => Some(123),
        "right" | "right_arrow" => Some(124),
        "down" | "down_arrow" => Some(125),
        "up" | "up_arrow" => Some(126),
        "f1" => Some(122),
        "f2" => Some(120),
        "f3" => Some(99),
        "f4" => Some(118),
        "f5" => Some(96),
        "f6" => Some(97),
        "f7" => Some(98),
        "f8" => Some(100),
        "f9" => Some(101),
        "f10" => Some(109),
        "f11" => Some(103),
        "f12" => Some(111),
        "f13" => Some(105),
        "f14" => Some(107),
        "f15" => Some(113),
        "f16" => Some(106),
        "f17" => Some(64),
        "f18" => Some(79),
        "f19" => Some(80),
        "f20" => Some(90),
        _ => None,
    };
    if let Some(code) = named {
        return Some((code, false));
    }

    let mut characters = key.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    printable_key_code(character)
}

fn printable_key_code(character: char) -> Option<(u16, bool)> {
    let base = match character.to_ascii_lowercase() {
        'a' => 0,
        's' => 1,
        'd' => 2,
        'f' => 3,
        'h' => 4,
        'g' => 5,
        'z' => 6,
        'x' => 7,
        'c' => 8,
        'v' => 9,
        'b' => 11,
        'q' => 12,
        'w' => 13,
        'e' => 14,
        'r' => 15,
        'y' => 16,
        't' => 17,
        '1' => 18,
        '2' => 19,
        '3' => 20,
        '4' => 21,
        '6' => 22,
        '5' => 23,
        '=' | '+' => 24,
        '9' => 25,
        '7' => 26,
        '-' | '_' => 27,
        '8' => 28,
        '0' => 29,
        ']' | '}' => 30,
        'o' => 31,
        'u' => 32,
        '[' | '{' => 33,
        'i' => 34,
        'p' => 35,
        'l' => 37,
        'j' => 38,
        '\'' | '"' => 39,
        'k' => 40,
        ';' | ':' => 41,
        '\\' | '|' => 42,
        ',' | '<' => 43,
        '/' | '?' => 44,
        'n' => 45,
        'm' => 46,
        '.' | '>' => 47,
        '`' | '~' => 50,
        '!' => 18,
        '@' => 19,
        '#' => 20,
        '$' => 21,
        '^' => 22,
        '%' => 23,
        '(' => 25,
        '&' => 26,
        '*' => 28,
        ')' => 29,
        ' ' => 49,
        _ => return None,
    };
    let implied_shift = character.is_ascii_uppercase()
        || matches!(
            character,
            '!' | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '_'
                | '+'
                | '{'
                | '}'
                | '|'
                | ':'
                | '"'
                | '<'
                | '>'
                | '?'
                | '~'
        );
    Some((base, implied_shift))
}

/// Press and release a single key, delivered to `pid` without stealing focus.
pub fn press_key(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    // Handle "+" / "plus" → Shift+= (US keyboard layout).
    if key == "+" || key.to_lowercase() == "plus" {
        let flags = modifier_flags(&["shift"]);
        let eq_code = key_name_to_code("=")?;
        post_key(pid, eq_code, true, modifier_flags(modifiers) | flags)?;
        std::thread::sleep(std::time::Duration::from_millis(8));
        post_key(pid, eq_code, false, modifier_flags(modifiers) | flags)?;
        return Ok(());
    }

    let key_code = key_name_to_code(key)?;
    let flags = modifier_flags(modifiers);

    post_key(pid, key_code, true, flags)?;
    std::thread::sleep(std::time::Duration::from_millis(8));
    post_key(pid, key_code, false, flags)?;
    Ok(())
}

/// Type a string character-by-character to `pid`.
pub fn type_text(pid: i32, text: &str) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;

    for ch in text.chars() {
        let ch_str = ch.to_string();
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard down failed"))?;
        down.set_string(&ch_str);
        // Always zero flags: Chrome inspects the flags field to infer modifier
        // state; without this, uppercase chars (e.g. 'E') are seen as Shift+e
        // and the modifier leaks into the next character (Swift fix: event.flags = []).
        down.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &down);
        std::thread::sleep(std::time::Duration::from_millis(8));

        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard up failed"))?;
        up.set_string(&ch_str);
        up.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &up);
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    Ok(())
}

/// Type a string character-by-character with an extra `inter_char_delay_ms`
/// pause after each character (on top of the internal 8 ms down/up gap).
pub fn type_text_with_delay(pid: i32, text: &str, inter_char_delay_ms: u64) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;

    for ch in text.chars() {
        let ch_str = ch.to_string();
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard down failed"))?;
        down.set_string(&ch_str);
        down.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &down);
        std::thread::sleep(std::time::Duration::from_millis(8));

        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard up failed"))?;
        up.set_string(&ch_str);
        up.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &up);

        // Additional inter-character delay on top of the 8 ms internal gap.
        if inter_char_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(inter_char_delay_ms));
        } else {
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
    }
    Ok(())
}

/// Send a key combination (hotkey) to `pid`.
pub fn hotkey(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    press_key(pid, key, modifiers)
}

/// Send a key combination to `pid` WITHOUT the auth-message envelope.
///
/// Required for NSMenu key equivalents: with the envelope, SLEventPostToPid
/// forks onto a direct-mach path that bypasses IOHIDPostEvent — NSMenu never
/// sees those events. Without the envelope the path goes through IOHIDPostEvent
/// so NSApplication.sendEvent: dispatches NSMenu key equivalents.
pub fn hotkey_no_auth(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    let key_code = key_name_to_code(key)?;
    let flags = modifier_flags(modifiers);
    post_key_no_auth(pid, key_code, true, flags)?;
    std::thread::sleep(std::time::Duration::from_millis(8));
    post_key_no_auth(pid, key_code, false, flags)?;
    Ok(())
}

/// Press and release a single key to `pid` WITHOUT the auth-message envelope.
/// Works for single keys as well as combinations (same as hotkey_no_auth for single key).
pub fn press_key_no_auth(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    let key_code = key_name_to_code(key)?;
    let flags = modifier_flags(modifiers);
    post_key_no_auth(pid, key_code, true, flags)?;
    std::thread::sleep(std::time::Duration::from_millis(8));
    post_key_no_auth(pid, key_code, false, flags)?;
    Ok(())
}

/// Post a keyboard event to `pid` via SLEventPostToPid (with auth message for
/// Chromium/Electron support) or fall back to CGEvent::post_to_pid.
fn post_keyboard_event(pid: i32, event: &CGEvent) {
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;
    // attachAuthMessage = true: required for Chromium keyboard on macOS 14+.
    if !crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, true) {
        event.post_to_pid(pid as libc::pid_t);
    }
}

fn post_key(pid: i32, key_code: u16, key_down: bool, flags: CGEventFlags) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let event = CGEvent::new_keyboard_event(source, key_code, key_down)
        .map_err(|_| anyhow::anyhow!("CGEvent::new_keyboard_event failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        event.set_flags(flags);
    }
    post_keyboard_event(pid, &event);
    Ok(())
}

fn post_key_no_auth(
    pid: i32,
    key_code: u16,
    key_down: bool,
    flags: CGEventFlags,
) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let event = CGEvent::new_keyboard_event(source, key_code, key_down)
        .map_err(|_| anyhow::anyhow!("CGEvent::new_keyboard_event failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        event.set_flags(flags);
    }
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;
    // attach_auth_message = false → IOHIDPostEvent path → NSMenu fires
    if !crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false) {
        event.post_to_pid(pid as libc::pid_t);
    }
    Ok(())
}

fn modifier_flags(modifiers: &[&str]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in modifiers {
        match m.to_lowercase().as_str() {
            "cmd" | "command" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "fn" => flags |= CGEventFlags::CGEventFlagSecondaryFn,
            _ => {}
        }
    }
    flags
}

fn key_name_to_code(key: &str) -> anyhow::Result<u16> {
    let code = match key.to_lowercase().as_str() {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" | "backspace" => 51,
        "escape" | "esc" => 53,
        "command" | "cmd" => 55,
        "shift" => 56,
        "capslock" => 57,
        "option" | "alt" => 58,
        "control" | "ctrl" => 59,
        "fn" => 63,
        "home" => 115,
        "pageup" => 116,
        "del" | "forward_delete" => 117,
        "end" => 119,
        "pagedown" => 121,
        "left" | "left_arrow" => 123,
        "right" | "right_arrow" => 124,
        "down" | "down_arrow" => 125,
        "up" | "up_arrow" => 126,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "`" => 50,
        _ => anyhow::bail!("Unknown key name: {key}"),
    };
    Ok(code)
}

#[cfg(test)]
mod v2_tests {
    use super::*;

    #[test]
    fn normalization_canonicalizes_one_chord_and_rejects_unknown_keys() {
        let chord = normalize_chord(&KeyStroke {
            key: "+".to_owned(),
            modifiers: vec![
                Modifier::Meta,
                Modifier::Alt,
                Modifier::Control,
                Modifier::Shift,
                Modifier::Meta,
            ],
        })
        .unwrap();
        assert_eq!(chord.key_code, 24);
        assert_eq!(
            chord
                .modifiers
                .iter()
                .map(|modifier| modifier.modifier)
                .collect::<Vec<_>>(),
            vec![
                Modifier::Shift,
                Modifier::Control,
                Modifier::Alt,
                Modifier::Meta,
            ]
        );

        let restore_flags = CGEventFlags::CGEventFlagAlphaShift;
        let events = chord_events(&chord, restore_flags);
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                TargetedKeyEventKind::FlagsChangedRequested,
                TargetedKeyEventKind::KeyDown,
                TargetedKeyEventKind::KeyUp,
                TargetedKeyEventKind::FlagsChangedRestore,
            ]
        );
        let requested_flags = CGEventFlags::CGEventFlagShift
            | CGEventFlags::CGEventFlagControl
            | CGEventFlags::CGEventFlagAlternate
            | CGEventFlags::CGEventFlagCommand;
        assert_eq!(events[0].flags, requested_flags);
        assert_eq!(events[1].flags, requested_flags);
        assert_eq!(events[2].flags, requested_flags);
        assert_eq!(events[3].flags, restore_flags);

        assert_eq!(
            normalize_chord(&KeyStroke {
                key: "return".to_owned(),
                modifiers: vec![],
            })
            .unwrap()
            .key_code,
            36
        );
        let uppercase = normalize_chord(&KeyStroke {
            key: "A".to_owned(),
            modifiers: vec![],
        })
        .unwrap();
        assert_eq!(uppercase.key_code, 0);
        assert_eq!(uppercase.modifiers[0].modifier, Modifier::Shift);

        let error = normalize_chord(&KeyStroke {
            key: "definitely-not-a-key".to_owned(),
            modifiers: vec![],
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.phase, ErrorPhase::Preflight);
    }
}
