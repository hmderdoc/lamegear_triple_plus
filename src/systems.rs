//! Data-driven system registry: one entry per emulated console, each with its
//! own menu theme. Adding a console is an entry here (plus a core adapter).
//!
//! Themes are truecolor-native and quantized to the caller's depth at paint
//! time (`Theme::fg`/`Theme::bg` in menu.rs), same approach as lameboy's menu.

use crate::emu::Machine;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb8(pub u8, pub u8, pub u8);

/// Menu chrome palette for one system.
pub struct Theme {
    /// Screen fill outside the panels.
    pub backdrop: Rgb8,
    /// Panel interior fill.
    pub panel: Rgb8,
    /// Panel border / frame lines.
    pub frame: Rgb8,
    /// Big title / wordmark.
    pub title: Rgb8,
    /// Primary accent (badges, arrows, selected border).
    pub accent: Rgb8,
    /// Secondary accent (LED dots, small flourishes).
    pub accent2: Rgb8,
    /// Body text.
    pub text: Rgb8,
    /// De-emphasized text (hints, counts).
    pub dim: Rgb8,
    /// Selection bar.
    pub sel_fg: Rgb8,
    pub sel_bg: Rgb8,
}

pub struct SystemDef {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub extensions: &'static [&'static str],
    pub machine: Machine,
    /// Native picture size (post border-crop), for the footer fit readout.
    pub native: (u16, u16),
    pub theme: Theme,
    /// Small console doodle drawn beside the wordmark (CP437-safe glyphs).
    pub doodle: &'static [&'static str],
}

/// Game Gear: blue-black handheld, red power LED.
const GG: SystemDef = SystemDef {
    id: "gg",
    name: "GAME GEAR",
    tagline: "portable power in the palm of your hand",
    extensions: &["gg"],
    machine: Machine::GameGear,
    native: (160, 144),
    theme: Theme {
        backdrop: Rgb8(4, 6, 16),
        panel: Rgb8(10, 16, 38),
        frame: Rgb8(38, 84, 180),
        title: Rgb8(120, 180, 255),
        accent: Rgb8(64, 140, 255),
        accent2: Rgb8(255, 60, 60),
        text: Rgb8(210, 220, 235),
        dim: Rgb8(110, 125, 155),
        sel_fg: Rgb8(8, 12, 30),
        sel_bg: Rgb8(90, 160, 255),
    },
    doodle: &[
        "╔══════════════════╗",
        "║ o ┌──────────┐ ∙ ║",
        "║ + │▒▒▒▒▒▒▒▒▒▒│ **║",
        "║   └──────────┘ * ║",
        "╚══════════════════╝",
    ],
};

/// Master System: red-on-white grid, the classic western box art.
const SMS: SystemDef = SystemDef {
    id: "sms",
    name: "MASTER SYSTEM",
    tagline: "the challenge will always be there",
    extensions: &["sms"],
    machine: Machine::MasterSystem,
    native: (256, 192),
    theme: Theme {
        backdrop: Rgb8(24, 24, 28),
        panel: Rgb8(240, 238, 232),
        frame: Rgb8(200, 30, 40),
        title: Rgb8(200, 30, 40),
        accent: Rgb8(220, 40, 50),
        accent2: Rgb8(20, 20, 24),
        text: Rgb8(30, 30, 34),
        dim: Rgb8(130, 125, 120),
        sel_fg: Rgb8(250, 248, 244),
        sel_bg: Rgb8(200, 30, 40),
    },
    doodle: &[
        "┌──────────────────┐",
        "│ ▪▪▪▪▪▪▪▪▪▪▪▪▪▪ ∙ │",
        "│ ══════════════ o │",
        "│    [1]  [2]      │",
        "└──────────────────┘",
    ],
};

/// SG-1000: cream shell, navy + gold, 1983 vintage.
const SG1000: SystemDef = SystemDef {
    id: "sg",
    name: "SG-1000",
    tagline: "where it all began - 1983",
    extensions: &["sg"],
    machine: Machine::Sg1000,
    native: (256, 192),
    theme: Theme {
        backdrop: Rgb8(8, 10, 26),
        panel: Rgb8(16, 22, 52),
        frame: Rgb8(215, 175, 70),
        title: Rgb8(245, 210, 110),
        accent: Rgb8(230, 190, 80),
        accent2: Rgb8(220, 70, 70),
        text: Rgb8(232, 226, 205),
        dim: Rgb8(130, 130, 115),
        sel_fg: Rgb8(20, 24, 55),
        sel_bg: Rgb8(230, 190, 80),
    },
    doodle: &[
        "┌──────────────────┐",
        "│  ╓───╖  SG-1000  │",
        "│  ║ ∙ ║  ▬▬▬▬▬▬▬  │",
        "│  ╙───╜     ──o   │",
        "└──────────────────┘",
    ],
};

/// Genesis: black shell, red stripe, gold "16-BIT" badge.
const GENESIS: SystemDef = SystemDef {
    id: "md",
    name: "GENESIS",
    tagline: "welcome to the next level",
    extensions: &["md", "gen", "smd"],
    machine: Machine::Genesis,
    native: (320, 224),
    theme: Theme {
        backdrop: Rgb8(6, 6, 8),
        panel: Rgb8(16, 16, 20),
        frame: Rgb8(200, 40, 45),
        title: Rgb8(235, 235, 240),
        accent: Rgb8(220, 50, 55),
        accent2: Rgb8(212, 175, 55),
        text: Rgb8(205, 205, 212),
        dim: Rgb8(110, 110, 120),
        sel_fg: Rgb8(12, 12, 16),
        sel_bg: Rgb8(220, 50, 55),
    },
    doodle: &[
        "┌──────────────────┐",
        "│ ╔═════╗  16-BIT  │",
        "│ ║ ▒▒▒ ║ ═══════  │",
        "│ ╚═════╝    (o)   │",
        "└──────────────────┘",
    ],
};

/// NES: grey console, red logo and buttons.
const NES: SystemDef = SystemDef {
    id: "nes",
    name: "NES",
    tagline: "now you're playing with power",
    extensions: &["nes"],
    machine: Machine::Nes,
    native: (256, 224),
    theme: Theme {
        backdrop: Rgb8(14, 14, 15),
        panel: Rgb8(52, 52, 54),
        frame: Rgb8(150, 150, 152),
        title: Rgb8(228, 30, 40),
        accent: Rgb8(228, 30, 40),
        accent2: Rgb8(220, 218, 214),
        text: Rgb8(216, 214, 210),
        dim: Rgb8(135, 133, 130),
        sel_fg: Rgb8(245, 244, 242),
        sel_bg: Rgb8(200, 26, 34),
    },
    doodle: &[
        "┌──────────────────┐",
        "│ ═══════════════  │",
        "│  ┌───────┐       │",
        "│  │ ▪▪▪▪▪ │ [] [] │",
        "└──┴───────┴───────┘",
    ],
};

/// SNES: light grey console, purple buttons (US colorway).
const SNES: SystemDef = SystemDef {
    id: "sfc",
    name: "SUPER NES",
    tagline: "now you're playing with super power",
    extensions: &["sfc", "smc"],
    machine: Machine::Snes,
    native: (256, 224),
    theme: Theme {
        backdrop: Rgb8(16, 15, 20),
        panel: Rgb8(74, 72, 78),
        frame: Rgb8(126, 100, 190),
        title: Rgb8(190, 165, 250),
        accent: Rgb8(140, 105, 220),
        accent2: Rgb8(230, 228, 225),
        text: Rgb8(222, 220, 226),
        dim: Rgb8(150, 147, 155),
        sel_fg: Rgb8(240, 238, 245),
        sel_bg: Rgb8(120, 85, 205),
    },
    doodle: &[
        "┌──────────────────┐",
        "│ ╔══════════╗ ∙∙  │",
        "│ ║ ▒▒▒▒▒▒▒▒ ║ (Y) │",
        "│ ╚══╤══════╤╝ (B) │",
        "└────┴──────┴──────┘",
    ],
};

/// GBA: the classic indigo/violet shell.
const GBA: SystemDef = SystemDef {
    id: "gba",
    name: "GBA",
    tagline: "life advanced - single player",
    extensions: &["gba"],
    machine: Machine::Gba,
    native: (240, 160),
    theme: Theme {
        backdrop: Rgb8(10, 8, 22),
        panel: Rgb8(38, 28, 78),
        frame: Rgb8(110, 90, 200),
        title: Rgb8(185, 170, 255),
        accent: Rgb8(140, 120, 240),
        accent2: Rgb8(90, 210, 190),
        text: Rgb8(215, 210, 240),
        dim: Rgb8(125, 118, 165),
        sel_fg: Rgb8(25, 18, 55),
        sel_bg: Rgb8(150, 130, 245),
    },
    doodle: &[
        "╔════════════════════╗",
        "║ <  ┌──────────┐  ∙ ║",
        "║ () │▒▒▒▒▒▒▒▒▒▒│ (a)║",
        "║ ++ └──────────┘ (b)║",
        "╚════════════════════╝",
    ],
};

/// PC Engine / TurboGrafx-16: black shell, orange badge.
const PCE: SystemDef = SystemDef {
    id: "pce",
    name: "PC ENGINE",
    tagline: "the world's first 16-bit look - single player",
    extensions: &["pce"],
    machine: Machine::Pce,
    native: (256, 224),
    theme: Theme {
        backdrop: Rgb8(8, 7, 6),
        panel: Rgb8(22, 20, 18),
        frame: Rgb8(235, 120, 20),
        title: Rgb8(255, 160, 50),
        accent: Rgb8(240, 130, 30),
        accent2: Rgb8(200, 200, 205),
        text: Rgb8(215, 210, 200),
        dim: Rgb8(125, 118, 108),
        sel_fg: Rgb8(20, 16, 12),
        sel_bg: Rgb8(240, 130, 30),
    },
    doodle: &[
        "┌──────────────────┐",
        "│  ╔════════╗  (∙) │",
        "│  ║ HuCARD ║ ───  │",
        "│  ╚════════╝ TURBO│",
        "└──────────────────┘",
    ],
};

pub const SYSTEMS: &[SystemDef] = &[GG, SMS, SG1000, GENESIS, NES, SNES, GBA, PCE];

pub fn system_for_extension(ext: &str) -> Option<&'static SystemDef> {
    let ext = ext.to_ascii_lowercase();
    SYSTEMS.iter().find(|s| s.extensions.contains(&ext.as_str()))
}

/// Look a system up by its roster/ini slug ("gg", "md", ...).
pub fn system_index_for_id(id: &str) -> Option<usize> {
    SYSTEMS.iter().position(|s| s.id == id)
}

/// Whether this machine has a second controller port at all (GBA and PC
/// Engine are wired single-player here: no JOIN, no knock, no open port).
pub fn two_player(machine: Machine) -> bool {
    !matches!(machine, Machine::Gba | Machine::Pce)
}
