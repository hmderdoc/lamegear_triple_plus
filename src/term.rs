//! Cross-BBS terminal I/O.
//!
//! The door talks to the caller over whatever the BBS provides — an inherited
//! socket named in DOOR32.SYS, or stdio — instead of assuming a local console.
//! This mirrors the `Term` layer in the spectre door (see its io_unix.go /
//! io_windows.go).
//!
//!   - **socket mode**: the BBS hands us an inherited connection. On unix that
//!     handle is a file descriptor; on Windows it's a Winsock SOCKET, which is
//!     NOT a descriptor and must be driven with WSARecv/WSASend.
//!   - **stdio mode**: no socket in the dropfile -> read stdin / write stdout
//!     (a real console or a launcher's pipe). On Windows we write the raw OS
//!     handle so CP437 bytes aren't rejected by std's console UTF-8 guard.
//!
//! Reads are non-blocking (`read_available` polls; the input loop drains it each
//! frame). Writes block-with-backpressure by retrying on EWOULDBLOCK, so a full
//! send buffer throttles us (which LinkPace then measures) instead of erroring.

use std::io::{self, Write};

use crate::door32::Door32;

/// The caller connection. `Write` carries output; `read_available` polls input.
pub trait Term: Write {
    /// Read whatever bytes are ready right now (0 if none). Never blocks.
    /// Returns an `UnexpectedEof` error when the peer has closed.
    fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

/// Open the terminal: the inherited socket if the dropfile carries one (type 2),
/// otherwise stdio.
pub fn open(door: Option<&Door32>) -> io::Result<Box<dyn Term>> {
    let socket = door.and_then(|d| d.socket());
    let term = imp::open(socket)?;
    Ok(maybe_tee(term))
}

// ---------------------------------------------------------------------------
// Session tee (diagnostic): record every byte in both directions, in order.
// Enabled by a `lamegear.tee` marker file in the startup dir (the BBS launcher
// can't set env vars) or the LAMEGEAR_TEE env var. Used to capture a REAL
// failing session (e.g. SyncTERM sixel troubleshooting) for offline diffing
// against a known-good stream. Zero cost when not enabled.
// ---------------------------------------------------------------------------
fn maybe_tee(term: Box<dyn Term>) -> Box<dyn Term> {
    let enabled = std::env::var_os("LAMEGEAR_TEE").is_some()
        || std::path::Path::new("lamegear.tee").exists();
    if !enabled {
        return term;
    }
    let name = format!("tee_session_{}.log", std::process::id());
    match std::fs::File::create(&name) {
        Ok(mut file) => {
            let _ = writeln!(file, "# lameboy {} session tee", env!("CARGO_PKG_VERSION"));
            Box::new(TeeTerm {
                inner: term,
                file,
                start: std::time::Instant::now(),
            })
        }
        Err(_) => term,
    }
}

struct TeeTerm {
    inner: Box<dyn Term>,
    file: std::fs::File,
    start: std::time::Instant,
}

impl TeeTerm {
    /// One line per chunk: `<dir> <ms> <hex>` — `o` door->terminal, `i` terminal->door.
    fn log(&mut self, dir: char, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut line = String::with_capacity(data.len() * 2 + 16);
        use std::fmt::Write as _;
        let _ = write!(line, "{} {} ", dir, self.start.elapsed().as_millis());
        for b in data {
            let _ = write!(line, "{b:02x}");
        }
        line.push('\n');
        let _ = self.file.write_all(line.as_bytes());
    }
}

impl Write for TeeTerm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.log('o', &buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Term for TeeTerm {
    fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_available(buf)?;
        if n > 0 {
            self.log('i', &buf[..n]);
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Unix: inherited socket fd or stdio (fd 0/1), driven at the syscall level.
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::io::RawFd;

    pub fn open(socket: Option<u64>) -> io::Result<Box<dyn Term>> {
        match socket {
            Some(h) => {
                // An inherited socket isn't a tty, so no termios is involved.
                let fd = h as RawFd;
                set_nonblock(fd)?;
                Ok(Box::new(FdTerm { in_fd: fd, out_fd: fd, saved: None }))
            }
            None => {
                // stdio: put a controlling tty into raw mode (char-at-a-time, no
                // echo). A non-tty stdio (a passthru socket given as fd 0/1) needs
                // none of that and make_raw is a no-op.
                let saved = make_raw(0);
                set_nonblock(0)?;
                Ok(Box::new(FdTerm { in_fd: 0, out_fd: 1, saved }))
            }
        }
    }

    struct FdTerm {
        in_fd: RawFd,
        out_fd: RawFd,
        saved: Option<libc::termios>, // original tty settings to restore on drop
    }

    fn set_nonblock(fd: RawFd) -> io::Result<()> {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn make_raw(fd: RawFd) -> Option<libc::termios> {
        unsafe {
            if libc::isatty(fd) != 1 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(saved)
        }
    }

    impl Drop for FdTerm {
        fn drop(&mut self) {
            if let Some(t) = self.saved.as_ref() {
                unsafe { libc::tcsetattr(self.in_fd, libc::TCSANOW, t) };
            }
        }
    }

    impl Write for FdTerm {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            loop {
                let n = unsafe {
                    libc::write(self.out_fd, buf.as_ptr() as *const libc::c_void, buf.len())
                };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    // Send buffer full: backpressure, not failure — wait and retry.
                    // (EWOULDBLOCK == EAGAIN on Linux, so EAGAIN covers both.)
                    Some(libc::EAGAIN) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Some(libc::EINTR) => {}
                    _ => return Err(e),
                }
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Term for FdTerm {
        fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = unsafe {
                libc::read(self.in_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n > 0 {
                return Ok(n as usize);
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // EWOULDBLOCK == EAGAIN on Linux; EAGAIN covers both.
                Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(0),
                _ => Err(e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: inherited Winsock SOCKET (WSARecv/WSASend) or stdio via raw handle.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use super::*;
    use std::fs::File;
    use std::mem::ManuallyDrop;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Networking::WinSock::{
        ioctlsocket, recv, send, WSAGetLastError, WSAStartup, FIONBIO, SOCKET, WSADATA,
        WSAEWOULDBLOCK,
    };

    pub fn open(socket: Option<u64>) -> io::Result<Box<dyn Term>> {
        match socket {
            Some(h) => {
                // The BBS started Winsock in its own process; init it in ours too
                // before touching the inherited handle.
                unsafe {
                    let mut data: WSADATA = std::mem::zeroed();
                    WSAStartup(0x202, &mut data);
                }
                let sock = h as SOCKET;
                let mut nb: u32 = 1;
                unsafe { ioctlsocket(sock, FIONBIO, &mut nb) };
                Ok(Box::new(WinSockTerm { sock }))
            }
            None => {
                // Wrap the stdout handle in a File (ManuallyDrop so we don't close
                // the shared handle). File::write goes straight to WriteFile and so
                // bypasses std::io::Stdout's "console mode can't write non-UTF-8"
                // guard that rejects our CP437 bytes.
                let out = ManuallyDrop::new(unsafe {
                    File::from_raw_handle(io::stdout().as_raw_handle())
                });
                Ok(Box::new(WinStdioTerm { out }))
            }
        }
    }

    struct WinSockTerm {
        sock: SOCKET,
    }

    impl Write for WinSockTerm {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            loop {
                let n = unsafe { send(self.sock, buf.as_ptr(), buf.len() as i32, 0) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let err = unsafe { WSAGetLastError() };
                if err == WSAEWOULDBLOCK {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                return Err(io::Error::from_raw_os_error(err));
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Term for WinSockTerm {
        fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let n = unsafe { recv(self.sock, buf.as_mut_ptr(), buf.len() as i32, 0) };
            if n > 0 {
                return Ok(n as usize);
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
            }
            let err = unsafe { WSAGetLastError() };
            if err == WSAEWOULDBLOCK {
                return Ok(0);
            }
            Err(io::Error::from_raw_os_error(err))
        }
    }

    // stdio fallback: raw-handle output (bypasses the UTF-8 console guard). Input
    // over a console/pipe needs PeekConsoleInput/PeekNamedPipe; the socket path is
    // the real door path, so stdio input is a later pass (returns "no input").
    struct WinStdioTerm {
        out: ManuallyDrop<File>,
    }

    impl Write for WinStdioTerm {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.out.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.out.flush()
        }
    }

    impl Term for WinStdioTerm {
        fn read_available(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }
}
