//! Key -> pad mapping, shared across every emulated system.
//!
//! Z = primary button (SMS 1 / NES A / Genesis B / SNES B / GBA A),
//! X = secondary (SMS 2 / NES B / Genesis C / SNES A / GBA B),
//! A = third (Genesis A / SNES Y), S = fourth (SNES X),
//! C / V = shoulder L / R (SNES, GBA), ENTER = Start/Pause,
//! SPACE = Select. Per-machine fan-out into core input structs is emu.rs.

use crate::keys::Key;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    One,
    Two,
    Pause,
    Three,
    Select,
    Four,
    L,
    R,
}

pub const BUTTON_COUNT: usize = 12;

pub fn button_index(b: Button) -> usize {
    match b {
        Button::Up => 0,
        Button::Down => 1,
        Button::Left => 2,
        Button::Right => 3,
        Button::One => 4,
        Button::Two => 5,
        Button::Pause => 6,
        Button::Three => 7,
        Button::Select => 8,
        Button::Four => 9,
        Button::L => 10,
        Button::R => 11,
    }
}

pub fn map_key_to_button(key: &Key) -> Option<Button> {
    match key {
        Key::Up => Some(Button::Up),
        Key::Down => Some(Button::Down),
        Key::Left => Some(Button::Left),
        Key::Right => Some(Button::Right),
        Key::Char('z') | Key::Char('Z') => Some(Button::One),
        Key::Char('x') | Key::Char('X') => Some(Button::Two),
        Key::Char('a') | Key::Char('A') => Some(Button::Three),
        Key::Char('s') | Key::Char('S') => Some(Button::Four),
        Key::Char('c') | Key::Char('C') => Some(Button::L),
        Key::Char('v') | Key::Char('V') => Some(Button::R),
        Key::Char(' ') => Some(Button::Select),
        Key::Enter => Some(Button::Pause),
        _ => None,
    }
}

// evdev codes (CTerm physical-key reports use these)
pub const EVDEV_ESC: u16 = 1;
pub const EVDEV_Z: u16 = 44;
pub const EVDEV_X: u16 = 45;
pub const EVDEV_A: u16 = 30;
pub const EVDEV_S: u16 = 31;
pub const EVDEV_C: u16 = 46;
pub const EVDEV_V: u16 = 47;
pub const EVDEV_SPACE: u16 = 57;
pub const EVDEV_Q: u16 = 16;
pub const EVDEV_ENTER: u16 = 28;
pub const EVDEV_5: u16 = 6;
pub const EVDEV_8: u16 = 9;
/// '2' / '0' — the in-game controller-port keys (2 = let player 2 in /
/// toggle the port, 0 = decline a knock). Never mapped to pad buttons.
pub const EVDEV_2: u16 = 3;
pub const EVDEV_0: u16 = 11;
/// '`' — chat compose (the edge protocols are switched off while typing so
/// the compose line sees plain chars; re-enabled when the compose closes).
pub const EVDEV_GRAVE: u16 = 41;
pub const EVDEV_UP: u16 = 103;
pub const EVDEV_LEFT: u16 = 105;
pub const EVDEV_RIGHT: u16 = 106;
pub const EVDEV_DOWN: u16 = 108;
pub const EVDEV_KP_ENTER: u16 = 96;

pub fn evdev_to_button(code: u16) -> Option<Button> {
    match code {
        EVDEV_UP => Some(Button::Up),
        EVDEV_DOWN => Some(Button::Down),
        EVDEV_LEFT => Some(Button::Left),
        EVDEV_RIGHT => Some(Button::Right),
        EVDEV_Z => Some(Button::One),
        EVDEV_X => Some(Button::Two),
        EVDEV_A => Some(Button::Three),
        EVDEV_S => Some(Button::Four),
        EVDEV_C => Some(Button::L),
        EVDEV_V => Some(Button::R),
        EVDEV_SPACE => Some(Button::Select),
        EVDEV_ENTER | EVDEV_KP_ENTER => Some(Button::Pause),
        _ => None,
    }
}

/// Compact per-machine in-game key hint for the status bar.
pub fn game_key_hint(machine: crate::emu::Machine) -> &'static str {
    use crate::emu::Machine::*;
    match machine {
        // SMS/SG-1000 pads have no start button: games start with button 1
        // (labeled "1/START" on the real pad). ENTER is the console PAUSE.
        MasterSystem | Sg1000 => "Z:1/START X:2 ENTER:pause",
        GameGear | GameGearExpanded => "Z:1 X:2 ENTER:start",
        // 6-button pad, SNES-aligned (Street Fighter II throws the same
        // move on the same key on both systems).
        Genesis => "ZXV:ABC ASC:XYZ ENTER:start",
        Nes => "Z:A X:B SPC:sel ENTER:start",
        Snes => "Z:B X:A A:Y S:X C:L V:R SPC:sel ENTER:start",
        Gba => "Z:A X:B C:L V:R SPC:sel ENTER:start",
        Pce => "Z:I X:II SPC:sel ENTER:run",
    }
}

/// Long-form controls for the menu's help page, one (key, action) pair per
/// line, specific to the machine.
pub fn game_key_table(machine: crate::emu::Machine) -> &'static [(&'static str, &'static str)] {
    use crate::emu::Machine::*;
    match machine {
        MasterSystem | Sg1000 => &[
            ("Z", "button 1 / START (starts the game)"),
            ("X", "button 2"),
            ("ENTER", "console pause button"),
        ],
        GameGear | GameGearExpanded => &[
            ("Z", "button 1"),
            ("X", "button 2"),
            ("ENTER", "start"),
        ],
        Genesis => &[
            ("Z", "A  (SF2 short kick - same key as SNES B)"),
            ("X", "B  (SF2 forward kick - SNES A)"),
            ("V", "C  (SF2 roundhouse - SNES R)"),
            ("A", "X  (SF2 jab - SNES Y)"),
            ("S", "Y  (SF2 strong - SNES X)"),
            ("C", "Z  (SF2 fierce - SNES L)"),
            ("SPACE", "mode"),
            ("ENTER", "start"),
        ],
        Nes => &[
            ("Z", "A"),
            ("X", "B"),
            ("SPACE", "select"),
            ("ENTER", "start"),
        ],
        Snes => &[
            ("Z", "B  (bottom)"),
            ("X", "A  (right)"),
            ("A", "Y  (left)"),
            ("S", "X  (top)"),
            ("C", "L shoulder"),
            ("V", "R shoulder"),
            ("SPACE", "select"),
            ("ENTER", "start"),
        ],
        Gba => &[
            ("Z", "A"),
            ("X", "B"),
            ("C", "L shoulder"),
            ("V", "R shoulder"),
            ("SPACE", "select"),
            ("ENTER", "start"),
        ],
        Pce => &[
            ("Z", "button I"),
            ("X", "button II"),
            ("SPACE", "select"),
            ("ENTER", "run"),
        ],
    }
}

