#!/usr/bin/env python3
"""Latency matrix (design spec 4.4.3): run a real two-door netplay session
through a delay-injecting TCP proxy at several RTT/jitter cells and assert
zero desync in every cell. Input frames ride TCP (reliable ordered), so
packet loss shows up as added latency, not missing input — loss cells are
covered by the jitter ones.

Also records the negotiated input delay D per cell (read back from the
session replay header), i.e. the measured input lag in frames.

Usage: python3 tools/latency_matrix.py   (takes a couple of minutes)
"""
import asyncio, os, pty, select, shutil, subprocess, sys, threading, time, random, glob

BASE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RELAY_PORT = 9931
PROXY_PORT = 9932

CELLS = [
    (0, 0),     # LAN
    (50, 10),   # same-coast internet
    (150, 30),  # cross-country BBS-typical
    (300, 30),  # intercontinental / bad night
]


def run_proxy(rtt_ms, jitter_ms, stop_event):
    """TCP proxy adding rtt/2 (+/- jitter/2) delay per direction."""
    async def pipe(reader, writer, one_way_ms, jitter):
        try:
            while True:
                data = await reader.read(4096)
                if not data:
                    break
                delay = max(0.0, one_way_ms + random.uniform(-jitter / 2, jitter / 2)) / 1000
                await asyncio.sleep(delay)
                writer.write(data)
                await writer.drain()
        except (ConnectionError, asyncio.CancelledError):
            pass
        finally:
            try:
                writer.close()
            except Exception:
                pass

    async def handle(client_r, client_w):
        up_r, up_w = await asyncio.open_connection("127.0.0.1", RELAY_PORT)
        await asyncio.gather(
            pipe(client_r, up_w, rtt_ms / 2, jitter_ms),
            pipe(up_r, client_w, rtt_ms / 2, jitter_ms),
        )

    async def main():
        server = await asyncio.start_server(handle, "127.0.0.1", PROXY_PORT)
        async with server:
            while not stop_event.is_set():
                await asyncio.sleep(0.1)

    # Deterministic jitter per cell for reproducible runs.
    random.seed(rtt_ms * 1000 + jitter_ms)
    asyncio.run(main())


class Door:
    def __init__(self, name):
        self.master, slave = pty.openpty()
        self.buf = bytearray()
        self.proc = subprocess.Popen(
            [f"{BASE}/target/release/lamegear", "--roms", f"{BASE}/roms",
             "--user", name, "--handle", name,
             "--link", f"127.0.0.1:{PROXY_PORT}", "--fps", "10"],
            stdin=slave, stdout=slave, stderr=subprocess.DEVNULL,
            cwd=BASE, close_fds=True)
        os.close(slave)

    def send(self, s):
        os.write(self.master, s.encode() if isinstance(s, str) else s)


def run_cell(rtt, jitter):
    shutil.rmtree(f"{BASE}/roms/.replays", ignore_errors=True)
    for f in glob.glob(os.path.expanduser("~/.config/lamegear/config-lat*")):
        os.unlink(f)
    relay = subprocess.Popen([f"{BASE}/link-server/gg-link-server", str(RELAY_PORT)],
                             stderr=subprocess.DEVNULL)
    stop = threading.Event()
    proxy = threading.Thread(target=run_proxy, args=(rtt, jitter, stop), daemon=True)
    proxy.start()
    time.sleep(0.6)

    A, B = Door("latA"), Door("latB")

    def pump(dur):
        end = time.time() + dur
        while time.time() < end:
            for d in (A, B):
                r, _, _ = select.select([d.master], [], [], 0.01)
                if d.master in r:
                    try:
                        data = os.read(d.master, 65536)
                    except OSError:
                        continue
                    d.buf.extend(data)
                    if b"\x1b[6n" in data:
                        d.send(b"\x1b[30;100R")

    def to_lobby(d):
        for _ in range(9):
            d.buf.clear(); d.send("\t"); pump(0.55)
            if b"L I N K   L O B B Y" in bytes(d.buf):
                return True
        return False

    pump(1.5); A.send(" "); B.send(" "); pump(1.2)
    A.send("\t"); pump(0.5)
    for ch in "pong":
        A.send(ch); pump(0.12)
    pump(0.4); A.send("\r"); pump(2.0); A.send("q"); pump(1.2)
    ok = to_lobby(A) and to_lobby(B)
    A.buf.clear(); B.buf.clear()
    A.send("\r"); pump(1.5 + rtt / 250); B.send("a")
    pump(10.0 + rtt / 100)          # link + handshake + play
    for _ in range(10):
        A.send("\x1b[C"); pump(0.12)
    pump(8.0)                        # several CRC windows
    out = bytes(A.buf) + bytes(B.buf)
    linked = out.count(b"\xdf") > 400
    desync = b"DESYNC" in out
    A.send("q"); pump(2.0); A.send("q"); pump(0.6)
    B.send("q"); pump(0.6); B.send("q"); pump(0.6)
    for d in (A, B):
        try:
            d.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            d.proc.kill()
    stop.set(); relay.kill()
    time.sleep(0.3)

    delay = "?"
    replays = glob.glob(f"{BASE}/roms/.replays/*.lgr")
    if replays:
        header = open(replays[0]).readline()
        for tok in header.split():
            if tok.startswith("delay="):
                delay = tok.split("=")[1]
    return ok and linked, desync, delay


def main():
    print(f"{'rtt(ms)':>8} {'jitter':>7} {'linked':>7} {'desync':>7} {'D(frames)':>10}")
    failures = 0
    for rtt, jitter in CELLS:
        linked, desync, delay = run_cell(rtt, jitter)
        status_ok = linked and not desync
        if not status_ok:
            failures += 1
        print(f"{rtt:>8} {jitter:>7} {str(linked):>7} {str(desync):>7} {delay:>10}"
              + ("" if status_ok else "   <-- FAIL"))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
