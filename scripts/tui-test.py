#!/usr/bin/env python3
# tui-test.py <binary> <args...>: drive the terminal UI in a 200x50 pty through a small VT screen model:
# open a daemon, drag the summary/rows border with SGR mouse sequences, press +/-, click a row, scroll; exit 1 on a failed check.
import os, pty, re, select, sys, time, signal, fcntl, termios, struct
ROWS, COLS = 50, 200
argv = sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(argv[0], argv)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

class Screen:
    def __init__(s):
        s.g = [[" "] * COLS for _ in range(ROWS)]; s.y = 0; s.x = 0; s.raw = b""
    def feed(s, b):
        s.raw += b
        t = b.decode(errors="replace"); i = 0
        while i < len(t):
            c = t[i]
            if c == "\x1b":
                m = re.match(r"\x1b\[([0-9;?<]*)([a-zA-Z@])", t[i:])
                if m:
                    params, fin = m.group(1), m.group(2); i += m.end()
                    if fin == "H" and not params.startswith("?"):
                        p = [int(v) if v else 1 for v in params.split(";")] if params else [1, 1]
                        s.y = min(max(p[0] - 1, 0), ROWS - 1); s.x = min(max((p[1] if len(p) > 1 else 1) - 1, 0), COLS - 1)
                    elif fin == "J":
                        if params in ("2", "3", ""):
                            s.g = [[" "] * COLS for _ in range(ROWS)]
                    elif fin == "K":
                        for x in range(s.x, COLS): s.g[s.y][x] = " "
                    continue
                m = re.match(r"\x1b[()][A-Z0-9]|\x1b[=>78]", t[i:])
                if m: i += m.end(); continue
                i += 1; continue
            if c == "\r": s.x = 0
            elif c == "\n": s.y = min(s.y + 1, ROWS - 1)
            elif c == "\b": s.x = max(s.x - 1, 0)
            elif c >= " ":
                s.g[s.y][s.x] = c
                if s.x < COLS - 1: s.x += 1
            i += 1
    def line(s, y): return "".join(s.g[y]).rstrip()
    def corners(s, ch="┌"): return [y for y in range(ROWS) if s.g[y][0] == ch]
    def selected(s):
        return [y for y in range(ROWS) if s.g[y][1] == ">" and s.g[y][2] == " "]

scr = Screen()
def step(data, wait):
    if data: os.write(fd, data)
    end = time.time() + wait
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: scr.feed(os.read(fd, 65536))
            except OSError: break
def press(c, r): return f"\x1b[<0;{c};{r}M".encode()
def drag(c, r): return f"\x1b[<32;{c};{r}M".encode()
def release(c, r): return f"\x1b[<0;{c};{r}m".encode()
def wheel_down(c, r): return f"\x1b[<65;{c};{r}M".encode()

ok = True
def check(name, cond, detail=""):
    global ok
    print(("ok  " if cond else "FAIL"), name, detail)
    ok = ok and cond

step(b"", 2.5)
check("node screen", "ceph-memtop" in scr.line(0), scr.line(0)[:60])
step(b"\r", 1.5)
tops = scr.corners()
check("daemon screen has two boxes", len(tops) == 2, str(tops) + " " + scr.line(0)[:40])
T = tops[1]                                   # 0-based row of the table's top border
natural = T
# grab the table's top border (1-based row T+1) and pull it down by 6 rows
step(press(10, T + 1), 0.2)
step(drag(10, T + 4), 0.3)
step(drag(10, T + 7), 0.5)
step(release(10, T + 7), 0.3)
tops = scr.corners()
check("drag down moves the table top by 6", tops == [0, T + 6], f"{tops} expected [0, {T + 6}]")
check("summary bottom border moved with it", scr.corners("└")[0] == T + 5, str(scr.corners("└")))
step(b"-", 0.5)
check("- shrinks the summary by one", scr.corners() == [0, T + 5], str(scr.corners()))
step(b"+", 0.5)
check("+ grows it again", scr.corners() == [0, T + 6], str(scr.corners()))
# click the fourth row of the table: 0-based y = (T+6) + 2 + 3, 1-based +1
step(press(20, T + 6 + 2 + 3 + 1), 0.2); step(release(20, T + 6 + 2 + 3 + 1), 0.4)
check("click selects row 3", scr.selected() == [T + 6 + 2 + 3], str(scr.selected()))
step(wheel_down(20, T + 12), 0.5)
check("wheel moves to row 4", scr.selected() == [T + 6 + 2 + 4], str(scr.selected()))
# grab the summary's own bottom border (0-based T+5) and push it far up: clamps at 3 rows
step(press(10, T + 5 + 1), 0.2); step(drag(10, 1), 0.5); step(release(10, 1), 0.3)
check("drag up clamps the summary at 3 rows", scr.corners() == [0, 3], str(scr.corners()))
step(b"\x1b", 0.8)
check("Esc back to the node screen", len(scr.corners()) == 3, str(scr.corners()))
step(b"\r", 0.8)
check("the split is kept when reopening", scr.corners() == [0, 3], str(scr.corners()))
# the four tuner caches are drawn in cyan: open an OSD (the daemons after the current one)
step(b"\x1b", 0.5)
found = False
for _ in range(8):
    step(b"j", 0.2); step(b"\r", 0.8)
    if "osd." in scr.line(0):
        found = True; break
    step(b"\x1b", 0.5)
check("an OSD screen opened", found, scr.line(0)[:40])
i = scr.raw.rfind(b"data cache (BlueStore")   # the tree leaf of cache:data
before = scr.raw[max(0, i - 96):i] if i > 0 else b""
# crossterm writes the colour as "\x1b[36m", "\x1b[96m" or "\x1b[38;5;6m", possibly with more SGR parameters around it
cyan = re.compile(rb"\x1b\[(?:[0-9;]*;)?(?:36|96|38;5;6)(?:;[0-9;]*)?m")
check("the cache rows are drawn in cyan", i > 0 and cyan.search(before) is not None, repr(before[-60:]))
print("---- screen after the last drag ----")
for y in range(0, min(ROWS, 14)): print(f"{y:2} {scr.line(y)[:120]}")
step(b"q", 1.0)
try: os.kill(pid, signal.SIGTERM)
except ProcessLookupError: pass
_, status = os.waitpid(pid, 0)
raw = scr.raw
check("mouse reporting was enabled", b"\x1b[?1000h" in raw)
check("mouse reporting was disabled on exit", b"\x1b[?1000l" in raw and raw.rfind(b"\x1b[?1000l") > raw.rfind(b"\x1b[?1000h"))
check("exit status 0", status == 0, str(status))
check("no panic", b"panicked" not in raw)
print("RESULT", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
