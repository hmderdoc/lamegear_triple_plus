//! Emit crossterm `Command`s as ANSI bytes to an arbitrary writer (our `Term`),
//! on every platform.
//!
//! crossterm's own `execute!`/`queue!` fall back to the Windows console API when
//! they can't prove ANSI support — which is exactly wrong for a door whose output
//! is an inherited socket, not a console. We always serialize via
//! `Command::write_ansi` instead, so the same ANSI goes to whatever the `Term`
//! is. The `emit!` macro is a drop-in for `execute!` (minus the implicit flush).

use crossterm::Command;
use std::fmt;
use std::io::{self, Write};

/// Adapts an `io::Write` to the `fmt::Write` that `Command::write_ansi` wants,
/// capturing the first I/O error.
struct FmtIo<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    err: io::Result<()>,
}

impl<W: Write + ?Sized> fmt::Write for FmtIo<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.err.is_ok() {
            self.err = self.inner.write_all(s.as_bytes());
        }
        if self.err.is_ok() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

/// `writer.emit_cmd(Command)` — method-call form so the macro works uniformly on
/// an owned writer, a `&mut` writer, or a `Box<dyn Term>` (auto-ref/deref).
pub(crate) trait EmitExt: Write {
    fn emit_cmd<C: Command>(&mut self, cmd: C) -> io::Result<()> {
        let mut adapter = FmtIo { inner: self, err: Ok(()) };
        let _ = cmd.write_ansi(&mut adapter);
        adapter.err
    }
}

impl<W: Write + ?Sized> EmitExt for W {}

/// `emit!(writer, Cmd, Cmd, ...)` — like crossterm's `execute!`, but always ANSI
/// to the given writer (no console-API fallback) and without an implicit flush.
#[macro_export]
macro_rules! emit {
    ($w:expr $(, $cmd:expr)* $(,)?) => {{
        #[allow(unused_imports)]
        use $crate::out::EmitExt as _;
        (|| -> ::std::io::Result<()> {
            $( $w.emit_cmd($cmd)?; )*
            Ok(())
        })()
    }};
}
