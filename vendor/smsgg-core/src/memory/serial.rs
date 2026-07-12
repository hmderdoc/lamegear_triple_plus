//! Game Gear serial port (Gear-to-Gear cable) emulation, plus a cheap I/O port
//! access trace used by the frontend to classify a ROM's multiplayer shape.
//!
//! Register semantics are from the official Sega Game Gear Hardware Reference
//! Manual ("2. System control port", I/O ports 00H-06H); cross-checked against
//! MEKA's commport documentation and SMS Power's smstech notes.
//!
//! I/O port $05 (Serial communications mode setting, R/W):
//! ```text
//!   D7   D6   D5   D4   D3   D2    D1    D0
//!   BS1  BS0  RON  TON  INT  FRER  RXRD  TXFL
//! ```
//! - BS1/BS0 (R/W): baud rate select: 00=4800, 01=2400, 10=1200, 11=300 bps
//! - RON (R/W): 1 = receive enable (PC5 forced to input)
//! - TON (R/W): 1 = send enable (PC4 forced to output)
//! - INT (R/W): 1 = an NMI is generated when data is received
//! - FRER (R): 1 = framing error
//! - RXRD (R): 1 = receive data is present in port $04
//! - TXFL (R): 1 = the send data ($03) has not been transferred yet
//!
//! Ports $03 (TX data, R/W) and $04 (RX data, R) are plain 8-bit data
//! registers. The wire format is a 5V TTL UART frame: 1 start bit, 8 data
//! bits LSB-first, no parity, 1 stop bit = 10 bit times per byte.
//!
//! All timing is counted in emulated Z80 T-cycles (3,579,545 Hz on the Game
//! Gear); there is no wall-clock or randomness anywhere in this module, so
//! emulation stays deterministic.

use bincode::{Decode, Encode};

/// NTSC Z80 clock frequency in Hz (the Game Gear is NTSC-only).
const Z80_CLOCK_HZ: u64 = 3_579_545;

/// Bits of a UART frame: 1 start + 8 data + 1 stop.
const BITS_PER_FRAME: u64 = 10;

// $05 writable control bits (D7-D3); D2-D0 are read-only status flags
const CONTROL_WRITE_MASK: u8 = 0xF8;
const CONTROL_RON: u8 = 1 << 5;
const CONTROL_TON: u8 = 1 << 4;
const CONTROL_INT: u8 = 1 << 3;

const STATUS_FRER: u8 = 1 << 2;
const STATUS_RXRD: u8 = 1 << 1;
const STATUS_TXFL: u8 = 1 << 0;

/// Z80 cycles per 10-bit UART frame at the baud rate selected by $05 bits 7-6.
///
/// cycles = round(Z80_CLOCK_HZ * 10 / baud):
/// - 00 = 4800 bps ->   7,457 cycles/byte
/// - 01 = 2400 bps ->  14,915 cycles/byte
/// - 10 = 1200 bps ->  29,830 cycles/byte
/// - 11 =  300 bps -> 119,318 cycles/byte
const fn cycles_per_frame(control: u8) -> u32 {
    // Computed from the constants above; spelled out so the mapping is greppable
    const _: () = assert!(Z80_CLOCK_HZ * BITS_PER_FRAME == 35_795_450);
    match control >> 6 {
        0 => 7_457,    // 4800 bps
        1 => 14_915,   // 2400 bps
        2 => 29_830,   // 1200 bps
        3 => 119_318,  // 300 bps
        _ => unreachable!(),
    }
}

/// State of the Game Gear serial port (I/O ports $03-$05).
///
/// The "cable" is pluggable: a completed outgoing byte is surfaced via
/// [`GgSerial::take_tx`], and incoming bytes are injected via
/// [`GgSerial::deliver_rx`]. With no cable attached (nobody calls those),
/// transmitted bytes drain into the void after the baud delay and the RX
/// buffer never becomes ready — i.e. standalone-hardware behavior.
#[derive(Debug, Clone, Encode, Decode)]
pub struct GgSerial {
    /// $05 writable bits (BS1 BS0 RON TON INT); low 3 bits always zero here
    control: u8,
    /// $03 TX data register (R/W)
    tx_data: u8,
    /// TXFL: $03 has been written but not yet transferred to the shift register
    tx_full: bool,
    /// Byte currently being shifted out on the wire
    tx_shift: u8,
    tx_active: bool,
    tx_cycles_remaining: u32,
    /// Completed outgoing byte awaiting pickup by the cable (overwritten by
    /// the next completed byte if never taken — the void-drain case)
    tx_out: Option<u8>,
    /// $04 RX data register (R)
    rx_data: u8,
    /// RXRD: receive data is present in $04
    rx_ready: bool,
    /// FRER: framing error
    framing_error: bool,
    /// Byte currently being shifted in from the wire
    rx_shift: u8,
    rx_active: bool,
    rx_cycles_remaining: u32,
    /// The in-flight frame was clobbered mid-reception (wire collision);
    /// FRER will be raised when the frame completes
    rx_garbled: bool,
    /// INT: an NMI edge is being asserted (byte received with INT enabled)
    nmi_pending: bool,
}

impl GgSerial {
    pub fn new() -> Self {
        Self {
            // Power-on: BS=00 (4800 bps), RON/TON/INT off, no status flags set
            control: 0x00,
            tx_data: 0x00,
            tx_full: false,
            tx_shift: 0x00,
            tx_active: false,
            tx_cycles_remaining: 0,
            tx_out: None,
            rx_data: 0xFF,
            rx_ready: false,
            framing_error: false,
            rx_shift: 0x00,
            rx_active: false,
            rx_cycles_remaining: 0,
            rx_garbled: false,
            nmi_pending: false,
        }
    }

    fn ron(&self) -> bool {
        self.control & CONTROL_RON != 0
    }

    fn ton(&self) -> bool {
        self.control & CONTROL_TON != 0
    }

    fn int_enabled(&self) -> bool {
        self.control & CONTROL_INT != 0
    }

    /// $03 write: latch send data. TXFL raises until the byte moves into the
    /// shift register (immediately on the next tick if TON=1 and the shifter
    /// is idle).
    pub fn write_tx(&mut self, value: u8) {
        self.tx_data = value;
        self.tx_full = true;
    }

    /// $03 read: the TX data register is documented R/W; reads return the
    /// last value written.
    pub fn read_tx(&self) -> u8 {
        self.tx_data
    }

    /// $04 read: return receive data. Modeled to consume the byte: clears
    /// RXRD and FRER and acknowledges the receive NMI (the manual does not
    /// document the clearing mechanism; this matches typical UART data-read
    /// semantics).
    pub fn read_rx(&mut self) -> u8 {
        self.rx_ready = false;
        self.framing_error = false;
        self.nmi_pending = false;
        self.rx_data
    }

    /// $05 read: writable control bits plus live status flags.
    pub fn read_control(&self) -> u8 {
        (self.control & CONTROL_WRITE_MASK)
            | (u8::from(self.framing_error) * STATUS_FRER)
            | (u8::from(self.rx_ready) * STATUS_RXRD)
            | (u8::from(self.tx_full) * STATUS_TXFL)
    }

    /// $05 write: D7-D3 are stored; D2-D0 (status) are unaffected.
    pub fn write_control(&mut self, value: u8) {
        self.control = value & CONTROL_WRITE_MASK;

        if !self.ton() {
            // Transmitter disabled: PC4 stops being driven, killing any
            // in-flight frame. A byte still latched in $03 (TXFL) is kept and
            // will transmit if TON is re-enabled.
            self.tx_active = false;
        }
        if !self.ron() {
            // Receiver disabled: abandon any in-flight frame
            self.rx_active = false;
            self.rx_garbled = false;
        }
        if !self.int_enabled() {
            self.nmi_pending = false;
        }
    }

    /// True while the receive-complete NMI line should be asserted (low).
    pub fn nmi_pending(&self) -> bool {
        self.nmi_pending
    }

    /// Cable-side: take a byte that the game finished transmitting (i.e. the
    /// full 10-bit frame has been shifted out at the selected baud rate).
    pub fn take_tx(&mut self) -> Option<u8> {
        self.tx_out.take()
    }

    /// Cable-side: a byte starts arriving from the peer. After a full frame
    /// time at *this* receiver's selected baud rate it lands in the RX buffer
    /// and RXRD raises (plus an NMI if INT is enabled).
    ///
    /// - If RON=0 the receiver is not listening and the byte is dropped.
    /// - If a frame is already mid-reception, the wire is being driven with
    ///   overlapping frames: the newest data wins and FRER raises when the
    ///   (garbled) frame completes.
    pub fn deliver_rx(&mut self, byte: u8) {
        if !self.ron() {
            return;
        }

        if self.rx_active {
            self.rx_garbled = true;
        }
        self.rx_shift = byte;
        self.rx_active = true;
        self.rx_cycles_remaining = cycles_per_frame(self.control);
    }

    /// Advance the serial engine by the given number of emulated Z80 T-cycles.
    pub fn tick(&mut self, z80_cycles: u32) {
        // Transmit side: load the shift register first so that a byte written
        // just before this tick window starts transmitting at the start of it
        if self.ton() && !self.tx_active && self.tx_full {
            // Move $03 into the shift register and start clocking it out
            self.tx_shift = self.tx_data;
            self.tx_full = false;
            self.tx_active = true;
            self.tx_cycles_remaining = cycles_per_frame(self.control);
        }
        if self.tx_active {
            if self.tx_cycles_remaining > z80_cycles {
                self.tx_cycles_remaining -= z80_cycles;
            } else {
                self.tx_active = false;
                // Surface the byte for the cable; if no cable ever takes it,
                // it is simply overwritten by the next one (void drain)
                self.tx_out = Some(self.tx_shift);
            }
        }

        // Receive side
        if self.rx_active {
            if self.rx_cycles_remaining > z80_cycles {
                self.rx_cycles_remaining -= z80_cycles;
            } else {
                self.rx_active = false;
                // Receive buffer overrun simply overwrites the previous byte;
                // the hardware has no overrun flag (FRER is strictly a
                // stop-bit framing error), so RXRD just stays set
                self.rx_data = self.rx_shift;
                self.rx_ready = true;
                if self.rx_garbled {
                    self.framing_error = true;
                }
                self.rx_garbled = false;
                if self.int_enabled() {
                    self.nmi_pending = true;
                }
            }
        }
    }
}

impl Default for GgSerial {
    fn default() -> Self {
        Self::new()
    }
}

/// Cheap trace of "interesting" I/O port accesses, used by the frontend to
/// classify a ROM's multiplayer shape:
/// - [`PortTrace::READ_DC`] / [`PortTrace::READ_DD`] (joypad ports carrying
///   player-2 bits) suggest a shared-console two-player game
/// - [`PortTrace::SERIAL_PORTS`] ($03-$05) suggests Gear-to-Gear link play
/// - [`PortTrace::PARALLEL_PORTS`] ($01-$02, EXT connector parallel mode)
///
/// Flags are only ever OR-ed in from the I/O dispatch (O(1)); they are never
/// cleared during emulation. Note that $DC/$DD are mirrored throughout
/// $C0-$FF, and the flags trigger on any mirror. The trace is included in
/// save states (it lives inside `Memory`, which is bincode-encoded whole).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Encode, Decode)]
pub struct PortTrace(u8);

impl PortTrace {
    /// A read of I/O port $DC (or a mirror): joypad A/B, includes P2 up/down
    pub const READ_DC: Self = Self(1 << 0);
    /// A read of I/O port $DD (or a mirror): joypad B/misc, includes P2 left/right/buttons
    pub const READ_DD: Self = Self(1 << 1);
    /// Any access to Game Gear serial ports $03/$04/$05
    pub const SERIAL_PORTS: Self = Self(1 << 2);
    /// Any access to Game Gear EXT parallel ports $01/$02
    pub const PARALLEL_PORTS: Self = Self(1 << 3);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TON+RON enabled, INT off, at the given baud bits
    fn enabled_serial(baud_bits: u8) -> GgSerial {
        let mut serial = GgSerial::new();
        serial.write_control((baud_bits << 6) | CONTROL_RON | CONTROL_TON);
        serial
    }

    // Pump the "cable": move any completed TX byte from one side to the other
    fn pump(a: &mut GgSerial, b: &mut GgSerial) {
        if let Some(byte) = a.take_tx() {
            b.deliver_rx(byte);
        }
        if let Some(byte) = b.take_tx() {
            a.deliver_rx(byte);
        }
    }

    #[test]
    fn baud_divisor_table() {
        assert_eq!(cycles_per_frame(0b00 << 6), 7_457); // 4800 bps
        assert_eq!(cycles_per_frame(0b01 << 6), 14_915); // 2400 bps
        assert_eq!(cycles_per_frame(0b10 << 6), 29_830); // 1200 bps
        assert_eq!(cycles_per_frame(0b11 << 6), 119_318); // 300 bps
    }

    #[test]
    fn back_to_back_exchange_at_each_baud() {
        for baud_bits in 0..4u8 {
            let frame = cycles_per_frame(baud_bits << 6);
            let mut a = enabled_serial(baud_bits);
            let mut b = enabled_serial(baud_bits);

            a.write_tx(0xA5);
            b.write_tx(0x3C);
            assert_eq!(a.read_control() & STATUS_TXFL, STATUS_TXFL);

            // Interleave small ticks with cable pumping, like the door would
            let mut cycles = 0;
            while cycles < 3 * frame {
                a.tick(23);
                b.tick(23);
                pump(&mut a, &mut b);
                cycles += 23;

                if a.read_control() & STATUS_RXRD != 0 && b.read_control() & STATUS_RXRD != 0 {
                    break;
                }
            }

            assert_ne!(a.read_control() & STATUS_RXRD, 0, "baud bits {baud_bits}: A never got RXRD");
            assert_ne!(b.read_control() & STATUS_RXRD, 0, "baud bits {baud_bits}: B never got RXRD");
            assert_eq!(a.read_rx(), 0x3C);
            assert_eq!(b.read_rx(), 0xA5);
            // Reading $04 consumed the byte
            assert_eq!(a.read_control() & STATUS_RXRD, 0);
            assert_eq!(b.read_control() & STATUS_RXRD, 0);
            // No errors on a clean exchange
            assert_eq!(a.read_control() & STATUS_FRER, 0);
            assert_eq!(b.read_control() & STATUS_FRER, 0);
        }
    }

    #[test]
    fn tx_timing_honors_baud_delay() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.write_tx(0x42);
        assert_ne!(serial.read_control() & STATUS_TXFL, 0, "TXFL raised on $03 write");
        serial.tick(0); // loads shift register, starts transmission
        assert_eq!(serial.read_control() & STATUS_TXFL, 0, "byte moved to shift register");
        assert_eq!(serial.take_tx(), None, "nothing out before the frame time elapses");

        serial.tick(frame - 1);
        assert_eq!(serial.take_tx(), None, "one cycle early: still shifting");

        serial.tick(1);
        assert_eq!(serial.take_tx(), Some(0x42));
        assert_eq!(serial.take_tx(), None, "byte only surfaces once");
    }

    #[test]
    fn rx_timing_honors_baud_delay() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.deliver_rx(0x99);
        serial.tick(frame - 1);
        assert_eq!(serial.read_control() & STATUS_RXRD, 0, "not ready before frame time");
        serial.tick(1);
        assert_ne!(serial.read_control() & STATUS_RXRD, 0);
        assert_eq!(serial.read_rx(), 0x99);
    }

    #[test]
    fn tx_double_buffering() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.write_tx(0x11);
        serial.tick(0); // 0x11 -> shift register, TXFL clears
        assert_eq!(serial.read_control() & STATUS_TXFL, 0);

        // Queue a second byte while the first is on the wire
        serial.write_tx(0x22);
        assert_ne!(serial.read_control() & STATUS_TXFL, 0, "buffer full while shifting");

        serial.tick(frame); // finish first byte
        assert_eq!(serial.take_tx(), Some(0x11));

        serial.tick(0); // load second byte
        assert_eq!(serial.read_control() & STATUS_TXFL, 0, "second byte moved to shifter");

        serial.tick(frame);
        assert_eq!(serial.take_tx(), Some(0x22));
    }

    #[test]
    fn overrun_overwrites_rx_buffer_without_framing_error() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.deliver_rx(0x01);
        serial.tick(frame);
        assert_ne!(serial.read_control() & STATUS_RXRD, 0);

        // Second byte fully received before the game reads the first
        serial.deliver_rx(0x02);
        serial.tick(frame);

        assert_ne!(serial.read_control() & STATUS_RXRD, 0);
        assert_eq!(serial.read_control() & STATUS_FRER, 0, "overrun is not a framing error");
        assert_eq!(serial.read_rx(), 0x02, "newest byte wins");
    }

    #[test]
    fn mid_frame_collision_sets_framing_error() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.deliver_rx(0x01);
        serial.tick(frame / 2);
        // A second frame starts while the first is still shifting in
        serial.deliver_rx(0x02);
        serial.tick(frame);

        assert_ne!(serial.read_control() & STATUS_RXRD, 0);
        assert_ne!(serial.read_control() & STATUS_FRER, 0, "garbled frame raises FRER");
        assert_eq!(serial.read_rx(), 0x02);
        // Reading $04 clears FRER
        assert_eq!(serial.read_control() & STATUS_FRER, 0);
    }

    #[test]
    fn receiver_disabled_drops_bytes() {
        let frame = cycles_per_frame(0);
        let mut serial = GgSerial::new();
        serial.write_control(CONTROL_TON); // RON off

        serial.deliver_rx(0x55);
        serial.tick(2 * frame);
        assert_eq!(serial.read_control() & STATUS_RXRD, 0);
    }

    #[test]
    fn transmitter_disabled_holds_byte() {
        let frame = cycles_per_frame(0);
        let mut serial = GgSerial::new();
        serial.write_control(CONTROL_RON); // TON off

        serial.write_tx(0x77);
        serial.tick(2 * frame);
        assert_ne!(serial.read_control() & STATUS_TXFL, 0, "byte held while TON=0");
        assert_eq!(serial.take_tx(), None);

        // Enabling TON releases it
        serial.write_control(CONTROL_RON | CONTROL_TON);
        serial.tick(frame);
        assert_eq!(serial.take_tx(), Some(0x77));
    }

    #[test]
    fn nmi_asserted_on_receive_when_int_enabled() {
        let frame = cycles_per_frame(0);
        let mut serial = GgSerial::new();
        serial.write_control(CONTROL_RON | CONTROL_INT);

        serial.deliver_rx(0xE0);
        assert!(!serial.nmi_pending());
        serial.tick(frame);
        assert!(serial.nmi_pending());

        // Reading $04 acknowledges the NMI
        assert_eq!(serial.read_rx(), 0xE0);
        assert!(!serial.nmi_pending());

        // Disabling INT also deasserts a pending NMI
        serial.deliver_rx(0xE1);
        serial.tick(frame);
        assert!(serial.nmi_pending());
        serial.write_control(CONTROL_RON);
        assert!(!serial.nmi_pending());
    }

    #[test]
    fn status_bits_not_writable_via_port_05() {
        let frame = cycles_per_frame(0);
        let mut serial = enabled_serial(0);

        serial.deliver_rx(0x10);
        serial.tick(frame);
        assert_ne!(serial.read_control() & STATUS_RXRD, 0);

        // Attempt to clear status bits by writing zeros to them
        serial.write_control(CONTROL_RON | CONTROL_TON);
        assert_ne!(serial.read_control() & STATUS_RXRD, 0, "RXRD unaffected by $05 write");
    }

    #[test]
    fn port_trace_flags() {
        let mut trace = PortTrace::empty();
        assert!(trace.is_empty());

        trace.insert(PortTrace::READ_DC);
        trace.insert(PortTrace::SERIAL_PORTS);
        assert!(trace.contains(PortTrace::READ_DC));
        assert!(trace.contains(PortTrace::SERIAL_PORTS));
        assert!(!trace.contains(PortTrace::READ_DD));
        assert!(!trace.contains(PortTrace::PARALLEL_PORTS));
        assert_eq!(trace.bits(), 0b0101);
    }
}
