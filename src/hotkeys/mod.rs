use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::config::AppConfig;

/// Action emitted when a registered global hotkey is pressed anywhere on the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    PlaySound(String),
    StopAll,
    PitchUp,
    PitchDown,
    PitchReset,
}

/// Result of inspecting `egui` key events while recording a new shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutCapture {
    Idle,
    Cancelled,
    Captured(String),
}

/// Diagnostic snapshot exposed to the `!` Status Alerts modal.
#[derive(Debug, Clone, Default)]
pub struct HotkeyStatus {
    pub manager_ready: bool,
    pub registered_count: usize,
    pub warnings: Vec<String>,
    pub macos_accessibility_trusted: Option<bool>,
}

static HOTKEY_EVENT_TX: OnceLock<Mutex<Option<(mpsc::Sender<u32>, egui::Context)>>> = OnceLock::new();

fn install_global_event_forwarder(tx: mpsc::Sender<u32>, ctx: egui::Context) {
    let cell = HOTKEY_EVENT_TX.get_or_init(|| {
        GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
            if event.state == HotKeyState::Pressed
                && let Some(slot) = HOTKEY_EVENT_TX.get()
                && let Ok(guard) = slot.lock()
                && let Some((ref sender, ref egui_ctx)) = *guard
                && sender.send(event.id).is_ok()
            {
                egui_ctx.request_repaint();
            }
        }));
        Mutex::new(None)
    });

    if let Ok(mut guard) = cell.lock() {
        *guard = Some((tx, ctx));
    }
}

/// Manages system-wide global hotkeys via `global-hotkey` on the GUI event-loop thread.
pub struct HotkeyService {
    manager: Option<GlobalHotKeyManager>,
    event_rx: mpsc::Receiver<u32>,
    registered_hotkeys: Vec<HotKey>,
    id_to_action: HashMap<u32, HotkeyAction>,
    last_signature: Option<BindingsSignature>,
    status: HotkeyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingsSignature {
    stop_shortcut: Option<String>,
    pitch_up_shortcut: Option<String>,
    pitch_down_shortcut: Option<String>,
    pitch_reset_shortcut: Option<String>,
    sound_shortcuts: Vec<(String, String, Option<String>)>,
}

impl HotkeyService {
    /// Initializes the global hotkey manager on the main GUI/event-loop thread.
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<u32>();
        install_global_event_forwarder(tx, ctx.clone());

        let mut warnings = Vec::new();
        let manager = match GlobalHotKeyManager::new() {
            Ok(mgr) => Some(mgr),
            Err(err) => {
                warnings.push(format!("Global hotkey manager unavailable: {err}"));
                None
            }
        };

        let macos_trust = query_macos_accessibility_trusted();
        if macos_trust == Some(false) {
            warnings.push(
                "macOS Accessibility permission is not granted yet. Open System Settings -> Privacy & Security -> Accessibility and allow WaweFX for global hotkeys."
                    .to_string(),
            );
        }

        Self {
            manager,
            event_rx: rx,
            registered_hotkeys: Vec::new(),
            id_to_action: HashMap::new(),
            last_signature: None,
            status: HotkeyStatus {
                manager_ready: manager_is_some(&warnings),
                registered_count: 0,
                warnings,
                macos_accessibility_trusted: macos_trust,
            },
        }
    }

    /// Re-queries macOS Accessibility permission status and updates status warnings.
    pub fn refresh_permissions(&mut self) {
        self.status.macos_accessibility_trusted = query_macos_accessibility_trusted();
        self.last_signature = None;
    }

    /// Re-registers global hotkeys whenever sound or global shortcuts change in [`AppConfig`].
    pub fn sync_bindings(&mut self, config: &AppConfig) {
        let sig = BindingsSignature {
            stop_shortcut: config.stop_shortcut.clone(),
            pitch_up_shortcut: config.pitch_up_shortcut.clone(),
            pitch_down_shortcut: config.pitch_down_shortcut.clone(),
            pitch_reset_shortcut: config.pitch_reset_shortcut.clone(),
            sound_shortcuts: config
                .sounds
                .iter()
                .map(|s| (s.id.clone(), s.title.clone(), s.shortcut.clone()))
                .collect(),
        };

        if self.last_signature.as_ref() == Some(&sig) {
            return;
        }
        self.last_signature = Some(sig);

        let Some(ref manager) = self.manager else {
            return;
        };

        if !self.registered_hotkeys.is_empty() {
            let _ = manager.unregister_all(&self.registered_hotkeys);
            self.registered_hotkeys.clear();
        }
        self.id_to_action.clear();

        let mut warnings = Vec::new();
        self.status.macos_accessibility_trusted = query_macos_accessibility_trusted();
        let mut has_any_shortcut = false;

        let mut register_binding = |raw: &str, owner_label: &str, action: HotkeyAction| {
            has_any_shortcut = true;
            match parse_shortcut(raw) {
                Ok(hotkey) => {
                    if self.id_to_action.contains_key(&hotkey.id()) {
                        warnings.push(format!(
                            "Skipped duplicate shortcut '{raw}' on {owner_label}."
                        ));
                        return;
                    }
                    match manager.register(hotkey) {
                        Ok(()) => {
                            self.id_to_action.insert(hotkey.id(), action);
                            self.registered_hotkeys.push(hotkey);
                        }
                        Err(err) => warnings.push(format!(
                            "Could not register {owner_label} shortcut '{raw}' (already in use or blocked by OS): {err}"
                        )),
                    }
                }
                Err(err) => {
                    warnings.push(format!("Invalid {owner_label} shortcut '{raw}': {err}"))
                }
            }
        };

        for (slot, label, action) in [
            (&config.stop_shortcut, "Stop All", HotkeyAction::StopAll),
            (&config.pitch_up_shortcut, "Pitch Up", HotkeyAction::PitchUp),
            (&config.pitch_down_shortcut, "Pitch Down", HotkeyAction::PitchDown),
            (&config.pitch_reset_shortcut, "Pitch Reset", HotkeyAction::PitchReset),
        ] {
            if let Some(raw) = slot {
                register_binding(raw, label, action);
            }
        }

        for sound in &config.sounds {
            if let Some(ref raw) = sound.shortcut {
                let owner = format!("'{}'", sound.title);
                register_binding(raw, &owner, HotkeyAction::PlaySound(sound.id.clone()));
            }
        }

        if has_any_shortcut && self.status.macos_accessibility_trusted == Some(false) {
            warnings.push(
                "macOS Accessibility permission is required for global hotkeys. Enable WaweFX in System Settings -> Privacy & Security -> Accessibility."
                    .to_string(),
            );
        }

        self.status.manager_ready = true;
        self.status.registered_count = self.registered_hotkeys.len();
        self.status.warnings = warnings;
    }

    /// Drains any triggered global hotkey actions since the previous frame.
    pub fn poll_triggered(&self) -> Vec<HotkeyAction> {
        let mut actions = Vec::new();
        while let Ok(id) = self.event_rx.try_recv() {
            if let Some(action) = self.id_to_action.get(&id) {
                actions.push(action.clone());
            }
        }
        actions
    }

    /// Current hotkey registration and permission diagnostics.
    pub fn status(&self) -> &HotkeyStatus {
        &self.status
    }
}

fn manager_is_some(warnings: &[String]) -> bool {
    !warnings
        .iter()
        .any(|w| w.starts_with("Global hotkey manager unavailable"))
}

/// Checks macOS Accessibility trust (`AXIsProcessTrusted`) without extra dependencies.
fn query_macos_accessibility_trusted() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn AXIsProcessTrusted() -> u8;
        }
        // SAFETY: AXIsProcessTrusted takes no arguments and returns a CoreFoundation Boolean (u8).
        let trusted = unsafe { AXIsProcessTrusted() } != 0;
        Some(trusted)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Opens macOS Privacy & Security -> Accessibility settings when running on macOS.
pub fn open_macos_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

/// Parses a human-readable shortcut string (e.g. `"Ctrl+Shift+1"`, `"F9"`, `"Numpad1"`, `"Alt+Q"`)
/// into a [`global_hotkey::hotkey::HotKey`].
pub fn parse_shortcut(raw: &str) -> Result<HotKey, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Shortcut cannot be empty.".to_string());
    }

    let mut mods = Modifiers::empty();
    let mut main_key: Option<Code> = None;

    for part in trimmed.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return Err("Empty token in shortcut.".to_string());
        }

        if let Some(m) = parse_modifier_token(token) {
            if main_key.is_some() {
                return Err("Modifiers must appear before the main key.".to_string());
            }
            mods |= m;
            continue;
        }

        let code = parse_key_token(token)
            .ok_or_else(|| format!("Unsupported key '{token}'."))?;
        if main_key.is_some() {
            return Err("Only one non-modifier key is allowed per shortcut.".to_string());
        }
        main_key = Some(code);
    }

    let key = main_key.ok_or_else(|| "Shortcut must include a non-modifier key.".to_string())?;
    let opt_mods = if mods.is_empty() { None } else { Some(mods) };
    Ok(HotKey::new(opt_mods, key))
}

/// Validates and returns the canonical display representation of a shortcut string.
pub fn normalize_shortcut(raw: &str) -> Result<String, String> {
    let hk = parse_shortcut(raw)?;
    Ok(format_hotkey(hk.mods, hk.key))
}

/// Canonical uppercase comparison key used to detect conflicting shortcut bindings.
pub fn canonical_shortcut_key(raw: &str) -> Option<String> {
    normalize_shortcut(raw)
        .ok()
        .map(|s| s.to_ascii_uppercase())
}

/// Formats a shortcut string as a compact uppercase card badge (`"Ctrl+Shift+1"` -> `"CTRL+SHIFT+1"`, `"Numpad1"` -> `"NUM1"`).
pub fn badge_label(raw: &str) -> String {
    let formatted = normalize_shortcut(raw).unwrap_or_else(|_| raw.trim().to_string());
    formatted
        .to_ascii_uppercase()
        .replace("NUMPAD", "NUM")
}

/// Captures the next non-modifier key combination pressed inside `egui` while recording a shortcut.
pub fn capture_egui_shortcut(ctx: &egui::Context) -> ShortcutCapture {
    ctx.input(|i| {
        for event in &i.events {
            if let egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } = event
            {
                if *key == egui::Key::Escape && !any_egui_mod(*modifiers) {
                    return ShortcutCapture::Cancelled;
                }
                if let Some(code) = egui_key_to_code(*key) {
                    let hk_mods = egui_mods_to_hotkey_mods(*modifiers);
                    return ShortcutCapture::Captured(format_hotkey(hk_mods, code));
                }
            }
        }
        ShortcutCapture::Idle
    })
}

fn any_egui_mod(m: egui::Modifiers) -> bool {
    m.alt || m.ctrl || m.shift || m.mac_cmd
}

fn egui_mods_to_hotkey_mods(m: egui::Modifiers) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.ctrl {
        out |= Modifiers::CONTROL;
    }
    if m.mac_cmd {
        out |= Modifiers::SUPER;
    }
    if m.alt {
        out |= Modifiers::ALT;
    }
    if m.shift {
        out |= Modifiers::SHIFT;
    }
    out
}

fn parse_modifier_token(token: &str) -> Option<Modifiers> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Modifiers::CONTROL),
        "cmd" | "command" | "super" | "meta" | "win" => Some(Modifiers::SUPER),
        "alt" | "option" | "opt" => Some(Modifiers::ALT),
        "shift" => Some(Modifiers::SHIFT),
        _ => None,
    }
}

fn parse_key_token(token: &str) -> Option<Code> {
    let lower = token.to_ascii_lowercase();
    if lower.len() == 1 {
        let ch = lower.bytes().next()?;
        return match ch {
            b'a'..=b'z' => Some(match ch {
                b'a' => Code::KeyA,
                b'b' => Code::KeyB,
                b'c' => Code::KeyC,
                b'd' => Code::KeyD,
                b'e' => Code::KeyE,
                b'f' => Code::KeyF,
                b'g' => Code::KeyG,
                b'h' => Code::KeyH,
                b'i' => Code::KeyI,
                b'j' => Code::KeyJ,
                b'k' => Code::KeyK,
                b'l' => Code::KeyL,
                b'm' => Code::KeyM,
                b'n' => Code::KeyN,
                b'o' => Code::KeyO,
                b'p' => Code::KeyP,
                b'q' => Code::KeyQ,
                b'r' => Code::KeyR,
                b's' => Code::KeyS,
                b't' => Code::KeyT,
                b'u' => Code::KeyU,
                b'v' => Code::KeyV,
                b'w' => Code::KeyW,
                b'x' => Code::KeyX,
                b'y' => Code::KeyY,
                b'z' => Code::KeyZ,
                _ => return None,
            }),
            b'0'..=b'9' => Some(match ch {
                b'0' => Code::Digit0,
                b'1' => Code::Digit1,
                b'2' => Code::Digit2,
                b'3' => Code::Digit3,
                b'4' => Code::Digit4,
                b'5' => Code::Digit5,
                b'6' => Code::Digit6,
                b'7' => Code::Digit7,
                b'8' => Code::Digit8,
                b'9' => Code::Digit9,
                _ => return None,
            }),
            b'-' => Some(Code::Minus),
            b'=' => Some(Code::Equal),
            b'`' => Some(Code::Backquote),
            b'[' => Some(Code::BracketLeft),
            b']' => Some(Code::BracketRight),
            b'\\' => Some(Code::Backslash),
            b';' => Some(Code::Semicolon),
            b'\'' => Some(Code::Quote),
            b',' => Some(Code::Comma),
            b'.' => Some(Code::Period),
            b'/' => Some(Code::Slash),
            _ => None,
        };
    }

    match lower.as_str() {
        "f1" => Some(Code::F1),
        "f2" => Some(Code::F2),
        "f3" => Some(Code::F3),
        "f4" => Some(Code::F4),
        "f5" => Some(Code::F5),
        "f6" => Some(Code::F6),
        "f7" => Some(Code::F7),
        "f8" => Some(Code::F8),
        "f9" => Some(Code::F9),
        "f10" => Some(Code::F10),
        "f11" => Some(Code::F11),
        "f12" => Some(Code::F12),
        "f13" => Some(Code::F13),
        "f14" => Some(Code::F14),
        "f15" => Some(Code::F15),
        "f16" => Some(Code::F16),
        "f17" => Some(Code::F17),
        "f18" => Some(Code::F18),
        "f19" => Some(Code::F19),
        "f20" => Some(Code::F20),
        "f21" => Some(Code::F21),
        "f22" => Some(Code::F22),
        "f23" => Some(Code::F23),
        "f24" => Some(Code::F24),
        "numpad0" | "num0" | "kp0" => Some(Code::Numpad0),
        "numpad1" | "num1" | "kp1" => Some(Code::Numpad1),
        "numpad2" | "num2" | "kp2" => Some(Code::Numpad2),
        "numpad3" | "num3" | "kp3" => Some(Code::Numpad3),
        "numpad4" | "num4" | "kp4" => Some(Code::Numpad4),
        "numpad5" | "num5" | "kp5" => Some(Code::Numpad5),
        "numpad6" | "num6" | "kp6" => Some(Code::Numpad6),
        "numpad7" | "num7" | "kp7" => Some(Code::Numpad7),
        "numpad8" | "num8" | "kp8" => Some(Code::Numpad8),
        "numpad9" | "num9" | "kp9" => Some(Code::Numpad9),
        "numpadadd" | "numadd" | "numplus" | "kpadd" => Some(Code::NumpadAdd),
        "numpadsubtract" | "numsubtract" | "numsub" | "kpsubtract" => Some(Code::NumpadSubtract),
        "numpadmultiply" | "nummultiply" | "nummul" | "kpmultiply" => Some(Code::NumpadMultiply),
        "numpaddivide" | "numdivide" | "numdiv" | "kpdivide" => Some(Code::NumpadDivide),
        "numpadenter" | "numenter" | "kpenter" => Some(Code::NumpadEnter),
        "numpaddecimal" | "numdecimal" | "numdot" | "kpdecimal" => Some(Code::NumpadDecimal),
        "pause" | "break" => Some(Code::Pause),
        "scrolllock" | "scrlk" => Some(Code::ScrollLock),
        "printscreen" | "prtsc" | "prtscn" => Some(Code::PrintScreen),
        "mediaplaypause" | "playpause" => Some(Code::MediaPlayPause),
        "mediastop" => Some(Code::MediaStop),
        "mediatracknext" | "medianext" | "nexttrack" => Some(Code::MediaTrackNext),
        "mediatrackprevious" | "mediaprev" | "prevtrack" => Some(Code::MediaTrackPrevious),
        "space" => Some(Code::Space),
        "enter" | "return" => Some(Code::Enter),
        "tab" => Some(Code::Tab),
        "backspace" => Some(Code::Backspace),
        "insert" | "ins" => Some(Code::Insert),
        "delete" | "del" => Some(Code::Delete),
        "home" => Some(Code::Home),
        "end" => Some(Code::End),
        "pageup" | "pgup" => Some(Code::PageUp),
        "pagedown" | "pgdn" => Some(Code::PageDown),
        "up" | "arrowup" => Some(Code::ArrowUp),
        "down" | "arrowdown" => Some(Code::ArrowDown),
        "left" | "arrowleft" => Some(Code::ArrowLeft),
        "right" | "arrowright" => Some(Code::ArrowRight),
        "minus" => Some(Code::Minus),
        "equal" | "equals" => Some(Code::Equal),
        "backquote" | "backtick" | "grave" => Some(Code::Backquote),
        "openbracket" | "bracketleft" => Some(Code::BracketLeft),
        "closebracket" | "bracketright" => Some(Code::BracketRight),
        "backslash" => Some(Code::Backslash),
        "semicolon" => Some(Code::Semicolon),
        "quote" => Some(Code::Quote),
        "comma" => Some(Code::Comma),
        "period" => Some(Code::Period),
        "slash" => Some(Code::Slash),
        _ => None,
    }
}

fn egui_key_to_code(key: egui::Key) -> Option<Code> {
    use egui::Key as K;
    Some(match key {
        K::A => Code::KeyA,
        K::B => Code::KeyB,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::E => Code::KeyE,
        K::F => Code::KeyF,
        K::G => Code::KeyG,
        K::H => Code::KeyH,
        K::I => Code::KeyI,
        K::J => Code::KeyJ,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::M => Code::KeyM,
        K::N => Code::KeyN,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::Q => Code::KeyQ,
        K::R => Code::KeyR,
        K::S => Code::KeyS,
        K::T => Code::KeyT,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Num0 => Code::Digit0,
        K::Num1 => Code::Digit1,
        K::Num2 => Code::Digit2,
        K::Num3 => Code::Digit3,
        K::Num4 => Code::Digit4,
        K::Num5 => Code::Digit5,
        K::Num6 => Code::Digit6,
        K::Num7 => Code::Digit7,
        K::Num8 => Code::Digit8,
        K::Num9 => Code::Digit9,
        K::F1 => Code::F1,
        K::F2 => Code::F2,
        K::F3 => Code::F3,
        K::F4 => Code::F4,
        K::F5 => Code::F5,
        K::F6 => Code::F6,
        K::F7 => Code::F7,
        K::F8 => Code::F8,
        K::F9 => Code::F9,
        K::F10 => Code::F10,
        K::F11 => Code::F11,
        K::F12 => Code::F12,
        K::F13 => Code::F13,
        K::F14 => Code::F14,
        K::F15 => Code::F15,
        K::F16 => Code::F16,
        K::F17 => Code::F17,
        K::F18 => Code::F18,
        K::F19 => Code::F19,
        K::F20 => Code::F20,
        K::F21 => Code::F21,
        K::F22 => Code::F22,
        K::F23 => Code::F23,
        K::F24 => Code::F24,
        K::Space => Code::Space,
        K::Enter => Code::Enter,
        K::Tab => Code::Tab,
        K::Backspace => Code::Backspace,
        K::Insert => Code::Insert,
        K::Delete => Code::Delete,
        K::Home => Code::Home,
        K::End => Code::End,
        K::PageUp => Code::PageUp,
        K::PageDown => Code::PageDown,
        K::ArrowUp => Code::ArrowUp,
        K::ArrowDown => Code::ArrowDown,
        K::ArrowLeft => Code::ArrowLeft,
        K::ArrowRight => Code::ArrowRight,
        K::Minus => Code::Minus,
        K::Equals | K::Plus => Code::Equal,
        K::Backtick => Code::Backquote,
        K::OpenBracket => Code::BracketLeft,
        K::CloseBracket => Code::BracketRight,
        K::Backslash => Code::Backslash,
        K::Semicolon => Code::Semicolon,
        K::Quote => Code::Quote,
        K::Comma => Code::Comma,
        K::Period => Code::Period,
        K::Slash => Code::Slash,
        _ => return None,
    })
}

fn format_hotkey(mods: Modifiers, key: Code) -> String {
    let mut parts: Vec<&'static str> = Vec::with_capacity(5);
    if mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if mods.contains(Modifiers::SUPER) {
        #[cfg(target_os = "macos")]
        parts.push("Cmd");
        #[cfg(not(target_os = "macos"))]
        parts.push("Super");
    }
    if mods.contains(Modifiers::ALT) {
        parts.push("Alt");
    }
    if mods.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }

    let key_str = code_display_name(key);
    if parts.is_empty() {
        key_str.to_string()
    } else {
        format!("{}+{key_str}", parts.join("+"))
    }
}

fn code_display_name(code: Code) -> &'static str {
    match code {
        Code::KeyA => "A",
        Code::KeyB => "B",
        Code::KeyC => "C",
        Code::KeyD => "D",
        Code::KeyE => "E",
        Code::KeyF => "F",
        Code::KeyG => "G",
        Code::KeyH => "H",
        Code::KeyI => "I",
        Code::KeyJ => "J",
        Code::KeyK => "K",
        Code::KeyL => "L",
        Code::KeyM => "M",
        Code::KeyN => "N",
        Code::KeyO => "O",
        Code::KeyP => "P",
        Code::KeyQ => "Q",
        Code::KeyR => "R",
        Code::KeyS => "S",
        Code::KeyT => "T",
        Code::KeyU => "U",
        Code::KeyV => "V",
        Code::KeyW => "W",
        Code::KeyX => "X",
        Code::KeyY => "Y",
        Code::KeyZ => "Z",
        Code::Digit0 => "0",
        Code::Digit1 => "1",
        Code::Digit2 => "2",
        Code::Digit3 => "3",
        Code::Digit4 => "4",
        Code::Digit5 => "5",
        Code::Digit6 => "6",
        Code::Digit7 => "7",
        Code::Digit8 => "8",
        Code::Digit9 => "9",
        Code::F1 => "F1",
        Code::F2 => "F2",
        Code::F3 => "F3",
        Code::F4 => "F4",
        Code::F5 => "F5",
        Code::F6 => "F6",
        Code::F7 => "F7",
        Code::F8 => "F8",
        Code::F9 => "F9",
        Code::F10 => "F10",
        Code::F11 => "F11",
        Code::F12 => "F12",
        Code::F13 => "F13",
        Code::F14 => "F14",
        Code::F15 => "F15",
        Code::F16 => "F16",
        Code::F17 => "F17",
        Code::F18 => "F18",
        Code::F19 => "F19",
        Code::F20 => "F20",
        Code::F21 => "F21",
        Code::F22 => "F22",
        Code::F23 => "F23",
        Code::F24 => "F24",
        Code::Numpad0 => "Numpad0",
        Code::Numpad1 => "Numpad1",
        Code::Numpad2 => "Numpad2",
        Code::Numpad3 => "Numpad3",
        Code::Numpad4 => "Numpad4",
        Code::Numpad5 => "Numpad5",
        Code::Numpad6 => "Numpad6",
        Code::Numpad7 => "Numpad7",
        Code::Numpad8 => "Numpad8",
        Code::Numpad9 => "Numpad9",
        Code::NumpadAdd => "NumpadAdd",
        Code::NumpadSubtract => "NumpadSubtract",
        Code::NumpadMultiply => "NumpadMultiply",
        Code::NumpadDivide => "NumpadDivide",
        Code::NumpadEnter => "NumpadEnter",
        Code::NumpadDecimal => "NumpadDecimal",
        Code::Pause => "Pause",
        Code::ScrollLock => "ScrollLock",
        Code::PrintScreen => "PrintScreen",
        Code::MediaPlayPause => "MediaPlayPause",
        Code::MediaStop => "MediaStop",
        Code::MediaTrackNext => "MediaTrackNext",
        Code::MediaTrackPrevious => "MediaTrackPrevious",
        Code::Space => "Space",
        Code::Enter => "Enter",
        Code::Tab => "Tab",
        Code::Backspace => "Backspace",
        Code::Insert => "Insert",
        Code::Delete => "Delete",
        Code::Home => "Home",
        Code::End => "End",
        Code::PageUp => "PageUp",
        Code::PageDown => "PageDown",
        Code::ArrowUp => "Up",
        Code::ArrowDown => "Down",
        Code::ArrowLeft => "Left",
        Code::ArrowRight => "Right",
        Code::Minus => "-",
        Code::Equal => "=",
        Code::Backquote => "`",
        Code::BracketLeft => "[",
        Code::BracketRight => "]",
        Code::Backslash => "\\",
        Code::Semicolon => ";",
        Code::Quote => "'",
        Code::Comma => ",",
        Code::Period => ".",
        Code::Slash => "/",
        _ => "Key",
    }
}

#[cfg(test)]
mod tests {
    use global_hotkey::hotkey::{Code, Modifiers};

    use super::{badge_label, canonical_shortcut_key, normalize_shortcut, parse_shortcut};

    #[test]
    fn parses_and_normalizes_requested_shortcut_formats() {
        let hk1 = parse_shortcut("Ctrl+Shift+1").expect("Ctrl+Shift+1");
        assert_eq!(hk1.mods, Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(hk1.key, Code::Digit1);
        assert_eq!(normalize_shortcut("shift + ctrl + 1").unwrap(), "Ctrl+Shift+1");

        let hk2 = parse_shortcut("F9").expect("F9");
        assert_eq!(hk2.mods, Modifiers::empty());
        assert_eq!(hk2.key, Code::F9);

        let hk3 = parse_shortcut("Numpad1").expect("Numpad1");
        assert_eq!(hk3.key, Code::Numpad1);

        let hk4 = parse_shortcut("Alt+Q").expect("Alt+Q");
        assert_eq!(hk4.mods, Modifiers::ALT);
        assert_eq!(hk4.key, Code::KeyQ);

        let hk5 = parse_shortcut("Cmd+F12").expect("Cmd+F12");
        assert_eq!(hk5.mods, Modifiers::SUPER);
        assert_eq!(hk5.key, Code::F12);

        assert_eq!(
            canonical_shortcut_key("alt + q"),
            canonical_shortcut_key("Alt+Q")
        );
    }

    #[test]
    fn rejects_invalid_or_modifier_only_shortcuts_cleanly() {
        assert!(parse_shortcut("").is_err());
        assert!(parse_shortcut("Ctrl+Shift").is_err());
        assert!(parse_shortcut("Ctrl+").is_err());
        assert!(parse_shortcut("A+Ctrl").is_err());
        assert!(parse_shortcut("Ctrl+A+B").is_err());
        assert!(parse_shortcut("Ctrl+UnknownKey").is_err());
    }

    #[test]
    fn formats_compact_card_badges() {
        assert_eq!(badge_label("Ctrl+Shift+1"), "CTRL+SHIFT+1");
        assert_eq!(badge_label("F9"), "F9");
        assert_eq!(badge_label("F24"), "F24");
        assert_eq!(badge_label("Numpad1"), "NUM1");
    }

    #[test]
    fn parses_extended_function_numpad_media_and_navigation_keys() {
        for n in 13..=24 {
            let label = format!("F{n}");
            let hk = parse_shortcut(&label).expect("F13..F24 should parse");
            assert_eq!(normalize_shortcut(&label.to_lowercase()).unwrap(), label);
            assert_eq!(super::code_display_name(hk.key), label);
        }

        assert_eq!(parse_shortcut("Ctrl+F20").unwrap().key, Code::F20);
        assert_eq!(parse_shortcut("NumpadAdd").unwrap().key, Code::NumpadAdd);
        assert_eq!(parse_shortcut("NumpadSubtract").unwrap().key, Code::NumpadSubtract);
        assert_eq!(parse_shortcut("NumpadMultiply").unwrap().key, Code::NumpadMultiply);
        assert_eq!(parse_shortcut("NumpadDivide").unwrap().key, Code::NumpadDivide);
        assert_eq!(parse_shortcut("NumpadEnter").unwrap().key, Code::NumpadEnter);
        assert_eq!(parse_shortcut("NumpadDecimal").unwrap().key, Code::NumpadDecimal);
        assert_eq!(parse_shortcut("Pause").unwrap().key, Code::Pause);
        assert_eq!(parse_shortcut("ScrollLock").unwrap().key, Code::ScrollLock);
        assert_eq!(parse_shortcut("PrintScreen").unwrap().key, Code::PrintScreen);
        assert_eq!(parse_shortcut("MediaPlayPause").unwrap().key, Code::MediaPlayPause);
        assert_eq!(parse_shortcut("Insert").unwrap().key, Code::Insert);
        assert_eq!(parse_shortcut("PageDown").unwrap().key, Code::PageDown);
        assert_eq!(parse_shortcut("BracketLeft").unwrap().key, Code::BracketLeft);

        assert_eq!(super::egui_key_to_code(eframe::egui::Key::F13), Some(Code::F13));
        assert_eq!(super::egui_key_to_code(eframe::egui::Key::F24), Some(Code::F24));
    }
}
