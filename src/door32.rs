//! DOOR32.SYS dropfile reader.
//!
//! Cross-BBS doors (Synchronet, EleBBS, Mystic, …) don't always hand the door
//! its connection via stdio. The portable convention is the DOOR32.SYS dropfile:
//! the BBS writes it, passes its path on the command line, and line 2 carries an
//! inherited **comm handle** — a socket fd on unix, a Winsock SOCKET on Windows
//! — that the door reads/writes for all terminal I/O. This is how a door talks
//! to a caller on a Windows BBS, where there is no stdio passthrough socket.
//!
//! Format (one value per line):
//!   1  comm type: 0=local, 1=serial, 2=telnet (socket)
//!   2  comm/socket handle
//!   3  baud rate
//!   4  BBS software name/version
//!   5  user record number
//!   6  user real name
//!   7  user handle / alias
//!   8  user security level
//!   9  time left, minutes
//!   10 emulation: 0=ascii, 1=ansi, …
//!   11 current node number

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Door32 {
    pub comm_type: u8, // 0=local, 1=serial, 2=telnet socket
    pub handle: u64,   // inherited comm handle (fd on unix, SOCKET on Windows)
    pub bbs_id: String,
    pub user_number: u32,
    pub real_name: String,
    pub alias: String,
    pub security: u32,
    pub minutes_left: u32,
    pub emulation: u8,
    pub node: u32,
}

impl Door32 {
    /// The inherited socket handle iff this is a telnet (type 2) connection.
    /// `None` means "no socket — fall back to stdio."
    pub fn socket(&self) -> Option<u64> {
        if self.comm_type == 2 {
            Some(self.handle)
        } else {
            None
        }
    }

    /// A stable per-user key for saves/prefs. Prefer the numeric user record
    /// (zero-padded, like Synchronet's `%4`); when the BBS has no usable record
    /// number — some identify users by name — fall back to the handle, then the
    /// real name, sanitized to a filesystem-safe key. This keeps every caller's
    /// saves/prefs separate instead of collapsing name-based users into one
    /// shared pile. `None` only if the dropfile carries no identity at all.
    /// Caller-facing display name for the multiplayer lobby: alias first,
    /// real name as the fallback.
    pub fn display_name(&self) -> String {
        if !self.alias.trim().is_empty() {
            self.alias.trim().to_string()
        } else {
            self.real_name.trim().to_string()
        }
    }

    pub fn user_key(&self) -> Option<String> {
        if self.user_number > 0 {
            return Some(format!("{:04}", self.user_number));
        }
        for name in [self.alias.as_str(), self.real_name.as_str()] {
            let key = sanitize_key(name);
            if !key.is_empty() {
                return Some(key);
            }
        }
        None
    }
}

/// Fold a name into a filesystem-safe key: keep ASCII alphanumerics, turn
/// everything else into `_`, and trim leading/trailing `_`.
fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Read and parse a DOOR32.SYS. `path` may be the file itself, OR a directory
/// that contains it — some BBS configs pass only the drop-file directory (e.g.
/// Synchronet's `--dropfile .\`), in which case we look for DOOR32.SYS inside.
/// Returns None if nothing readable is found or the first two (required) lines
/// don't parse.
pub fn read(path: &Path) -> Option<Door32> {
    let text = std::fs::read_to_string(path).ok().or_else(|| {
        // `path` wasn't a readable file (likely a directory) — look inside it.
        ["DOOR32.SYS", "door32.sys"]
            .iter()
            .find_map(|name| std::fs::read_to_string(path.join(name)).ok())
    })?;
    parse(&text)
}

fn parse(text: &str) -> Option<Door32> {
    let mut lines = text.lines();
    let comm_type = lines.next()?.trim().parse().ok()?;
    // The comm/socket handle is -1 for local/stdio sessions (no network
    // socket). Parse leniently as signed and clamp: a negative/unparseable
    // handle just means "no socket" (socket() gates on comm_type==2 anyway) —
    // it must NOT discard the identity fields below (the alias is why we read
    // this at all). A door running in stdio mode still has a real user name.
    let handle = lines.next()?.trim().parse::<i64>().unwrap_or(0).max(0) as u64;
    let mut next_str = || lines.next().unwrap_or("").trim().to_string();
    let _baud = next_str();
    let bbs_id = next_str();
    let user_number = next_str().parse().unwrap_or(0);
    let real_name = next_str();
    let alias = next_str();
    let security = next_str().parse().unwrap_or(0);
    let minutes_left = next_str().parse().unwrap_or(0);
    let emulation = next_str().parse().unwrap_or(0);
    let node = next_str().parse().unwrap_or(0);
    Some(Door32 {
        comm_type,
        handle,
        bbs_id,
        user_number,
        real_name,
        alias,
        security,
        minutes_left,
        emulation,
        node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_telnet_dropfile() {
        // Typical DOOR32.SYS for a telnet door (CRLF line endings, like a BBS writes).
        let sample = "2\r\n328\r\n0\r\nEleBBS\r\n7\r\nShurato Real\r\nShurato\r\n100\r\n90\r\n1\r\n3\r\n";
        let d = parse(sample).unwrap();
        assert_eq!(d.comm_type, 2);
        assert_eq!(d.handle, 328);
        assert_eq!(d.socket(), Some(328));
        assert_eq!(d.user_number, 7);
        assert_eq!(d.alias, "Shurato");
        assert_eq!(d.node, 3);
        assert_eq!(d.user_key().as_deref(), Some("0007"));
    }

    #[test]
    fn name_user_keys_off_the_handle_when_no_number() {
        // user number 0 (BBS identifies by name), handle "Shurato" -> own saves.
        let d = parse("2\n5\n0\nEleBBS\n0\nReal Name\nShurato\n10\n90\n1\n1\n").unwrap();
        assert_eq!(d.user_key().as_deref(), Some("Shurato"));
        // handle with spaces/symbols sanitizes to a safe key
        let d2 = parse("2\n5\n0\nEleBBS\n0\nReal Name\nSysop Bob!\n10\n90\n1\n1\n").unwrap();
        assert_eq!(d2.user_key().as_deref(), Some("Sysop_Bob"));
        // a real number still wins over the name
        let d3 = parse("2\n5\n0\nEleBBS\n7\nReal Name\nShurato\n10\n90\n1\n1\n").unwrap();
        assert_eq!(d3.user_key().as_deref(), Some("0007"));
    }

    #[test]
    fn reads_dropfile_when_given_its_directory() {
        // Synchronet-style `--dropfile .\` passes the directory, not the file.
        use std::fs;
        let dir = std::env::temp_dir().join(format!("lameboy_d32_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("DOOR32.SYS"), "2\r\n328\r\n0\r\nSBBS\r\n7\r\nReal\r\nShu\r\n100\r\n90\r\n1\r\n1\r\n").unwrap();
        let d = read(&dir).expect("should find DOOR32.SYS inside the directory");
        assert_eq!(d.socket(), Some(328));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_mode_has_no_socket() {
        let d = parse("0\n0\n0\nBBS\n1\nSysOp\nSysOp\n255\n1000\n1\n1\n").unwrap();
        assert_eq!(d.socket(), None);
    }

    #[test]
    fn stdio_dropfile_with_negative_handle_still_yields_identity() {
        // Synchronet writes comm type 0 + socket handle -1 for a door running
        // in stdio mode. The -1 must NOT reject the dropfile — the alias/real
        // name are still valid (this was showing every caller as "Player").
        let real = "0\n-1\n38400\nSynchronet BBS for Linux  Version 3.21d\n1\nHm Derdoc\nHm Derdoc\n99\n1440\n1\n2\n";
        let d = parse(real).expect("a -1 socket handle must not discard the dropfile");
        assert_eq!(d.socket(), None); // no network socket -> door uses stdio
        assert_eq!(d.real_name, "Hm Derdoc");
        assert_eq!(d.alias, "Hm Derdoc");
        assert_eq!(d.user_key().as_deref(), Some("0001"));
    }
}
