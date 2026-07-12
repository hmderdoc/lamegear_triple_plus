//! Sega Game Genie cheat codes for SMS / Game Gear.
//!
//! Code format (verified against SMS Power's Game Genie documentation,
//! https://www.smspower.org/Development/GameGenie — the same reference cited
//! by the vendored jgenesis decoder in vendor/smsgg-config/src/cheats.rs):
//!
//! Codes are 9 hex digits printed `DDA-AAA-RRR`, or 6 digits `DDA-AAA` when
//! the optional reference group is omitted. With the digits numbered
//! d0..d8 (dashes removed):
//!
//! * `d0 d1`  — the patch VALUE, unobfuscated.
//! * `d2..d5` — the Z80 ADDRESS, obfuscated: move the last digit to the
//!   front and invert its bits, i.e.
//!   `address = ((d5 ^ 0xF) << 12) | (d2 << 8) | (d3 << 4) | d4`
//!   (equivalently: rotate the 16-bit group right by 4, XOR with 0xF000).
//! * `d6 d7 d8` — optional. `d6 ^ d7` is a "cloak" nibble (always 8 in
//!   factory codes; ignored here, as jgenesis does). The REFERENCE byte is
//!   built from the first and third digits, rotated right 2 bits, XOR 0xBA:
//!   `reference = (((d6 << 4) | d8) >>> 2) ^ 0xBA`.
//!
//! The reference (compare) byte makes the patch conditional: the override
//! only applies when the byte currently at the address equals the reference.
//! smsgg-core enforces this in `CheatByteOverridesU16Address::get`
//! (vendor/jgenesis-common/src/cheats.rs), consulted on every memory read
//! (vendor/smsgg-core/src/memory.rs, end of `Memory::read`).
//!
//! Sanity check on published codes: Sonic 1 GG "infinite lives"
//! `007-01A-3BE` decodes to address $5701, value $00, reference $35 — i.e.
//! NOP out a `DEC (HL)` (Z80 opcode $35), exactly what an infinite-lives
//! patch looks like — and its cloak nibble is 8 as documented.
//!
//! Persistence follows lameboy's sidecar scheme: a tab-delimited
//! `gamegenie-<user>` file (`rom_filename\t0|1\tCODE` per line) in the same
//! `~/.config/lamegear/` directory used by config.rs.

use jgenesis_common::cheats::ByteCheatCodeU16Address;
use std::fs;
use std::path::{Path, PathBuf};

/// One Game Genie entry as the user sees it: the printed code (with or
/// without dashes) plus whether it is currently switched on. Codes are kept
/// on disk even while toggled off so they reappear next session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameGenieCode {
    pub code: String,
    pub enabled: bool,
}

/// A decoded code: the raw patch smsgg-core applies on memory reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedCode {
    /// Z80 address whose reads are overridden.
    pub address: u16,
    /// Byte returned in place of the real memory value.
    pub value: u8,
    /// If present, the override only fires while the underlying byte equals
    /// this value (the Game Genie "reference" byte).
    pub compare: Option<u8>,
}

// ── Decoding ────────────────────────────────────────────────────────────────

/// Keep only hex digits, uppercased, capped at 9 — for building up a code in
/// a menu entry field.
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_uppercase())
        .take(9)
        .collect()
}

/// Printed form of a (possibly partial) code: digits grouped in threes with
/// dashes, e.g. "00701A3BE" -> "007-01A-3BE", "0070" -> "007-0".
pub fn format_grouped(input: &str) -> String {
    let digits = normalize(input);
    let mut out = String::with_capacity(11);
    for (i, c) in digits.chars().enumerate() {
        if i == 3 || i == 6 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// Whether `code` has a full 6- or 9-digit code's worth of hex digits.
pub fn is_complete(code: &str) -> bool {
    matches!(normalize(code).len(), 6 | 9)
}

/// Decode a Game Genie code. Accepts 6 or 9 significant hex digits, with or
/// without `-`/space separators, any case. Returns `None` for anything else.
pub fn decode(code: &str) -> Option<DecodedCode> {
    let mut digits = [0u8; 9];
    let mut n = 0usize;
    for c in code.chars() {
        match c {
            '-' | ' ' => continue,
            _ => {
                let d = c.to_digit(16)?;
                if n == 9 {
                    return None; // too many digits
                }
                digits[n] = d as u8;
                n += 1;
            }
        }
    }
    if n != 6 && n != 9 {
        return None;
    }

    // d0 d1: value, unobfuscated.
    let value = (digits[0] << 4) | digits[1];

    // d2..d5: address — last digit moved to the front and inverted.
    let address = (u16::from(digits[5] ^ 0xF) << 12)
        | (u16::from(digits[2]) << 8)
        | (u16::from(digits[3]) << 4)
        | u16::from(digits[4]);

    // d6 d7 d8: optional reference — (d6,d8) as a byte, rotated right 2,
    // XOR 0xBA. d7 only participates in the "cloak" nibble, which is
    // irrelevant for emulation.
    let compare = (n == 9).then(|| {
        let scrambled = (digits[6] << 4) | digits[8];
        scrambled.rotate_right(2) ^ 0xBA
    });

    Some(DecodedCode { address, value, compare })
}

/// Convert the enabled, well-formed codes to the structs
/// `SmsGgEmulatorConfig.cheat_codes` wants. Disabled or malformed entries are
/// silently skipped.
// ---------------------------------------------------------------------------
// Genesis cheats: the decoders (Game Genie XXXX-XXXX with its letter
// alphabet, Pro Action Replay XXXXX:XXXX-style, raw memory overrides) ship
// upstream in genesis-config; this just adapts them to the door's slot store.

/// Is this a complete, decodable Genesis cheat code?
pub fn genesis_is_valid(code: &str) -> bool {
    genesis_config::cheats::GenesisCheatCodeType::guess_from(code)
        .and_then(|t| t.decode(code))
        .is_some()
}

/// Enabled+valid codes -> the (address, value) overrides genesis-core takes.
pub fn to_genesis(codes: &[GameGenieCode]) -> Vec<(u32, u16)> {
    codes
        .iter()
        .filter(|c| c.enabled)
        .filter_map(|c| {
            genesis_config::cheats::GenesisCheatCodeType::guess_from(&c.code)
                .and_then(|t| t.decode(&c.code))
        })
        .collect()
}

pub fn to_core(codes: &[GameGenieCode]) -> Vec<ByteCheatCodeU16Address> {
    codes
        .iter()
        .filter(|c| c.enabled)
        .filter_map(|c| decode(&c.code))
        .map(|d| ByteCheatCodeU16Address {
            address: d.address,
            value: d.value,
            reference: d.compare,
        })
        .collect()
}

// ── Per-user, per-ROM persistence ───────────────────────────────────────────
//
// Same directory and username sanitizing as config.rs (kept self-contained
// here on purpose): `~/.config/lamegear/gamegenie-<user>`, one line per code:
// `<rom-filename>\t<0|1 enabled>\t<code>`.

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("lamegear"))
}

fn gg_file(user: &str) -> Option<PathBuf> {
    let safe: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    config_dir().map(|d| {
        if safe.is_empty() { d.join("gamegenie") } else { d.join(format!("gamegenie-{safe}")) }
    })
}

/// Parse the store into (rom, entry) pairs, preserving order. Malformed
/// lines are skipped. Pure, so it's unit-testable.
fn parse_lines(contents: &str) -> Vec<(String, GameGenieCode)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let (Some(rom), Some(flag), Some(code)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let code = code.trim();
        if rom.is_empty() || code.is_empty() {
            continue;
        }
        out.push((
            rom.to_string(),
            GameGenieCode { code: code.to_string(), enabled: flag.trim() != "0" },
        ));
    }
    out
}

/// Serialize (rom, entry) pairs back to the store format. Pure.
fn serialize_lines(all: &[(String, GameGenieCode)]) -> String {
    let mut s = String::new();
    for (rom, e) in all {
        s.push_str(rom);
        s.push('\t');
        s.push(if e.enabled { '1' } else { '0' });
        s.push('\t');
        s.push_str(&e.code);
        s.push('\n');
    }
    s
}

fn load_all_from(path: &Path) -> Vec<(String, GameGenieCode)> {
    match fs::read_to_string(path) {
        Ok(contents) => parse_lines(&contents),
        Err(_) => Vec::new(),
    }
}

fn load_codes_from(path: &Path, rom_file: &str) -> Vec<GameGenieCode> {
    load_all_from(path).into_iter().filter(|(r, _)| r == rom_file).map(|(_, e)| e).collect()
}

/// Replace one ROM's codes in the store at `path`, preserving every other
/// ROM's lines. An empty slice clears that ROM's codes.
fn save_codes_to(path: &Path, rom_file: &str, codes: &[GameGenieCode]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut all = load_all_from(path);
    all.retain(|(r, _)| r != rom_file);
    for e in codes {
        all.push((rom_file.to_string(), e.clone()));
    }
    fs::write(path, serialize_lines(&all))
}

/// Load the saved Game Genie codes for one ROM (keyed by file name), in the
/// order they were saved. Missing file / no HOME just yields an empty list.
#[cfg(test)]
mod genesis_tests {
    use super::*;

    #[test]
    fn genesis_code_shapes_validate_and_decode() {
        // Game Genie shape (letter alphabet, 4-4).
        assert!(genesis_is_valid("ATBT-AA32"));
        // Action Replay shape (5+5 hex digits, optional space/dash).
        assert!(genesis_is_valid("FFFE1 20001"));
        assert!(genesis_is_valid("FFFE1-20001"));
        // Memory override shape.
        assert!(genesis_is_valid("FF0000:00FF"));
        // Junk.
        assert!(!genesis_is_valid("XYZ"));
        assert!(!genesis_is_valid("007-01A-3BE")); // SMS shape, not Genesis

        let codes = vec![
            GameGenieCode { code: "FF0000:00FF".into(), enabled: true },
            GameGenieCode { code: "FF0002:1234".into(), enabled: false },
        ];
        let decoded = to_genesis(&codes);
        assert_eq!(decoded, vec![(0xFF0000, 0x00FF)]);
    }
}

pub fn load_codes(user: &str, rom_file: &str) -> Vec<GameGenieCode> {
    let Some(path) = gg_file(user) else { return Vec::new() };
    load_codes_from(&path, rom_file)
}

/// Persist one ROM's codes for `user`, keeping other ROMs' codes intact.
/// Best-effort, like `UserConfig::save`: I/O errors are silently ignored.
pub fn save_codes(user: &str, rom_file: &str, codes: &[GameGenieCode]) {
    let Some(path) = gg_file(user) else { return };
    let _ = save_codes_to(&path, rom_file, codes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gg(code: &str, enabled: bool) -> GameGenieCode {
        GameGenieCode { code: code.to_string(), enabled }
    }

    // Published codes (gamegenie.com Game Gear lists), decoded per SMS Power's
    // documented algorithm. The reference bytes double as a semantic check:
    // both "infinite lives" codes replace an original $35 (Z80 `DEC (HL)`)
    // with $00 (`NOP`), and "never lose rings" replaces $32 (`LD (nn),A`,
    // the ring-count store) with $3A (`LD A,(nn)`).

    #[test]
    fn decodes_sonic1_infinite_lives() {
        // Sonic the Hedgehog (GG, v0): 007-01A-3BE
        assert_eq!(
            decode("007-01A-3BE"),
            Some(DecodedCode { address: 0x5701, value: 0x00, compare: Some(0x35) })
        );
    }

    #[test]
    fn decodes_sonic2_infinite_lives() {
        // Sonic the Hedgehog 2 (GG): 009-04F-3BE
        assert_eq!(
            decode("009-04F-3BE"),
            Some(DecodedCode { address: 0x0904, value: 0x00, compare: Some(0x35) })
        );
    }

    #[test]
    fn decodes_sonic1_never_lose_rings() {
        // Sonic the Hedgehog (GG): 3A0-21C-2A2
        assert_eq!(
            decode("3A0-21C-2A2"),
            Some(DecodedCode { address: 0x3021, value: 0x3A, compare: Some(0x32) })
        );
    }

    #[test]
    fn decodes_six_digit_code_without_compare() {
        assert_eq!(
            decode("3A0-21C"),
            Some(DecodedCode { address: 0x3021, value: 0x3A, compare: None })
        );
    }

    #[test]
    fn accepts_lowercase_undashed_and_spaced() {
        let want = decode("007-01A-3BE");
        assert!(want.is_some());
        assert_eq!(decode("00701a3be"), want);
        assert_eq!(decode("007 01A 3BE"), want);
        assert_eq!(decode("007-01a-3bE"), want);
    }

    #[test]
    fn matches_vendored_jgenesis_decoder() {
        // Cross-check against the vendored smsgg-config decoder (which only
        // accepts the dashed forms) on both 9- and 6-digit codes.
        for code in ["007-01A-3BE", "009-04F-3BE", "3A0-21C-2A2", "3A0-21C", "FFF-FFF-FFF"] {
            let ours = decode(code).expect(code);
            let theirs = smsgg_config::cheats::SmsGgCheatCodeType::GameGenie
                .decode(code)
                .expect(code);
            assert_eq!(ours.address, theirs.address, "{code}");
            assert_eq!(ours.value, theirs.value, "{code}");
            assert_eq!(ours.compare, theirs.reference, "{code}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",            // empty
            "007",         // too short
            "007-01",      // 5 digits
            "007-01A-3B",  // 8 digits
            "007-01A-3BEF",// 10 digits
            "00G-01A-3BE", // non-hex digit
            "007_01A_3BE", // bad separator
            "cheat",       // nonsense
        ] {
            assert_eq!(decode(bad), None, "{bad:?} should not decode");
        }
    }

    #[test]
    fn normalize_and_grouping() {
        assert_eq!(normalize("007-01a-3be"), "00701A3BE");
        assert_eq!(normalize("zz3a!0"), "3A0");
        assert_eq!(format_grouped("00701A3BE"), "007-01A-3BE");
        assert_eq!(format_grouped("00701"), "007-01");
        assert_eq!(format_grouped(""), "");
        assert!(is_complete("3A0-21C"));
        assert!(is_complete("007-01A-3BE"));
        assert!(!is_complete("007-01A-3B"));
        assert!(!is_complete("007"));
    }

    #[test]
    fn to_core_filters_disabled_and_invalid() {
        let codes = [
            gg("007-01A-3BE", true),
            gg("009-04F-3BE", false), // disabled: skipped
            gg("not-a-code", true),   // invalid: skipped
            gg("3A0-21C", true),
        ];
        let core = to_core(&codes);
        assert_eq!(core.len(), 2);
        assert_eq!(
            core[0],
            ByteCheatCodeU16Address { address: 0x5701, value: 0x00, reference: Some(0x35) }
        );
        assert_eq!(
            core[1],
            ByteCheatCodeU16Address { address: 0x3021, value: 0x3A, reference: None }
        );
    }

    #[test]
    fn parse_serialize_roundtrip_and_bad_lines() {
        let text = "sonic.gg\t1\t007-01A-3BE\n\
                    sonic.gg\t0\t3A0-21C-2A2\n\
                    \n\
                    missing-fields\t1\n\
                    columns.gg\t1\t00A-19B\n";
        let all = parse_lines(text);
        assert_eq!(
            all,
            vec![
                ("sonic.gg".into(), gg("007-01A-3BE", true)),
                ("sonic.gg".into(), gg("3A0-21C-2A2", false)),
                ("columns.gg".into(), gg("00A-19B", true)),
            ]
        );
        assert_eq!(parse_lines(&serialize_lines(&all)), all);
    }

    #[test]
    fn file_roundtrip_preserves_other_roms() {
        let path = std::env::temp_dir()
            .join(format!("lamegear-gg-test-{}", std::process::id()))
            .join("gamegenie-tester");

        let sonic = [gg("007-01A-3BE", true), gg("3A0-21C-2A2", false)];
        let columns = [gg("00A-19B", true)];
        save_codes_to(&path, "sonic.gg", &sonic).unwrap();
        save_codes_to(&path, "columns.gg", &columns).unwrap();
        assert_eq!(load_codes_from(&path, "sonic.gg"), sonic);
        assert_eq!(load_codes_from(&path, "columns.gg"), columns);

        // Rewriting one ROM's codes leaves the other ROM untouched; an empty
        // slice clears the ROM's codes.
        let sonic2 = [gg("009-04F-3BE", true)];
        save_codes_to(&path, "sonic.gg", &sonic2).unwrap();
        assert_eq!(load_codes_from(&path, "sonic.gg"), sonic2);
        assert_eq!(load_codes_from(&path, "columns.gg"), columns);
        save_codes_to(&path, "columns.gg", &[]).unwrap();
        assert_eq!(load_codes_from(&path, "columns.gg"), Vec::new());
        assert_eq!(load_codes_from(&path, "sonic.gg"), sonic2);

        // Unknown ROM / missing file read back empty.
        assert_eq!(load_codes_from(&path, "nope.sms"), Vec::new());
        assert_eq!(load_codes_from(Path::new("/nonexistent/gg"), "sonic.gg"), Vec::new());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
