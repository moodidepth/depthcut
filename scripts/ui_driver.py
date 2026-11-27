"""Drive a running Depthcut window with real mouse / keyboard input and check
the results through the app's JSON state dump, its project file, its log and
ffprobe. Windows only; needs Pillow for screenshots.

The app must run with the state dump on:

    set DEPTHCUT_STATE_DUMP=%TEMP%\\depthcut-state.json   (or --state-dump=<file>)

Usage (several steps per call; each prints one line, a failed check stops):

    python ui_driver.py [--pid N | --launch EXE] [--dump FILE] [--shots DIR] STEP...

Steps (arguments separated by spaces; quote a step that holds spaces):

    front                      bring the window forward (maximised)
    key CHORD                  press a key chord: ctrl+i, shift+z, space, shift+delete
    type TEXT                  type text (unicode, layout-independent)
    click TARGET [right|double|shift|ctrl]
                               TARGET is a rect name from the dump (panel:viewer,
                               clip:5, asset:1, viewer:timecode), text=LABEL (any
                               button / menu item / field by its label or value;
                               text=LABEL#2 for the second, text=LABEL@RECT only
                               inside that rect), clip#N (the Nth
                               storyline clip), asset=NAME, tl=SECONDS (timeline
                               storyline row at that time), or X,Y in points;
                               NAME@FX,FY is a point inside a rect (0..1 each)
    drag FROM TO               press on FROM, move, release on TO (same targets)
    hover TARGET
    seek SECONDS               type the playhead position into the viewer timecode
    import PATH                Ctrl+I, type the path into the file dialog
    save [PATH]                Ctrl+S (Save As into PATH when given), then 'saved'
    wait EXPR [TIMEOUT]        poll the state until EXPR is true (s = state dict)
    expect EXPR                fail unless EXPR is true now
    sleep SECONDS
    state [KEY.PATH]           print the state (or a part of it) as JSON
    clips                      print the timeline clips, one per line
    widgets [FILTER|RECT]      list labelled widgets (role, label, value, rect)
    shot NAME [MAXW]           screenshot, cropped to a rect name (or 'window'),
                               scaled to MAXW px wide (default 900)
    log [N] [TEXT]             print the last N new log lines (default 15), only those with TEXT
    probe FILE                 ffprobe summary of a rendered file
    loudness FILE              integrated loudness (ffmpeg ebur128)
    saved                      compare the saved project file with the dump

After every input step the driver waits for a fresh dump and prints what
changed (status message, clip count, playhead, dialogs). New log lines with
WARN / ERROR are printed at the end of the call.
"""

from __future__ import annotations

import ctypes
import ctypes.wintypes as wt
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32")
try:
    user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # per-monitor v2: real pixels
except Exception:
    pass

DEFAULT_DUMP = Path(tempfile.gettempdir()) / "depthcut-state.json"


# ----------------------------------------------------------------- win32 input

class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wt.LONG), ("dy", wt.LONG), ("mouseData", wt.DWORD), ("dwFlags", wt.DWORD), ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wt.WORD), ("wScan", wt.WORD), ("dwFlags", wt.DWORD), ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


class _U(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("pad", ctypes.c_byte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wt.DWORD), ("u", _U)]


KEYUP, UNICODE, EXTENDED = 0x2, 0x4, 0x1
MOUSE = {"left": (0x2, 0x4), "right": (0x8, 0x10), "middle": (0x20, 0x40)}

VK = {
    "ctrl": 0x11, "shift": 0x10, "alt": 0x12, "win": 0x5B,
    "space": 0x20, "enter": 0x0D, "return": 0x0D, "esc": 0x1B, "escape": 0x1B, "tab": 0x09,
    "backspace": 0x08, "delete": 0x2E, "del": 0x2E, "insert": 0x2D, "home": 0x24, "end": 0x23,
    "pageup": 0x21, "pagedown": 0x22, "left": 0x25, "up": 0x26, "right": 0x27, "down": 0x28,
    "minus": 0xBD, "-": 0xBD, "equals": 0xBB, "=": 0xBB, "plus": 0xBB, "comma": 0xBC, ",": 0xBC,
    "period": 0xBE, ".": 0xBE, "slash": 0xBF, "/": 0xBF, "backtick": 0xC0, "`": 0xC0,
    "[": 0xDB, "]": 0xDD, "semicolon": 0xBA, ";": 0xBA, "quote": 0xDE, "backslash": 0xDC,
}
VK.update({chr(c).lower(): c for c in range(ord("A"), ord("Z") + 1)})
VK.update({str(d): 0x30 + d for d in range(10)})
VK.update({f"f{n}": 0x6F + n for n in range(1, 13)})
EXTENDED_KEYS = {0x2E, 0x2D, 0x24, 0x23, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28}


def _send(*inputs: INPUT) -> None:
    arr = (INPUT * len(inputs))(*inputs)
    user32.SendInput(len(inputs), arr, ctypes.sizeof(INPUT))


def _key(vk: int, up: bool) -> INPUT:
    scan = user32.MapVirtualKeyW(vk, 0)
    flags = (KEYUP if up else 0) | (EXTENDED if vk in EXTENDED_KEYS else 0)
    i = INPUT(type=1)
    i.u.ki = KEYBDINPUT(vk, scan, flags, 0, 0)
    return i


def press_chord(chord: str) -> None:
    parts = [p for p in chord.lower().split("+") if p] if chord != "+" else ["plus"]
    vks = []
    for p in parts:
        if p not in VK:
            raise ValueError(f"unknown key {p!r} in {chord!r}")
        vks.append(VK[p])
    for vk in vks:
        _send(_key(vk, False))
        time.sleep(0.02)
    time.sleep(0.04)
    for vk in reversed(vks):
        _send(_key(vk, True))
        time.sleep(0.02)


def type_text(text: str) -> None:
    for ch in text:
        down, up = INPUT(type=1), INPUT(type=1)
        down.u.ki = KEYBDINPUT(0, ord(ch), UNICODE, 0, 0)
        up.u.ki = KEYBDINPUT(0, ord(ch), UNICODE | KEYUP, 0, 0)
        _send(down, up)
        time.sleep(0.015)


def move_to(x: int, y: int) -> None:
    user32.SetCursorPos(int(x), int(y))
    time.sleep(0.03)


def mouse(button: str, up: bool) -> None:
    i = INPUT(type=0)
    i.u.mi = MOUSEINPUT(0, 0, 0, MOUSE[button][1 if up else 0], 0, 0)
    _send(i)


# ----------------------------------------------------------------- the window

# steps that send keyboard / mouse input (to the foreground window)
INPUT_STEPS = {"key", "type", "hover", "click", "drag", "seek", "import", "save"}


def find_window(pid: int) -> int:
    found = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def cb(hwnd, _):
        p = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
        if p.value == pid and user32.IsWindowVisible(hwnd) and user32.GetWindowTextLengthW(hwnd) > 0:
            found.append(hwnd)
        return True

    user32.EnumWindows(cb, 0)
    if not found:
        raise RuntimeError(f"no visible window for pid {pid}")
    return found[0]


def client_origin(hwnd: int) -> tuple[int, int]:
    pt = wt.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    return pt.x, pt.y


def window_pid(hwnd: int) -> int:
    p = wt.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
    return p.value


def bring_front(hwnd: int, maximize: bool = True) -> None:
    # Windows refuses SetForegroundWindow from a background process unless it
    # shares the foreground thread's input queue; a tapped Alt helps too.
    fg = user32.GetForegroundWindow()
    fg_thread = user32.GetWindowThreadProcessId(fg, None) if fg else 0
    me = ctypes.windll.kernel32.GetCurrentThreadId()
    attached = bool(fg_thread) and fg_thread != me and user32.AttachThreadInput(me, fg_thread, True)
    try:
        _send(_key(0x12, False), _key(0x12, True))
        user32.ShowWindow(hwnd, 3 if maximize else 9)
        user32.BringWindowToTop(hwnd)
        user32.SetForegroundWindow(hwnd)
    finally:
        if attached:
            user32.AttachThreadInput(me, fg_thread, False)
    time.sleep(0.4)


def capture(hwnd: int):
    """The window's client area as a PIL image (works while covered)."""
    from PIL import Image

    r = wt.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(r))
    w, h = r.right, r.bottom
    wdc = user32.GetDC(hwnd)
    mdc = gdi32.CreateCompatibleDC(wdc)
    bmp = gdi32.CreateCompatibleBitmap(wdc, w, h)
    gdi32.SelectObject(mdc, bmp)
    user32.PrintWindow(hwnd, mdc, 0x1 | 0x2)  # client only, full content (GPU surfaces)

    class BMI(ctypes.Structure):
        _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG), ("biPlanes", wt.WORD), ("biBitCount", wt.WORD),
                    ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD), ("x", wt.LONG), ("y", wt.LONG), ("c1", wt.DWORD), ("c2", wt.DWORD)]

    bmi = BMI(ctypes.sizeof(BMI), w, -h, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mdc, bmp, 0, h, buf, ctypes.byref(bmi), 0)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mdc)
    user32.ReleaseDC(hwnd, wdc)
    return Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1).convert("RGB")


# ----------------------------------------------------------------- the driver

class StepFailed(Exception):
    pass


class Dot(dict):
    """A dict whose keys also read as attributes: s.ui.dialogs."""

    def __getattr__(self, k):
        try:
            return self[k]
        except KeyError:
            raise AttributeError(k) from None

    @staticmethod
    def wrap(v):
        if isinstance(v, dict):
            return Dot({k: Dot.wrap(x) for k, x in v.items()})
        if isinstance(v, list):
            return [Dot.wrap(x) for x in v]
        return v


class Driver:
    def __init__(self, pid: int | None, dump: Path, shots: Path, launch: str | None = None, open_file: str | None = None):
        self.dump = dump
        self.shots = shots
        if launch:
            env = dict(os.environ, DEPTHCUT_STATE_DUMP=str(dump))
            dump.unlink(missing_ok=True)
            exe = Path(launch).resolve()  # cwd below would otherwise re-anchor a relative path
            pid = subprocess.Popen([str(exe)] + ([str(Path(open_file).resolve())] if open_file else []), env=env, cwd=str(exe.parent)).pid
            self._wait_file(30)
        elif pid is None:
            pid = self._read_raw().get("app", {}).get("pid")
        self.pid = pid
        self.hwnd = find_window(pid)
        self.s = self.fresh(30.0)  # a fresh launch can take a while to draw its first frames
        log = self.s["app"].get("log")
        self.log_path = Path(log) if log else None
        self.log_start = self.log_path.stat().st_size if self.log_path and self.log_path.exists() else 0

    # --- state
    def _wait_file(self, timeout: float) -> None:
        end = time.time() + timeout
        while not self.dump.exists():
            if time.time() > end:
                raise RuntimeError(f"no state dump at {self.dump} (is DEPTHCUT_STATE_DUMP set for the app?)")
            time.sleep(0.2)

    def _read_raw(self) -> dict:
        for _ in range(20):
            try:
                return json.loads(self.dump.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                time.sleep(0.05)
        raise RuntimeError(f"cannot read {self.dump}")

    def _roundtrip(self, timeout: float = 5.0) -> dict:
        nonce = uuid.uuid4().hex[:12]
        req = Path(str(self.dump) + ".req")
        # replace atomically: the app must never read a truncated, empty file
        tmp = Path(str(req) + ".tmp")
        tmp.write_text(nonce, encoding="utf-8")
        os.replace(tmp, req)
        end = time.time() + timeout
        while time.time() < end:
            s = self._read_raw()
            if s.get("request") == nonce:
                req.unlink(missing_ok=True)  # the requester owns the file
                return s
            time.sleep(0.03)
        req.unlink(missing_ok=True)
        raise RuntimeError("the app did not answer a state request (hung, or the dump is off?)")

    def fresh(self, timeout: float = 5.0) -> dict:
        """A dump from a frame that began after every input sent so far."""
        self._roundtrip(timeout)
        self.s = self._roundtrip(timeout)
        return self.s

    # --- geometry
    def rect(self, name: str):
        """A named rect: a panel/clip rect from the dump, 'window:TITLE' for a
        floating window (dialog) by its title, or 'X,Y,W,H' in points."""
        if name in self.s["rects"]:
            return self.s["rects"][name]
        if name.startswith("window:"):
            title = name[7:].lower()
            for w in self.s.get("widgets", []):
                if w[0] == "Window" and w[1].lower() == title:
                    return w[3]
        if m := re.fullmatch(r"([\d.]+),([\d.]+),([\d.]+),([\d.]+)", name):
            return [float(v) for v in m.groups()]
        return None

    def point(self, target: str) -> tuple[int, int]:
        """Screen pixel for a target (see the module doc)."""
        s = self.s
        ppp = s["window"]["pixels_per_point"]
        ox, oy = client_origin(self.hwnd)
        rects = s["rects"]

        def center(r):
            return r[0] + r[2] / 2, r[1] + r[3] / 2

        anchor = None
        if "@" in target and not target.startswith("text="):  # name@fx,fy : a fraction inside the rect
            target, anchor = target.split("@", 1)
        if m := re.fullmatch(r"(-?[\d.]+),(-?[\d.]+)", target):
            x, y = float(m[1]), float(m[2])
        elif m := re.fullmatch(r"tl=([\d.]+)", target):
            lanes = rects["timeline:lanes"]
            v = s["timeline_view"]
            x = lanes[0] + (float(m[1]) - v["scroll_s"]) * v["pps"]
            spine = [c for c in s["timeline"]["clips"] if c["lane"] == 0 and f"clip:{c['id']}" in rects]
            y = center(rects[f"clip:{spine[0]['id']}"])[1] if spine else lanes[1] + lanes[3] * 0.35
        else:
            if m := re.fullmatch(r"clip#(\d+)", target):
                spine = [c for c in s["timeline"]["clips"] if c["lane"] == 0]
                target = f"clip:{spine[int(m[1])]['id']}"
            elif target.startswith("text="):
                # a menu that just opened is laid out off-screen for a frame:
                # wait until the widget stops moving
                w = self.find_widget(target[5:])
                for _ in range(10):
                    time.sleep(0.05)
                    self.fresh()
                    again = self.find_widget(target[5:])
                    if again[3] == w[3]:
                        break
                    w = again
                s, rects = self.s, self.s["rects"]
                target = "_w"
                rects = dict(rects, _w=w[3])
            elif target.startswith("asset="):
                name = target[6:].lower()
                hits = [a for a in s["assets"] if name in a["name"].lower()]
                if not hits:
                    raise StepFailed(f"no asset matching {name!r}")
                target = f"asset:{hits[0]['id']}"
            r = rects.get(target) or self.rect(target)
            if r is None:
                raise StepFailed(f"{target!r} is not on screen (rects: {', '.join(sorted(rects)[:30])})")
            if anchor:
                fx, fy = (float(v) for v in anchor.split(","))
                x, y = r[0] + r[2] * fx, r[1] + r[3] * fy
            else:
                x, y = center(r)
        return round(ox + x * ppp), round(oy + y * ppp)

    def find_widget(self, spec: str) -> list:
        """A widget by its label or value: exact (ignoring case) first, then
        substring; 'Label#2' takes the second match from the top,
        'Label@panel:inspector' only looks inside that rect."""
        nth, inside = 0, None
        if "@" in spec:
            spec, name = spec.rsplit("@", 1)
            inside = self.rect(name)
            if inside is None:
                raise StepFailed(f"no rect {name!r}")
        if m := re.fullmatch(r"(.*)#(\d+)", spec):
            spec, nth = m[1], int(m[2]) - 1
        want = spec.lower()

        def within(r):
            if inside is None:
                return True
            cx, cy = r[0] + r[2] / 2, r[1] + r[3] / 2
            return inside[0] <= cx <= inside[0] + inside[2] and inside[1] <= cy <= inside[1] + inside[3]

        ws = [w for w in self.s.get("widgets", []) if "off" not in w[4].split() and within(w[3])]
        hits = [w for w in ws if want in (w[1].lower(), w[2].lower())] or [w for w in ws if want in w[1].lower() or want in w[2].lower()]
        if len(hits) <= nth:
            near = sorted({w[1] or w[2] for w in ws if (w[1] or w[2])})[:40]
            raise StepFailed(f"no enabled widget {spec!r} (#{nth + 1}); on screen: {near}")
        return hits[nth]

    # --- reporting
    def summary(self, before: dict | None) -> str:
        s = self.s
        bits = []
        st = s.get("status")
        if st and (before is None or st != before.get("status") or (s.get("status_age_ms") or 1e9) < 1500):
            bits.append(f"status: {st}")
        n = len(s["timeline"]["clips"])
        if before is None or n != len(before["timeline"]["clips"]) or s["timeline"]["duration_s"] != before["timeline"]["duration_s"]:
            bits.append(f"clips {n}, {s['timeline']['duration_s']:.3f}s")
        if before is None or s["playhead_s"] != before["playhead_s"]:
            bits.append(f"playhead {s['playhead_s']:.3f}")
        if before is None or s["selection"] != before["selection"]:
            bits.append(f"sel {s['selection']}")
        if before is None or s["ui"]["dialogs"] != before["ui"]["dialogs"]:
            bits.append(f"dialogs {s['ui']['dialogs']}")
        if s["ui"]["menu_open"] or s["ui"]["popup_open"]:
            bits.append("menu/popup open")
        if before is not None and s["history"]["undo"] != before["history"]["undo"]:
            bits.append(f"undo: {s['history']['undo_label']}")
        return "; ".join(bits) or "no change"

    def new_log_lines(self) -> list[str]:
        if not self.log_path or not self.log_path.exists():
            return []
        with open(self.log_path, "rb") as f:
            f.seek(self.log_start)
            text = f.read().decode("utf-8", "replace")
        return text.splitlines()

    # --- steps
    def run(self, steps: list[str]) -> bool:
        ok = True
        for raw in steps:
            name, _, tail = raw.strip().partition(" ")
            if not name:
                continue
            if name in ("expect", "wait"):
                # expressions keep their quotes; wait takes an optional trailing timeout
                m = re.fullmatch(r"(.*?)\s+(\d+(?:\.\d+)?)", tail.strip()) if name == "wait" else None
                rest = [m[1], m[2]] if m else [tail.strip()]
            else:
                rest = shlex.split(tail, posix=True)
            before = self.s
            try:
                if name in INPUT_STEPS:
                    self.ensure_front()
                out = getattr(self, "do_" + name.replace("-", "_"))(*rest)
            except StepFailed as e:
                print(f"FAIL {raw}: {e}")
                ok = False
                break
            except AttributeError as e:
                if not hasattr(self, "do_" + name.replace("-", "_")):
                    print(f"FAIL unknown step {name!r}")
                    ok = False
                    break
                raise
            if out is None:
                out = self.summary(before)
            print(f"ok   {raw}: {out}")
        bad = [l for l in self.new_log_lines() if " WARN " in l or " ERROR " in l or "panicked" in l]
        for l in bad[-12:]:
            print("LOG  " + l)
        return ok

    def _after_input(self, settle: float = 0.15) -> None:
        time.sleep(settle)
        self.fresh()

    def do_front(self):
        bring_front(self.hwnd)
        self._after_input(0.3)

    def ensure_front(self) -> None:
        """Keys and clicks go to whatever window is in front: never send them
        unless that is the app under test (another Depthcut may be open)."""
        for _ in range(3):
            fg = user32.GetForegroundWindow()
            if fg and window_pid(fg) == self.pid:
                return
            bring_front(self.hwnd)
        fg = user32.GetForegroundWindow()
        raise StepFailed(f"pid {self.pid} is not the foreground window (pid {window_pid(fg) if fg else None} is); no input sent")

    def do_key(self, chord, times="1"):
        for _ in range(int(times)):
            press_chord(chord)
            time.sleep(0.05)
        self._after_input()

    def do_type(self, *text):
        type_text(" ".join(text))
        self._after_input()

    def do_hover(self, target):
        move_to(*self.point(target))
        self._after_input()

    def do_click(self, target, *mods):
        if bad := [m for m in mods if m not in ("shift", "ctrl", "alt", "right", "double")]:
            raise StepFailed(f"unknown click modifier {bad[0]!r}; quote a target with spaces: click 'text={target[5:]} {' '.join(bad)}'")
        x, y = self.point(target)
        button = "right" if "right" in mods else "left"
        held = [VK[m] for m in mods if m in ("shift", "ctrl", "alt")]
        move_to(x, y)
        time.sleep(0.05)  # let the app see the hover before the press
        for vk in held:
            _send(_key(vk, False))
        for _ in range(2 if "double" in mods else 1):
            mouse(button, False)
            time.sleep(0.04)
            mouse(button, True)
            time.sleep(0.06)
        for vk in held:
            _send(_key(vk, True))
        self._after_input(0.2)

    def do_drag(self, src, dst, steps="20"):
        x0, y0 = self.point(src)
        x1, y1 = self.point(dst)
        move_to(x0, y0)
        mouse("left", False)
        time.sleep(0.08)
        n = int(steps)
        for i in range(1, n + 1):
            move_to(x0 + (x1 - x0) * i / n, y0 + (y1 - y0) * i / n)
            time.sleep(0.015)
        time.sleep(0.1)
        mouse("left", True)
        self._after_input(0.3)

    def do_seek(self, seconds):
        fps = self.s["format"]["fps"]
        nominal = round(fps)
        frames = round(float(seconds) * fps)
        total_s, ff = divmod(frames, nominal)
        hh, rem = divmod(total_s, 3600)
        mm, ss = divmod(rem, 60)
        self.do_click("viewer:timecode")
        type_text(f"{hh:02}:{mm:02}:{ss:02}:{ff:02}")
        press_chord("enter")
        self._after_input()
        got = self.s["playhead_s"]
        if abs(got - float(seconds)) > 1.0 / fps + 1e-6:
            raise StepFailed(f"playhead is {got}, wanted {seconds}")
        return f"playhead {got:.3f}"

    def do_import(self, path):
        n = len(self.s["assets"])
        press_chord("ctrl+i")
        time.sleep(1.2)  # the native file dialog
        type_text(str(Path(path).resolve()))
        time.sleep(0.2)
        press_chord("enter")
        self.do_wait(f"len(s['assets']) > {n} and s['jobs']['importing'] == 0", "60")
        a = self.s["assets"][-1]
        v = a.get("video") or {}
        return f"asset {a['id']} {a['name']} {a['duration_s']:.2f}s {v.get('width')}x{v.get('height')}@{v.get('fps')}"

    def do_save(self, path=None):
        """Ctrl+S; with PATH, also fill in the native Save dialog (which
        blocks the app, so no state is read until it closes)."""
        press_chord("ctrl+shift+s" if path and self.s["app"]["project_path"] else "ctrl+s")
        if path:
            time.sleep(1.2)
            type_text(str(Path(path).resolve()))
            time.sleep(0.2)
            press_chord("enter")
            time.sleep(0.8)
            if Path(path).exists():  # "replace it?" confirmation, if the file existed
                press_chord("alt+y")
        self.fresh(30.0)
        if path and Path(self.s["app"]["project_path"] or "").resolve() != Path(path).resolve():
            raise StepFailed(f"project path is {self.s['app']['project_path']!r}")
        return self.do_saved()

    def do_wait(self, expr, timeout="20"):
        end = time.time() + float(timeout)
        while True:
            self.fresh()
            if self._eval(expr):
                return f"true after {float(timeout) - (end - time.time()):.1f}s"
            if time.time() > end:
                raise StepFailed(f"still false after {timeout}s: {expr}")
            time.sleep(0.25)

    def do_expect(self, *expr):
        e = " ".join(expr)
        self.fresh()
        if not self._eval(e):
            raise StepFailed(f"false: {e}  [status: {self.s.get('status')!r}; clips {len(self.s['timeline']['clips'])}; playhead {self.s['playhead_s']}; dialogs {self.s['ui']['dialogs']}]")
        return "true"

    def _eval(self, expr):
        s = Dot.wrap(self.s)
        clips = s["timeline"]["clips"]
        spine = [c for c in clips if c["lane"] == 0]
        try:
            return eval(expr, {"abs": abs, "len": len, "any": any, "all": all, "round": round, "min": min, "max": max},
                        {"s": s, "clips": clips, "spine": spine, "markers": s["timeline"]["markers"], "status": s.get("status") or ""})
        except Exception as e:
            raise StepFailed(f"cannot evaluate {expr!r}: {e}")

    def do_sleep(self, seconds):
        time.sleep(float(seconds))
        self.fresh()

    def do_state(self, path=""):
        self.fresh()
        v = self.s
        for k in [p for p in path.split(".") if p]:
            try:
                v = v[int(k)] if isinstance(v, list) else v[k]
            except (KeyError, IndexError, ValueError, TypeError):
                have = list(v) if isinstance(v, dict) else f"{len(v)} items" if isinstance(v, list) else type(v).__name__
                raise StepFailed(f"no {k!r} in state.{path}; have: {have}")
        return json.dumps(v, indent=None if path else 1)[:4000]

    def do_widgets(self, *filt):
        """List widgets (role, label/value, rect), optionally filtered by text or inside a rect name."""
        self.fresh()
        ws = self.s.get("widgets", [])
        f = " ".join(filt)
        if f and (r := self.rect(f)) is not None:
            x, y, w, h = r
            ws = [v for v in ws if x <= v[3][0] + v[3][2] / 2 <= x + w and y <= v[3][1] + v[3][3] / 2 <= y + h]
        elif f:
            ws = [v for v in ws if f.lower() in (v[1] + " " + v[2]).lower() or f.lower() == v[0].lower()]
        lines = [f"  {v[0]:<12} {(v[1] or '')[:40]!r:<42} {(v[2] or '')[:30]!r:<32} {[round(c) for c in v[3]]} {v[4]}" for v in ws[:80]]
        return f"{len(ws)} widgets\n" + "\n".join(lines)

    def do_clips(self):
        self.fresh()
        lines = []
        for c in self.s["timeline"]["clips"]:
            k = c["kind"]
            what = k.get("text") or next((a["name"] for a in self.s["assets"] if a["id"] == k.get("asset")), k["type"])
            fx = ",".join(e["kind"].split("(")[0].split("{")[0].strip() for e in c["effects"])
            extra = []
            if fx:
                extra.append(f"fx[{fx}]")
            if c["retime"]:
                extra.append("retimed")
            if c["transition_in"]:
                extra.append(f"in:{c['transition_in']['kind']}")
            if c["transition_out"]:
                extra.append(f"out:{c['transition_out']['kind']}")
            lines.append(f"  #{c['id']} lane {c['lane']:+d} {c['start_s']:8.3f}-{c['end_s']:8.3f}  src {c['source_in_s']:.3f}-{c['source_out_s']:.3f}  {what} {' '.join(extra)}{' *' if c['selected'] else ''}")
        m = self.s["timeline"]["markers"]
        if m:
            lines.append("  markers: " + ", ".join(f"{x['time_s']:.2f} {x['kind']}{(' ' + x['name']) if x['name'] else ''}" for x in m))
        return "\n" + "\n".join(lines) if lines else "empty timeline"

    def do_shot(self, name="window", maxw="900"):
        img = capture(self.hwnd)
        if name != "window":
            r = self.rect(name)
            if r is None:
                raise StepFailed(f"no rect {name!r}")
            ppp = self.s["window"]["pixels_per_point"]
            img = img.crop((round(r[0] * ppp), round(r[1] * ppp), round((r[0] + r[2]) * ppp), round((r[1] + r[3]) * ppp)))
        mw = int(maxw)
        if img.width > mw:
            img = img.resize((mw, max(1, round(img.height * mw / img.width))))
        self.shots.mkdir(parents=True, exist_ok=True)
        out = self.shots / f"{time.strftime('%H%M%S')}-{name.replace(':', '_')}.png"
        img.save(out)
        return str(out)

    def do_log(self, *args):
        """`log [N] [TEXT]`: the last N (15) new log lines, only those containing TEXT."""
        n = next((int(a) for a in args if a.isdigit()), 15)
        needle = " ".join(a for a in args if not a.isdigit())
        lines = [l for l in self.new_log_lines() if needle in l]
        return "\n" + "\n".join("  " + l for l in lines[-n:]) if lines else "no new log lines" + (f" containing {needle!r}" if needle else "")

    def do_probe(self, path):
        p = subprocess.run(["ffprobe", "-v", "error", "-show_entries",
                            "format=duration,bit_rate:stream=codec_type,codec_name,width,height,r_frame_rate,avg_frame_rate,pix_fmt,sample_rate,channels",
                            "-of", "json", path], capture_output=True, text=True)
        if p.returncode != 0:
            raise StepFailed(p.stderr.strip()[:300])
        j = json.loads(p.stdout)
        parts = [f"{float(j['format']['duration']):.3f}s {int(j['format'].get('bit_rate', 0)) // 1000} kb/s"]
        for st in j["streams"]:
            if st["codec_type"] == "video":
                parts.append(f"video {st['codec_name']} {st['width']}x{st['height']} {st['avg_frame_rate']} {st.get('pix_fmt')}")
            elif st["codec_type"] == "audio":
                parts.append(f"audio {st['codec_name']} {st.get('sample_rate')} Hz {st.get('channels')} ch")
        return "; ".join(parts)

    def do_loudness(self, path):
        p = subprocess.run(["ffmpeg", "-hide_banner", "-nostats", "-i", path, "-af", "ebur128=peak=true", "-f", "null", "-"], capture_output=True, text=True)
        tail = p.stderr[p.stderr.rfind("Summary:"):]
        i = re.search(r"I:\s+(-?[\d.]+) LUFS", tail)
        tp = re.search(r"Peak:\s+(-?[\d.]+) dBFS", tail)
        return f"integrated {i[1] if i else '?'} LUFS, true peak {tp[1] if tp else '?'} dBFS"

    def do_saved(self):
        self.fresh()
        path = self.s["app"]["project_path"]
        if not path:
            raise StepFailed("the project has not been saved")
        proj = json.loads(Path(path).read_text(encoding="utf-8"))
        spine = proj["timeline"]["spine"]
        n_saved = sum(1 + len(c.get("connected", [])) for c in spine)
        n_live = len(self.s["timeline"]["clips"])
        fmt = proj["timeline"]["format"]
        msg = f"{path}: {n_saved} clips (live {n_live}), format {fmt.get('width')}x{fmt.get('height')}, edited={self.s['app']['edited']}"
        if n_saved != n_live and not self.s["app"]["edited"]:
            raise StepFailed("saved project differs from the live timeline: " + msg)
        return msg


def main(argv: list[str]) -> int:
    import argparse

    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pid", type=int)
    ap.add_argument("--launch", help="start this exe with the state dump on")
    ap.add_argument("--open", help="with --launch: the project file to open")
    ap.add_argument("--dump", type=Path, default=Path(os.environ.get("DEPTHCUT_STATE_DUMP", DEFAULT_DUMP)))
    ap.add_argument("--shots", type=Path, default=Path(tempfile.gettempdir()) / "depthcut-shots")
    ap.add_argument("steps", nargs="*")
    a = ap.parse_args(argv)
    d = Driver(a.pid, a.dump, a.shots, a.launch, a.open)
    print(f"pid {d.pid}; {d.summary(None)}")
    return 0 if d.run(a.steps) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
