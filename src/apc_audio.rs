//! Stream the emulator's PCM to a SyncTERM-APC-capable terminal as a sequence of
//! short clips, kept in sync with wall-clock by a self-correcting timeline.
//!
//! Wire format (per the released cterm SyncTERM:A audio doc):
//!   ESC _ SyncTERM:C;S;<name>;<base64-wav> ESC \         (cache the clip)
//!   ESC _ SyncTERM:A;Load;S=<slot>;<name>  ESC \         (decode into a slot)
//!   ESC _ SyncTERM:A;Queue;C=<chan>;S=<slot> ESC \       (play on a channel)
//!
//! Sync model. The emulator runs locked to wall-clock, so each produced sample
//! already carries a timestamp: its position on the audio timeline *is* the
//! real time it was produced. The terminal drains the channel FIFO at a fixed
//! realtime rate. With a wall-clock producer and a realtime consumer, latency is
//! just the initial buffering delay -- it only *grows* if we keep sending audio
//! the saturated link can't clear. So every tick we send only as much as the
//! schedule asks for and reconcile the rest:
//!
//!   * the producer drifting a little faster than realtime -> declare a slightly
//!     higher WAV sample-rate so the terminal plays the clip back compressed,
//!     absorbing the drift with no gap (the terminal's own resampler does the
//!     work). Bounded by MAX_CORRECTION_PCT, a pitch-perceptibility limit.
//!   * a big surplus from a stall/catch-up burst -> compress at the cap and drop
//!     the part that won't fit, so audio skips once to "now" instead of lagging.
//!
//! The only cushion we hold is the measured jitter of our own emit cadence, not
//! a tuned constant. The terminal's one feedback signal -- the `Update;C=`
//! one-shot `CSI = 7 ; <ch> ; 0 n` fired when the channel drains -- re-anchors
//! the timeline after an underrun.
//!
//! lamegear port notes: the smsgg-core delivers samples as (l, r) f64 pairs
//! already resampled to 48 kHz (jgenesis-common DEFAULT_OUTPUT_FREQUENCY), so
//! the front end here is a downmix + fractional linear resampler instead of
//! lameboy's integer decimator. Everything downstream of the mono S16 buffer
//! (chunking, pacing, drain handling, resync, the wire protocol) is unchanged.

use std::io::{self, Write};

/// Source rate: smsgg-core resamples the PSG internally and pushes stereo
/// samples at this fixed rate (see vendor/jgenesis-common/src/audio.rs
/// DEFAULT_OUTPUT_FREQUENCY and emu.rs AudioSink).
const SRC_RATE: u32 = 48_000;
/// Channel to play on (0-1 are reserved by SyncTERM for internal music/SFX).
const CHANNEL: u8 = 2;
/// Number of rotating slot/filename pairs to cycle through.
const SLOTS: u8 = 8;
/// Largest playback-rate nudge used to absorb drift, as a percent. This is a
/// psychoacoustic bound (a ~2% pitch shift is hard to notice on lo-fi PSG
/// audio), not a tuning knob -- surplus beyond what this can compress is
/// dropped instead.
const MAX_CORRECTION_PCT: f32 = 2.0;
/// Silence re-primed after a periodic resync flush, in ms. Just enough to bridge
/// the gap until the next real clip so playback resumes without dead air/a click.
const REPRIME_SILENCE_MS: u32 = 20;

/// Sysop-tunable APC parameters, mirrored from lameboy's `[door] audio_*` ini
/// keys. `chunk_ms` is the min clip / drop granularity; `rate` is the output
/// sample rate (the bandwidth lever -- lower keeps the link from saturating);
/// `resync_secs` is the period of the hard channel flush that caps the
/// terminal-side FIFO tail (0 = disabled). `resync_secs` is consumed by the
/// caller's timer, not by `ApcAudio` itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApcTuning {
    pub chunk_ms: u32,
    pub rate: u32,
    pub resync_secs: u32,
}

impl ApcTuning {
    pub const DEFAULT: ApcTuning = ApcTuning { chunk_ms: 40, rate: 22050, resync_secs: 60 };

    /// Clamp raw (e.g. ini-sourced) values into their supported ranges, matching
    /// lameboy's parser: chunk 10-250 ms, rate 5512-44100 Hz, resync 0 (off) or
    /// 5-3600 s.
    pub fn sanitized(self) -> ApcTuning {
        ApcTuning {
            chunk_ms: self.chunk_ms.clamp(10, 250),
            rate: self.rate.clamp(5512, 44100),
            resync_secs: if self.resync_secs == 0 { 0 } else { self.resync_secs.clamp(5, 3600) },
        }
    }
}

impl Default for ApcTuning {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A snapshot of the stream's health for the on-screen/log diagnostics.
#[derive(Clone, Copy, Default)]
pub struct ApcStats {
    pub lead_ms: u32,      // audio queued ahead of realtime (the live cushion)
    pub drift_pct: f32,    // produced-vs-wall clock skew, signed (the "ratio")
    pub correction_pct: f32, // playback-rate nudge applied on the last clip
    pub drops: u32,        // cumulative clips with dropped (skipped) audio
    pub rate: u32,         // output sample rate in Hz (bandwidth)
}

/// Streaming mono downsampler: SRC_RATE f32 in, `out_rate` f32 out, by linear
/// interpolation on an integer phase accumulator (deterministic -- no float
/// timing state). Output sample j sits at source position j * SRC_RATE /
/// out_rate; `acc` tracks the next output's offset inside the current source
/// interval, scaled by `out_rate` so all clock arithmetic stays integral.
struct MonoResampler {
    out_rate: u32,
    acc: u32,       // next output's intra-interval offset, in 1/out_rate units
    prev: f32,      // left edge of the current source interval
    has_prev: bool, // first input primes `prev` without emitting
}

impl MonoResampler {
    fn new(out_rate: u32) -> Self {
        Self { out_rate, acc: 0, prev: 0.0, has_prev: false }
    }

    /// Feed one mono source sample; `emit` receives 0..n output samples.
    /// (Downsampling from 48 kHz means at most one per input in practice, but
    /// the loop is general.)
    #[inline]
    fn push<F: FnMut(f32)>(&mut self, cur: f32, mut emit: F) {
        if !self.has_prev {
            // Prime: the first output then lands exactly on the first source
            // sample (acc == 0 within the interval [s0, s1]).
            self.prev = cur;
            self.has_prev = true;
            return;
        }
        while self.acc < self.out_rate {
            let frac = self.acc as f32 / self.out_rate as f32;
            emit(self.prev + (cur - self.prev) * frac);
            self.acc += SRC_RATE; // next output is SRC/OUT source intervals on
        }
        self.acc -= self.out_rate; // advance one source interval
        self.prev = cur;
    }
}

pub struct ApcAudio {
    out_rate: u32, // output sample rate after resampling (bandwidth lever)
    min_send: u64, // smallest clip we bother emitting, ms (avoids tiny WAVs)

    // Downmix + resample front end (SRC_RATE stereo -> out_rate mono).
    resampler: MonoResampler,
    accum: Vec<i16>, // mono S16 @ out_rate awaiting emission

    slot: u8,
    primed: bool,
    play_start_ms: u64, // wall-clock anchor for the realtime playback timeline
    emitted_ms: u64,    // realtime audio handed to the terminal since the anchor

    // Measured cushion: the rolling max gap between emit calls (link jitter).
    last_now_ms: u64,
    jitter_ms: u64,

    // Diagnostics.
    start_ms: u64,
    produced_samples: u64,
    drops: u32,
    correction_pct: f32,
    lead_ms: u32,
}

impl ApcAudio {
    /// `min_chunk_ms` is the smallest clip we emit (drop granularity / overhead
    /// floor). `out_rate_hint` is the desired output sample rate (lower = less
    /// bandwidth, the lever that keeps the link from saturating in the first
    /// place); unlike lameboy's integer decimator it is honored exactly, only
    /// clamped to 5512..=SRC_RATE. Pass `ApcTuning`'s `chunk_ms` and `rate`.
    pub fn new(min_chunk_ms: u32, out_rate_hint: u32) -> Self {
        let out_rate = out_rate_hint.clamp(5512, SRC_RATE);
        Self {
            out_rate,
            min_send: min_chunk_ms.max(5) as u64,
            resampler: MonoResampler::new(out_rate),
            accum: Vec::with_capacity((out_rate as usize / 1000) * 256 + 64),
            slot: 0,
            primed: false,
            play_start_ms: 0,
            emitted_ms: 0,
            last_now_ms: 0,
            jitter_ms: 0,
            start_ms: 0,
            produced_samples: 0,
            drops: 0,
            correction_pct: 0.0,
            lead_ms: 0,
        }
    }

    /// Feed stereo (l, r) f64 [-1,1] @ SRC_RATE (48 kHz) -- the shape smsgg-core
    /// pushes into `emu::AudioSink::samples`. Accumulates only (no I/O); the
    /// main loop drains `emu.audio.samples` into here each frame.
    pub fn push_samples(&mut self, samples: &[(f64, f64)]) {
        let (accum, produced) = (&mut self.accum, &mut self.produced_samples);
        for &(l, r) in samples {
            let mono = (0.5 * (l + r)) as f32;
            self.resampler.push(mono, |s| {
                accum.push(f32_to_i16(s));
                *produced += 1;
            });
        }
    }

    /// Send one reconciled clip for this tick. `now_ms` is a monotonic wall clock
    /// (e.g. session-start elapsed). Call once per frame.
    pub fn emit_ready<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        if !self.primed {
            self.play_start_ms = now_ms;
            self.start_ms = now_ms;
            self.last_now_ms = now_ms;
            self.emitted_ms = 0;
            self.primed = true;
            self.arm_update(out)?;
            out.flush()?;
        }

        // Measured cushion: decaying max of the interval between emit calls.
        let gap = now_ms.saturating_sub(self.last_now_ms);
        self.last_now_ms = now_ms;
        self.jitter_ms = (self.jitter_ms * 15 / 16).max(gap);
        let target = self.min_send.max(self.jitter_ms);

        let elapsed = now_ms.saturating_sub(self.play_start_ms);
        self.lead_ms = self.emitted_ms.saturating_sub(elapsed) as u32;

        // How much realtime audio the schedule wants queued by now, beyond what
        // we've already sent.
        let need_ms = (elapsed + target).saturating_sub(self.emitted_ms);
        let avail = self.accum.len();
        let avail_ms = avail as u64 * 1000 / self.out_rate as u64;
        if need_ms == 0 || avail == 0 {
            return Ok(());
        }
        // Ahead of schedule with only a sliver buffered: wait for a fuller clip.
        if avail_ms < need_ms && avail_ms < self.min_send {
            return Ok(());
        }

        // Work the ratio in samples, not whole ms: at small tick sizes integer-ms
        // resolution is far coarser than the 2% correction band.
        let need_samples = need_ms as f32 * self.out_rate as f32 / 1000.0;
        let max_ratio = 1.0 + MAX_CORRECTION_PCT / 100.0;
        let (send_samples, rate, played_ms, dropped) = if (avail as f32) <= need_samples {
            // Keeping up or behind: send everything at its true rate.
            (avail, self.out_rate, avail_ms, 0usize)
        } else {
            let ratio = avail as f32 / need_samples;
            if ratio <= max_ratio {
                // Small drift: compress all of it into need_ms by declaring a
                // faster rate; the terminal resamples and the drift vanishes.
                let rate = (self.out_rate as f32 * ratio).round() as u32;
                (avail, rate, need_ms, 0)
            } else {
                // Big surplus (stall): compress at the cap, drop the oldest part
                // that still won't fit so playback skips once to the present.
                let keep = ((need_samples * max_ratio) as usize).min(avail);
                let drop = avail - keep;
                let rate = (self.out_rate as f32 * max_ratio).round() as u32;
                (keep, rate, need_ms, drop)
            }
        };

        if dropped > 0 {
            self.accum.drain(0..dropped);
            self.drops += 1;
        }
        self.correction_pct = (rate as f32 / self.out_rate as f32 - 1.0) * 100.0;
        self.emit_chunk(send_samples, rate, out)?;
        self.emitted_ms += played_ms;
        out.flush()?;
        Ok(())
    }

    /// Handle the terminal's `CSI = 7 ; <ch> ; 0 n` drain notification: the FIFO
    /// emptied, so the playback timeline is stale. Re-anchor to "queued == 0 now"
    /// and re-arm the one-shot notification.
    pub fn notify_drain<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        self.play_start_ms = now_ms;
        self.emitted_ms = 0;
        self.lead_ms = 0;
        self.arm_update(out)?;
        out.flush()
    }

    /// Periodic "engine restart": drop the latency that piles up in the
    /// terminal's channel FIFO. The producer runs a hair faster than realtime, so
    /// each second it hands over slightly more audio than the terminal plays; the
    /// surplus is never dropped (our own accounting reads lead==0, cor==0) and
    /// accumulates as an ever-growing unplayed tail in the terminal's FIFO — audio
    /// drifts further behind the picture the longer a game runs. The baseline
    /// emit_ready control can't see or clear that tail. So on a timer we hard-reset
    /// it: `A;Flush;C=2` frees the terminal's whole head..tail backlog, we drop our
    /// own pending accum, re-anchor the timeline to "queued == 0 now" (as
    /// notify_drain does), re-arm the one-shot drain notify (the flush leaves the
    /// channel idle and can itself trip a drain report), and re-prime a few ms of
    /// silence so playback resumes on the next tick without a gap or click. The
    /// cost is one brief skip per interval, capping the tail at interval * drift.
    pub fn resync<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        // Not primed yet: nothing has been queued, so there is no tail to drop.
        if !self.primed {
            return Ok(());
        }
        // 1) Flush the terminal channel FIFO (head..tail) — drops the whole tail.
        out.write_all(b"\x1b_SyncTERM:A;Flush;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b"\x1b\\")?;
        // 2) Drop our own pending backlog so we don't re-ship the flushed audio.
        self.accum.clear();
        // 3) Re-anchor the realtime timeline to "queued == 0 now" (like notify_drain).
        self.play_start_ms = now_ms;
        self.emitted_ms = 0;
        self.lead_ms = 0;
        // 4) Re-arm the one-shot drain notify (the flush idles the channel).
        self.arm_update(out)?;
        // 5) Re-prime a small silence cushion so playback resumes immediately with
        //    no dead air or click; count it as emitted so the schedule stays honest.
        let cushion = (self.out_rate as usize * REPRIME_SILENCE_MS as usize / 1000).max(1);
        let silence = vec![0i16; cushion];
        self.emit_raw(&silence, self.out_rate, out)?;
        self.emitted_ms += cushion as u64 * 1000 / self.out_rate as u64;
        out.flush()
    }

    /// On exit, flush the channel FIFO so no queued audio plays after the door
    /// returns to the menu (the latency "tail").
    pub fn stop<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        out.write_all(b"\x1b_SyncTERM:A;Flush;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b"\x1b\\")?;
        out.flush()
    }

    /// Current stream health for diagnostics. `now_ms` matches `emit_ready`.
    pub fn stats(&self, now_ms: u64) -> ApcStats {
        let wall = now_ms.saturating_sub(self.start_ms).max(1);
        let produced_ms = self.produced_samples * 1000 / self.out_rate as u64;
        ApcStats {
            lead_ms: self.lead_ms,
            drift_pct: (produced_ms as f32 / wall as f32 - 1.0) * 100.0,
            correction_pct: self.correction_pct,
            drops: self.drops,
            rate: self.out_rate,
        }
    }

    /// Arm the one-shot drain notification on our channel.
    fn arm_update<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        out.write_all(b"\x1b_SyncTERM:A;Update;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b"\x1b\\")?;
        Ok(())
    }

    /// Emit one clip of `n` samples at the given declared sample rate, as
    /// Store+Load+Queue. The declared rate is the resample knob: higher than the
    /// true `out_rate` makes the terminal play the clip back faster (compressed).
    fn emit_chunk<W: Write + ?Sized>(&mut self, n: usize, rate: u32, out: &mut W) -> io::Result<()> {
        let wav = encode_wav_mono(&self.accum[..n], rate);
        self.accum.drain(0..n);
        self.emit_wav(&wav, out)
    }

    /// Emit an arbitrary sample slice (not from `accum`) as one Store+Load+Queue
    /// clip at the given declared rate. Used to re-prime the silence cushion after
    /// a periodic resync flush, where `accum` has just been cleared.
    fn emit_raw<W: Write + ?Sized>(&mut self, samples: &[i16], rate: u32, out: &mut W) -> io::Result<()> {
        let wav = encode_wav_mono(samples, rate);
        self.emit_wav(&wav, out)
    }

    /// Store+Load+Queue an already-encoded WAV on our channel, cycling the slot.
    fn emit_wav<W: Write + ?Sized>(&mut self, wav: &[u8], out: &mut W) -> io::Result<()> {
        let b64 = base64_encode(wav);
        let slot = self.slot;
        self.slot = (self.slot + 1) % SLOTS;

        out.write_all(b"\x1b_SyncTERM:C;S;g")?;
        write_u8_dec(out, slot)?;
        out.write_all(b";")?;
        out.write_all(&b64)?;
        out.write_all(b"\x1b\\")?;

        out.write_all(b"\x1b_SyncTERM:A;Load;S=")?;
        write_u8_dec(out, slot)?;
        out.write_all(b";g")?;
        write_u8_dec(out, slot)?;
        out.write_all(b"\x1b\\")?;

        out.write_all(b"\x1b_SyncTERM:A;Queue;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b";S=")?;
        write_u8_dec(out, slot)?;
        out.write_all(b"\x1b\\")?;
        Ok(())
    }
}

#[inline]
fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn write_u8_dec<W: Write + ?Sized>(out: &mut W, n: u8) -> io::Result<()> {
    let mut buf = [0u8; 3];
    let s = {
        let mut i = 3;
        let mut v = n;
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10);
            v /= 10;
            if v == 0 {
                break;
            }
        }
        &buf[i..]
    };
    out.write_all(s)
}

/// Minimal canonical PCM WAV (RIFF/WAVE, S16) for one mono buffer.
fn encode_wav_mono(samples: &[i16], rate: u32) -> Vec<u8> {
    let ch: u16 = 1;
    let bits: u16 = 16;
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = rate * ch as u32 * (bits / 8) as u32;
    let block_align = ch * (bits / 8);
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&ch.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (with `=` padding) — what the terminal's decoder expects.
fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b0 = c[0];
        let b1 = *c.get(1).unwrap_or(&0);
        let b2 = *c.get(2).unwrap_or(&0);
        out.push(B64[(b0 >> 2) as usize]);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        out.push(if c.len() > 1 { B64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] } else { b'=' });
        out.push(if c.len() > 2 { B64[(b2 & 0x3f) as usize] } else { b'=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// Push `ms` of constant-level stereo @ SRC_RATE (48 kHz f64 pairs).
    fn push_ms(a: &mut ApcAudio, ms: u32) {
        let frames = (SRC_RATE * ms / 1000) as usize;
        a.push_samples(&vec![(0.2f64, 0.2f64); frames]);
    }

    /// Push stereo frames one at a time until `accum` holds exactly `want`
    /// mono output samples (sidesteps hand-computing the resampler phase).
    fn push_until_accum(a: &mut ApcAudio, want: usize) {
        while a.accum.len() < want {
            a.push_samples(&[(0.2f64, 0.2f64)]);
        }
        assert_eq!(a.accum.len(), want, "resampler emits at most 1 per input");
    }

    // ---- resampler ----

    fn resample_all(out_rate: u32, input: &[f32]) -> Vec<f32> {
        let mut r = MonoResampler::new(out_rate);
        let mut out = Vec::new();
        for &x in input {
            r.push(x, |s| out.push(s));
        }
        out
    }

    #[test]
    fn resampler_2_to_1_is_pure_decimation() {
        // 48000 -> 24000: output j sits exactly on source sample 2j.
        let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let out = resample_all(24000, &input);
        // 15 intervals * 24000/48000 -> 8 outputs (j*2 for j=0..7).
        assert_eq!(out, vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0]);
    }

    #[test]
    fn resampler_interpolates_between_source_samples() {
        // 48000 -> 32000: output positions 0, 1.5, 3.0, ... in source units.
        let input: Vec<f32> = (0..7).map(|i| i as f32 * 10.0).collect();
        let out = resample_all(32000, &input);
        assert_eq!(out, vec![0.0, 15.0, 30.0, 45.0]);
    }

    #[test]
    fn resampler_rate_is_exact_over_long_runs() {
        // n inputs cover (n-1) source intervals; outputs are every j with
        // j*SRC/OUT strictly inside that span, i.e. exactly OUT per second.
        for &out_rate in &[22050u32, 11025, 8000, 44100, 5512] {
            let n = SRC_RATE as usize + 1; // exactly 1 s of source intervals
            let out = resample_all(out_rate, &vec![0.5f32; n]);
            assert_eq!(out.len(), out_rate as usize, "rate {}", out_rate);
        }
    }

    #[test]
    fn resampler_passthrough_at_source_rate() {
        // OUT == SRC reproduces the input with one sample of latency (an
        // output landing exactly on sample i is emitted when i+1 arrives).
        let input: Vec<f32> = (0..5).map(|i| i as f32).collect();
        assert_eq!(resample_all(SRC_RATE, &input), input[..4].to_vec());
    }

    #[test]
    fn push_samples_downmixes_and_scales() {
        // Hard-panned full-scale halves downmix to 0.5 -> ~16383 in S16.
        let mut a = ApcAudio::new(40, SRC_RATE); // passthrough resampler
        a.push_samples(&[(1.0, 0.0), (1.0, 0.0)]);
        assert_eq!(a.accum.len(), 1); // first frame primes the resampler
        assert_eq!(a.accum[0], (0.5f32 * 32767.0) as i16);
    }

    // ---- protocol / chunker ----

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), b"");
        assert_eq!(base64_encode(b"f"), b"Zg==");
        assert_eq!(base64_encode(b"fo"), b"Zm8=");
        assert_eq!(base64_encode(b"foo"), b"Zm9v");
        assert_eq!(base64_encode(b"foobar"), b"Zm9vYmFy");
    }

    #[test]
    fn wav_header_is_canonical() {
        let w = encode_wav_mono(&[0, 1, -1, 100], 22050);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[36..40], b"data");
        // Declared rate lands in the fmt chunk's sample-rate field (offset 24).
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 22050);
        assert_eq!(w.len(), 44 + 4 * 2);
    }

    #[test]
    fn rate_hint_is_honored_and_clamped() {
        // Fractional resampling honors the hint exactly (no divisor snapping).
        assert_eq!(ApcAudio::new(40, 22050).out_rate, 22050);
        assert_eq!(ApcAudio::new(40, 11025).out_rate, 11025);
        assert_eq!(ApcAudio::new(40, 1).out_rate, 5512);
        assert_eq!(ApcAudio::new(40, 96000).out_rate, SRC_RATE);
    }

    #[test]
    fn tuning_sanitize_matches_ini_clamps() {
        let t = ApcTuning { chunk_ms: 1, rate: 999_999, resync_secs: 1 }.sanitized();
        assert_eq!((t.chunk_ms, t.rate, t.resync_secs), (10, 44100, 5));
        let t = ApcTuning { chunk_ms: 999, rate: 1, resync_secs: 99_999 }.sanitized();
        assert_eq!((t.chunk_ms, t.rate, t.resync_secs), (250, 5512, 3600));
        let t = ApcTuning { chunk_ms: 40, rate: 22050, resync_secs: 0 }.sanitized();
        assert_eq!(t.resync_secs, 0, "0 stays 0 (resync disabled)");
        assert_eq!(ApcTuning::default(), ApcTuning::DEFAULT);
        assert_eq!(
            (ApcTuning::DEFAULT.chunk_ms, ApcTuning::DEFAULT.rate, ApcTuning::DEFAULT.resync_secs),
            (40, 22050, 60)
        );
    }

    #[test]
    fn emits_and_arms_drain_notify_on_first_tick() {
        let mut a = ApcAudio::new(40, 22050);
        push_ms(&mut a, 60);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap();
        assert!(count(&out, b"A;Queue;C=2") >= 1, "queued a clip");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "armed the drain notify once");
    }

    #[test]
    fn small_drift_speeds_up_without_dropping() {
        // Compression only acts once the cushion is full, so set up that steady
        // state directly: emitted sits right at the cushion edge (elapsed+target),
        // a small emit gap keeps target at min_send, and we then produce a clip
        // ~1.6% longer than the schedule's per-tick need.
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap(); // prime
        a.last_now_ms = 980;
        a.jitter_ms = 20; // target = min_send(40).max(20) = 40
        a.emitted_ms = 1020; // = elapsed(1000) + target(40) - need(20)
        // need = 20ms = 441 samples @22050; buffer 448 (~1.6% over) so it compresses.
        push_until_accum(&mut a, 448);
        a.emit_ready(&mut out, 1000).unwrap();
        let s = a.stats(1000);
        assert_eq!(s.drops, 0, "no drops for sub-2% drift");
        assert!(s.correction_pct > 0.5 && s.correction_pct <= MAX_CORRECTION_PCT + 0.01,
            "applied a small speed-up, got {}", s.correction_pct);
    }

    #[test]
    fn big_surplus_is_dropped_and_capped() {
        // A stall dumps 1s of audio at once with the clock barely advanced.
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap(); // prime
        push_ms(&mut a, 1000);
        a.emit_ready(&mut out, 60).unwrap();
        let s = a.stats(60);
        assert_eq!(s.drops, 1, "dropped the un-absorbable surplus once");
        assert!(s.correction_pct <= MAX_CORRECTION_PCT + 0.01, "speed-up stayed within the cap");
        // Lead is bounded to the measured cushion, not the 1s we produced.
        assert!(s.lead_ms < 200, "lead {} should be bounded", s.lead_ms);
    }

    #[test]
    fn drain_notification_reanchors_and_rearms() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.notify_drain(&mut out, 1234).unwrap();
        assert_eq!(a.play_start_ms, 1234, "re-anchored to drain time");
        assert_eq!(a.emitted_ms, 0, "queued-ahead reset to zero");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "re-armed the drain notify");
    }

    #[test]
    fn resync_flushes_reanchors_rearms_and_reprimes() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        // Prime + queue a clip so there's a live timeline and a pending backlog.
        push_ms(&mut a, 60);
        a.emit_ready(&mut out, 0).unwrap();
        push_ms(&mut a, 60); // leftover accum the resync must drop
        out.clear();

        a.resync(&mut out, 5000).unwrap();

        // Flushed the terminal FIFO, re-armed the drain notify, and queued the
        // silence cushion (Store+Load+Queue) so playback resumes without a gap.
        assert_eq!(count(&out, b"A;Flush;C=2"), 1, "flushed the channel");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "re-armed the drain notify");
        assert_eq!(count(&out, b"A;Queue;C=2"), 1, "re-primed a silence cushion");
        // The Flush must precede the re-primed cushion (don't queue then flush it).
        let flush_at = out.windows(11).position(|w| w == b"A;Flush;C=2").unwrap();
        let queue_at = out.windows(11).position(|w| w == b"A;Queue;C=2").unwrap();
        assert!(flush_at < queue_at, "flush before the re-primed clip");
        // Re-anchored the timeline; own backlog cleared.
        assert_eq!(a.play_start_ms, 5000, "re-anchored to resync time");
        assert!(a.accum.is_empty(), "dropped the pending backlog");
        // emitted_ms reflects only the re-primed cushion, not the old timeline.
        assert_eq!(a.emitted_ms, REPRIME_SILENCE_MS as u64, "emitted == cushion");
    }

    #[test]
    fn resync_before_prime_is_a_noop() {
        // Never primed (no clip queued): there is no terminal tail to flush.
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.resync(&mut out, 1000).unwrap();
        assert!(out.is_empty(), "no I/O before the stream is primed");
    }

    #[test]
    fn stop_flushes_channel() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.stop(&mut out).unwrap();
        assert_eq!(count(&out, b"A;Flush;C=2"), 1, "flushed the channel on exit");
    }
}
