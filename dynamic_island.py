#!/usr/bin/env python3
"""
Dynamic Island for Hyprland - a floating, spring-animated island.

Three views, cycled with a right click:
  1. music  — current track, artwork, progress and controls
  2. system — clock, date, connected headphone name and battery
  3. tray   — StatusNotifier (system tray) icons

Left click / drag down expands, drag up or tap the background collapses.
Scroll adjusts media volume. Everything is drawn with cairo on a single
transparent layer-shell surface whose input region matches the pill.
"""

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("GtkLayerShell", "0.1")
gi.require_version("GdkPixbuf", "2.0")
gi.require_version("PangoCairo", "1.0")
gi.require_version("DbusmenuGtk3", "0.4")

import json
import math
import os
import re
import subprocess
import tempfile
import threading
import time
from dataclasses import dataclass

import cairo
import dbus
import dbus.mainloop.glib
import dbus.service
import numpy as np
from gi.repository import (
    DbusmenuGtk3,
    Gdk,
    GdkPixbuf,
    Gio,
    GLib,
    Gtk,
    GtkLayerShell,
    Pango,
    PangoCairo,
)

try:
    import requests
except ImportError:
    requests = None

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)


# Simple StatusNotifierWatcher fallback

try:

    class SNIWatcher(dbus.service.Object):
        BUS_NAME = "org.kde.StatusNotifierWatcher"
        OBJECT_PATH = "/StatusNotifierWatcher"
        IFACE = "org.kde.StatusNotifierWatcher"
        PROPS_IFACE = "org.freedesktop.DBus.Properties"

        def __init__(self, bus):
            self._bus = bus
            self._bus_name = dbus.service.BusName(self.BUS_NAME, bus)
            dbus.service.Object.__init__(self, self._bus_name, self.OBJECT_PATH)
            self._items = []
            self._hosts = []

        @dbus.service.method(IFACE, in_signature="", out_signature="as")
        def RegisteredStatusNotifierItems(self):
            return self._items

        @dbus.service.method(IFACE, in_signature="", out_signature="b")
        def IsStatusNotifierHostRegistered(self):
            return True

        @dbus.service.method(IFACE, in_signature="", out_signature="i")
        def ProtocolVersion(self):
            return 0

        @dbus.service.method(IFACE, in_signature="s", out_signature="")
        def RegisterStatusNotifierItem(self, service):
            if service and service not in self._items:
                self._items.append(service)
                self.StatusNotifierItemRegistered(service)
                self.PropertiesChanged(
                    self.IFACE,
                    {
                        "RegisteredStatusNotifierItems": dbus.Array(
                            self._items, signature="s"
                        )
                    },
                    [],
                )

        @dbus.service.method(IFACE, in_signature="s", out_signature="")
        def RegisterStatusNotifierHost(self, service):
            if service and service not in self._hosts:
                self._hosts.append(service)

        @dbus.service.method(PROPS_IFACE, in_signature="ss", out_signature="v")
        def Get(self, interface_name, property_name):
            if interface_name == self.IFACE:
                if property_name == "RegisteredStatusNotifierItems":
                    return dbus.Array(self._items, signature="s")
                elif property_name == "IsStatusNotifierHostRegistered":
                    return dbus.Boolean(True)
                elif property_name == "ProtocolVersion":
                    return dbus.Int32(0)
            raise dbus.exceptions.DBusException("Property not found")

        @dbus.service.method(PROPS_IFACE, in_signature="s", out_signature="a{sv}")
        def GetAll(self, interface_name):
            if interface_name == self.IFACE:
                return {
                    "RegisteredStatusNotifierItems": dbus.Array(
                        self._items, signature="s"
                    ),
                    "IsStatusNotifierHostRegistered": dbus.Boolean(True),
                    "ProtocolVersion": dbus.Int32(0),
                }
            raise dbus.exceptions.DBusException("Interface not found")

        @dbus.service.signal(IFACE, signature="s")
        def StatusNotifierItemRegistered(self, service):
            pass

        @dbus.service.signal(IFACE, signature="s")
        def StatusNotifierItemUnregistered(self, service):
            pass

        @dbus.service.signal(PROPS_IFACE, signature="sa{sv}as")
        def PropertiesChanged(
            self, interface_name, changed_properties, invalidated_properties
        ):
            pass
except Exception:

    class SNIWatcher:
        def __init__(self, *args, **kwargs):
            pass


# Layout

WIN_W, WIN_H = 660, 290

EXP_W, EXP_H = 580, 230
COMPACT_W, COMPACT_H = 200, 52
MEDIA_W, MEDIA_H = 360, 62

SYS_W, SYS_H = 290, 60
SYS_EXP_W, SYS_EXP_H = 510, 190

TRAY_H = 62
TRAY_EXP_H = 200

PILL_TOP = 6

FONT = "Noto Sans"
CLOCK_FONT = "Noto Sans"

COL_BG_TOP = (0.09, 0.09, 0.11, 0.97)
COL_BG_BOT = (0.02, 0.02, 0.03, 0.97)
COL_BORDER = (1.0, 1.0, 1.0, 0.10)
COL_TEXT = (1.0, 1.0, 1.0, 0.96)
COL_DIM = (1.0, 1.0, 1.0, 0.55)
COL_FAINT = (1.0, 1.0, 1.0, 0.30)
COL_ACCENT = (0.56, 0.72, 1.0, 1.0)


# Spring


class Spring:
    def __init__(self, value=0.0, stiffness=300.0, damping=32.0, mass=1.0):
        self.value = float(value)
        self.velocity = 0.0
        self.target = float(value)
        self.stiffness = stiffness
        self.damping = damping
        self.mass = mass

    def set(self, target):
        self.target = float(target)

    def snap(self, target):
        self.target = float(target)
        self.value = float(target)
        self.velocity = 0.0

    def update(self, dt):
        force = -self.stiffness * (self.value - self.target)
        damp = -self.damping * self.velocity
        accel = (force + damp) / self.mass
        self.velocity += accel * dt
        self.value += self.velocity * dt
        if abs(self.value - self.target) < 0.05 and abs(self.velocity) < 0.05:
            self.value = self.target
            self.velocity = 0.0
            return False
        return True


# MPRIS


class MPRIS:
    BUS = "org.mpris.MediaPlayer2."
    PATH = "/org/mpris/MediaPlayer2"
    PLAYER = "org.mpris.MediaPlayer2.Player"
    PROPS = "org.freedesktop.DBus.Properties"

    def __init__(self, on_change, on_art):
        self.on_change = on_change
        self.on_art = on_art
        self.bus = dbus.SessionBus()
        self.bus_name = None
        self.prop_iface = None
        self.player_iface = None
        self.meta = {}
        self.status = "Stopped"
        self.volume = 1.0
        self.position = 0
        self._pos_time = time.time()
        self.art_url = ""
        self.art_pixbuf = None
        self._art_cache = {}
        self._art_loading = set()
        self._sig = None

        self.bus.add_signal_receiver(
            self._on_owner_changed,
            signal_name="NameOwnerChanged",
            dbus_interface="org.freedesktop.DBus",
            path="/org/freedesktop/DBus",
        )
        self._select_player()

    def _names(self):
        return [n for n in self.bus.list_names() if n.startswith(self.BUS)]

    def _select_player(self):
        best = None
        best_score = -1
        for name in self._names():
            try:
                props = dbus.Interface(self.bus.get_object(name, self.PATH), self.PROPS)
                status = str(props.Get(self.PLAYER, "PlaybackStatus"))
                meta = props.Get(self.PLAYER, "Metadata")
                has_track = bool(meta.get("xesam:title"))
            except Exception:
                continue
            score = {"Playing": 3, "Paused": 2}.get(status, 1)
            if has_track:
                score += 1
            if score > best_score:
                best, best_score = name, score
        if best:
            self._connect(best)
        else:
            self.bus_name = None
            self.prop_iface = None
            self.player_iface = None
            self.meta = {}
            self.status = "Stopped"
            self._fire_change()

    def _connect(self, name):
        if name == self.bus_name and self.player_iface is not None:
            self._read_all()
            return
        self.bus_name = name
        try:
            obj = self.bus.get_object(name, self.PATH)
            self.prop_iface = dbus.Interface(obj, self.PROPS)
            self.player_iface = dbus.Interface(obj, self.PLAYER)
            self.prop_iface.connect_to_signal(
                "PropertiesChanged",
                self._on_props_changed,
                dbus_interface=self.PROPS,
            )
            self._read_all()
        except Exception as exc:
            print(f"[island] mpris connect failed: {exc}")

    def _read_all(self):
        try:
            meta = self.prop_iface.Get(self.PLAYER, "Metadata")
            self.status = str(self.prop_iface.Get(self.PLAYER, "PlaybackStatus"))
            try:
                self.volume = float(self.prop_iface.Get(self.PLAYER, "Volume"))
            except Exception:
                pass
            try:
                self.position = (
                    int(self.prop_iface.Get(self.PLAYER, "Position")) // 1_000_000
                )
            except Exception:
                self.position = 0
            self._pos_time = time.time()
            self.meta = dict(meta)
            self._apply_meta()
        except Exception:
            pass

    def _on_owner_changed(self, name, old, new):
        if name.startswith(self.BUS):
            self._select_player()

    def _on_props_changed(self, iface, changed, invalidated):
        if "Metadata" in changed:
            self.meta = dict(changed["Metadata"])
            self._apply_meta()
        if "PlaybackStatus" in changed:
            self.status = str(changed["PlaybackStatus"])
        if "Volume" in changed:
            try:
                self.volume = float(changed["Volume"])
            except Exception:
                pass
        self._fire_change()

    def _apply_meta(self):
        url = str(self.meta.get("mpris:artUrl", "") or "")
        if url and url != self.art_url:
            self.art_url = url
            self.art_pixbuf = self._art_cache.get(url)
            self._load_art(url)
        elif not url:
            self.art_url = ""
            self.art_pixbuf = None

    def _fire_change(self):
        sig = (
            self.playback_signature(),
            self.get_title(),
            self.get_artist(),
            self.art_url,
        )
        changed_track = sig != self._sig
        self._sig = sig
        if self.on_change:
            GLib.idle_add(self.on_change, changed_track)

    def playback_signature(self):
        return (self.bus_name, self.status)

    def has_media(self):
        return bool(self.meta.get("xesam:title"))

    def get_title(self):
        return str(self.meta.get("xesam:title", "") or "")

    def get_artist(self):
        artists = self.meta.get("xesam:artist", []) or []
        try:
            return ", ".join(str(a) for a in artists)
        except Exception:
            return ""

    def get_album(self):
        return str(self.meta.get("xesam:album", "") or "")

    def get_length(self):
        try:
            return int(self.meta.get("mpris:length", 0)) // 1_000_000
        except Exception:
            return 0

    def get_position(self):
        if self.is_playing():
            return self.position + (time.time() - self._pos_time)
        return self.position

    @property
    def pid(self):
        """Get the PID of the current player."""
        if self.bus_name is None:
            return None
        try:
            return get_bus_pid(self.bus_name)
        except Exception:
            return None

    def is_playing(self):
        return self.status == "Playing"

    def poll_position(self):
        if not self.prop_iface or not self.is_playing():
            return
        try:
            pos = int(self.prop_iface.Get(self.PLAYER, "Position")) // 1_000_000
            self.position = pos
            self._pos_time = time.time()
        except Exception:
            pass

    def _load_art(self, url):
        if url in self._art_cache:
            self.art_pixbuf = self._art_cache[url]
            return
        if url in self._art_loading:
            return
        if url.startswith("file://"):
            try:
                self._finish_art(url, self._read_url(url))
            except Exception as exc:
                print(f"[island] art: {exc}")
            return
        if requests is None:
            return

        self._art_loading.add(url)

        def worker():
            data = None
            try:
                resp = requests.get(url, timeout=4)
                if resp.ok:
                    data = resp.content
            except Exception:
                pass
            GLib.idle_add(self._finish_art, url, data)

        threading.Thread(target=worker, daemon=True).start()

    @staticmethod
    def _read_url(url):
        if url.startswith("file://"):
            with open(url[7:], "rb") as fh:
                return fh.read()
        return None

    def _load_pixbuf(self, data, size=320):
        if not data:
            return None
        loader = GdkPixbuf.PixbufLoader()
        loader.write(data)
        loader.close()
        pb = loader.get_pixbuf()
        if pb is None:
            return None
        w, h = pb.get_width(), pb.get_height()
        side = min(w, h)
        pb = pb.new_subpixbuf((w - side) // 2, (h - side) // 2, side, side)
        return pb.scale_simple(size, size, GdkPixbuf.InterpType.BILINEAR)

    def _finish_art(self, url, data):
        self._art_loading.discard(url)
        pb = self._load_pixbuf(data)
        self._art_cache[url] = pb
        if url == self.art_url:
            self.art_pixbuf = pb
            if self.on_art:
                self.on_art()
        return False

    def play_pause(self):
        if not self.player_iface:
            return
        try:
            self.player_iface.PlayPause()
        except Exception:
            pass

    def next(self):
        if not self.player_iface:
            return
        try:
            self.player_iface.Next()
        except Exception:
            pass

    def previous(self):
        if not self.player_iface:
            return
        try:
            self.player_iface.Previous()
        except Exception:
            pass

    def seek(self, position_seconds):
        if not self.player_iface:
            return
        try:
            subprocess.run(
                [
                    "playerctl",
                    "--player",
                    self.bus_name,
                    "position",
                    str(max(0.0, float(position_seconds))),
                ],
                check=True,
                timeout=2,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.position = int(max(0, position_seconds))
            self._pos_time = time.time()
            return
        except (subprocess.SubprocessError, FileNotFoundError):
            pass

        try:
            pos_us = int(max(0, position_seconds) * 1_000_000)
            track_id = self.meta.get("mpris:trackid")
            if track_id is not None:
                try:
                    if not isinstance(track_id, dbus.ObjectPath):
                        track_id = dbus.ObjectPath(str(track_id))
                except Exception:
                    pass
                try:
                    self.player_iface.SetPosition(track_id, dbus.Int64(pos_us))
                except Exception:
                    current_us = int(self.get_position() * 1_000_000)
                    self.player_iface.Seek(dbus.Int64(pos_us - current_us))
            else:
                current_us = int(self.get_position() * 1_000_000)
                self.player_iface.Seek(dbus.Int64(pos_us - current_us))
            self.position = int(pos_us // 1_000_000)
            self._pos_time = time.time()
        except Exception:
            pass

    def set_position(self, fraction):
        length = self.get_length()
        if length <= 0:
            return
        self.seek(fraction * length)

    def nudge_volume(self, delta):
        if not self.prop_iface:
            return
        try:
            self.volume = max(0.0, min(1.0, self.volume + delta))
            self.prop_iface.Set(self.PLAYER, "Volume", dbus.Double(self.volume))
        except Exception:
            pass


# Real audio visualization (PipeWire + pw-record + FFT)

SAMPLE_RATE = 44100
CHUNK = 1024
BAND_EDGES_HZ = (250, 2000, 6000, 12500, 16000)
DB_FLOOR, DB_CEIL = -40.0, -10.0
DECAY_PER_FRAME = 0.25

BUS_NAME_RE = re.compile(r"^org\.mpris\.MediaPlayer2\.[A-Za-z0-9_.\-]{1,200}$")


def _to_int(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def run_json(cmd, timeout=3.0):
    try:
        res = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            stdin=subprocess.DEVNULL,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return None
    except OSError:
        return None
    if res.returncode != 0:
        return None
    try:
        return json.loads(res.stdout)
    except json.JSONDecodeError:
        return None


def _unwrap(payload):
    if not isinstance(payload, dict) or "data" not in payload:
        return None
    data = payload["data"]
    if isinstance(data, list) and len(data) >= 1:
        return data[0]
    return data


def _busctl(*args):
    return run_json(["busctl", "--user", "--json=short", *args])


def list_mpris_names():
    payload = _busctl(
        "call",
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ListNames",
    )
    names = _unwrap(payload)
    if not isinstance(names, list):
        return []
    return sorted(n for n in names if isinstance(n, str) and BUS_NAME_RE.match(n))


def get_bus_pid(bus_name):
    payload = _busctl(
        "call",
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetConnectionUnixProcessID",
        "s",
        bus_name,
    )
    pid = _to_int(_unwrap(payload))
    if pid is None or pid <= 0:
        return None
    return pid


def _to_int(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def process_tree(root_pid):
    children = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return {root_pid}
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "rb") as fh:
                data = fh.read()
        except OSError:
            continue
        rest = data.rpartition(b")")[2].split()
        if len(rest) < 2:
            continue
        try:
            ppid = int(rest[1])
        except (IndexError, ValueError):
            continue
        children.setdefault(ppid, []).append(int(entry))
    seen = {root_pid}
    stack = [root_pid]
    while stack:
        for child in children.get(stack.pop(), ()):
            if child not in seen:
                seen.add(child)
                stack.append(child)
    return seen


@dataclass(frozen=True)
class StreamNode:
    serial: int
    name: str
    app_name: str
    binary: str
    pid: int | None
    running: bool


def list_output_streams():
    dump = run_json(["pw-dump"], timeout=5.0)
    if not isinstance(dump, list):
        return []
    streams = []
    for obj in dump:
        if not isinstance(obj, dict) or obj.get("type") != "PipeWire:Interface:Node":
            continue
        info = obj.get("info")
        props = info.get("props") if isinstance(info, dict) else None
        if (
            not isinstance(props, dict)
            or props.get("media.class") != "Stream/Output/Audio"
        ):
            continue
        serial = _to_int(props.get("object.serial"))
        if serial is None:
            continue
        streams.append(
            StreamNode(
                serial=serial,
                name=str(props.get("node.name", "")),
                app_name=str(props.get("application.name", "")),
                binary=os.path.basename(
                    str(props.get("application.process.binary", ""))
                ),
                pid=_to_int(props.get("application.process.id")),
                running=info.get("state") == "running",
            )
        )
    return streams


def _best(candidates):
    return max(candidates, key=lambda s: (s.running, s.serial))


def find_stream_for_player(player_bus, player_pid, streams):
    if player_bus is None or player_pid is None:
        return None, ""
    pids = process_tree(player_pid)
    by_pid = [s for s in streams if s.pid in pids]
    if by_pid:
        return _best(by_pid), "matched by PID"
    # fallback by name
    # get short name from bus
    short = player_bus[len("org.mpris.MediaPlayer2.") :]
    key = short.split(".")[0].lower()
    by_name = [
        s
        for s in streams
        if key and (key == s.binary.lower() or key in s.app_name.lower())
    ]
    if by_name:
        return _best(by_name), "matched by name"
    return None, ""


class SpectrumAnalyzer:
    def __init__(self, chunk=CHUNK, rate=SAMPLE_RATE, edges=BAND_EDGES_HZ):
        if edges[-1] > rate / 2:
            raise ValueError("band edges exceed Nyquist")
        self.chunk = chunk
        self.window = np.hanning(chunk).astype(np.float32)
        self.win_norm = float(self.window.sum()) / 2.0
        freqs = np.fft.rfftfreq(chunk, 1.0 / rate)
        self.slices = []
        for lo, hi in zip(edges[:-1], edges[1:]):
            idx = np.flatnonzero((freqs >= lo) & (freqs < hi))
            if idx.size == 0:
                raise ValueError("band has no FFT bins; increase chunk")
            self.slices.append(slice(int(idx[0]), int(idx[-1]) + 1))

    @property
    def bands(self):
        return len(self.slices)

    def levels(self, pcm):
        x = pcm.astype(np.float32) / 32768.0
        spec = np.abs(np.fft.rfft(x * self.window)) / self.win_norm
        out = np.empty(len(self.slices), dtype=np.float32)
        for i, sl in enumerate(self.slices):
            amp = math.sqrt(float(np.sum(spec[sl] ** 2)))
            db = 20.0 * math.log10(max(amp, 1e-9))
            out[i] = (db - DB_FLOOR) / (DB_CEIL - DB_FLOOR)
        return np.clip(np.nan_to_num(out), 0.0, 1.0)


class Capture:
    def __init__(self, serial):
        self.err_text = ""
        self._err = tempfile.TemporaryFile()
        cmd = [
            "pw-record",
            f"--target={serial}",
            "--format=s16",
            f"--rate={SAMPLE_RATE}",
            "--channels=1",
            "-P",
            "{ node.dont-fallback=true node.dont-reconnect=true }",
            "-",
        ]
        try:
            self.proc = subprocess.Popen(
                cmd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=self._err,
                close_fds=True,
            )
        except OSError:
            raise RuntimeError("cannot start pw-record")
        assert self.proc.stdout is not None
        self.stream = self.proc.stdout

    def stderr_tail(self, limit=300):
        try:
            self._err.seek(0)
            return self._err.read().decode("utf-8", "replace").strip()[-limit:]
        except (OSError, ValueError):
            return ""

    def close(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.err_text = self.stderr_tail()
        try:
            if self.stream:
                self.stream.close()
        finally:
            if self._err:
                self._err.close()


def consume_wav_header(stream):
    head = stream.read(12)
    if len(head) < 12 or head[:4] != b"RIFF" or head[8:12] != b"WAVE":
        return head
    while True:
        chunk_hdr = stream.read(8)
        if len(chunk_hdr) < 8:
            return b""
        chunk_id = chunk_hdr[:4]
        size = int.from_bytes(chunk_hdr[4:], "little")
        if chunk_id == b"data":
            return b""
        skip = size + (size & 1)
        if skip > 65536:
            return b""
        if len(stream.read(skip)) < skip:
            return b""
    return b""


class AudioVisualizer:
    """Background thread that captures audio and produces bar levels."""

    def __init__(self, mpris_ref, on_levels):
        self.mpris_ref = mpris_ref
        self.on_levels = on_levels
        self.analyzer = SpectrumAnalyzer()
        self.capture = None
        self.streams = []
        self._streams_cache_time = 0
        self._streams_cache_ttl = 2.0  # seconds
        self.running = False
        self.thread = None
        self._target_serial = None
        self._frame_bytes = self.analyzer.chunk * 2
        self.smooth = np.zeros(self.analyzer.bands, dtype=np.float32)
        self._pending = b""
        self._capture = None
        self._target_serial = None
        self._attack = 0.7  # fast attack
        self._release = 0.15  # smooth decay

    def start(self):
        if self.running:
            return
        self.running = True
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def stop(self):
        self.running = False
        if self._capture:
            self._capture.close()
            self._capture = None
        if self.thread:
            self.thread.join(timeout=2)

    def _find_stream(self):
        player = self.mpris_ref()
        if not player or player.bus_name is None:
            return None
        now = time.time()
        if now - self._streams_cache_time > self._streams_cache_ttl:
            self.streams = list_output_streams()
            self._streams_cache_time = now
        return find_stream_for_player(player.bus_name, player.pid, self.streams)

    def _run(self):
        pending = b""
        while self.running:
            # Find/refresh stream
            stream_info = self._find_stream()
            if stream_info is None:
                # No stream found - publish muted levels
                self.on_levels(np.zeros(self.analyzer.bands, dtype=np.float32), False)
                time.sleep(1)
                self.streams = list_output_streams()
                continue

            stream, how = stream_info
            if stream.serial != self._target_serial:
                if self._capture:
                    self._capture.close()
                self._capture = Capture(stream.serial)
                self._target_serial = stream.serial
                pending = b""
                # consume WAV header
                head = self._capture.stream.read(12)
                if len(head) >= 12 and head[:4] == b"RIFF" and head[8:12] == b"WAVE":
                    while True:
                        chunk_hdr = self._capture.stream.read(8)
                        if len(chunk_hdr) < 8:
                            break
                        chunk_id = chunk_hdr[:4]
                        size = int.from_bytes(chunk_hdr[4:], "little")
                        if chunk_id == b"data":
                            break
                        skip = size + (size & 1)
                        if skip > 65536:
                            break
                        self._capture.stream.read(skip)
                pending = b""

            # Read audio frame
            frame_bytes = self.analyzer.chunk * 2
            raw = pending + self._capture.stream.read(frame_bytes - len(pending))
            pending = b""
            if len(raw) < self.analyzer.chunk * 2:
                # EOF - stream ended
                self._capture.close()
                self._capture = None
                self._target_serial = None
                self.on_levels(np.zeros(self.analyzer.bands, dtype=np.float32), False)
                time.sleep(0.5)
                self.streams = list_output_streams()
                continue

            pending = raw[frame_bytes:]
            pcm = np.frombuffer(raw[:frame_bytes], dtype="<i2")
            levels = self.analyzer.levels(pcm)
            # Smooth attack/decay
            for i in range(len(self.smooth)):
                if levels[i] > self.smooth[i]:
                    self.smooth[i] = self.smooth[i] + 0.6 * (levels[i] - self.smooth[i])
                else:
                    self.smooth[i] = self.smooth[i] + 0.12 * (
                        levels[i] - self.smooth[i]
                    )
            self.smooth = np.maximum(self.smooth, 0.0)
            self.on_levels(self.smooth, True)

            # Small sleep to not hammer CPU
            time.sleep(0.001)


# Update MPRIS to expose pid and bus_name


# BlueZ battery


class BlueZ:
    def __init__(self, on_change):
        self.on_change = on_change
        self.bus = None
        self.name = ""
        self.percentage = None
        self.connected = False
        try:
            self.bus = dbus.SystemBus()
            for sig, path in (
                ("InterfacesAdded", "/"),
                ("InterfacesRemoved", "/"),
            ):
                self.bus.add_signal_receiver(
                    lambda *_: self.refresh(),
                    signal_name=sig,
                    dbus_interface="org.freedesktop.DBus.ObjectManager",
                    path=path,
                )
        except Exception as exc:
            print(f"[island] bluez: {exc}")
        self.refresh(notify=False)

    def refresh(self, notify=True):
        if self.bus is None:
            return
        found = None
        try:
            mgr = dbus.Interface(
                self.bus.get_object("org.bluez", "/"),
                "org.freedesktop.DBus.ObjectManager",
            )
            for path, ifaces in mgr.GetManagedObjects().items():
                dev = ifaces.get("org.bluez.Device1")
                if not dev or not dev.get("Connected"):
                    continue
                bat = ifaces.get("org.bluez.Battery1")
                name = str(dev.get("Name") or dev.get("Alias") or "Bluetooth device")
                pct = (
                    int(bat.get("Percentage"))
                    if bat and bat.get("Percentage") is not None
                    else None
                )
                score = 2 if pct is not None else 1
                if found is None or score > found[0]:
                    found = (score, name, pct)
        except Exception:
            pass

        if found:
            _, name, pct = found
            changed = (name, pct) != (self.name, self.percentage)
            self.name, self.percentage, self.connected = name, pct, True
        else:
            changed = self.connected or bool(self.name)
            self.name, self.percentage, self.connected = "", None, False
        if changed and notify and self.on_change:
            GLib.idle_add(self.on_change)


# System tray (StatusNotifier host that reads an existing watcher)


class TrayItem:
    def __init__(self, identifier, bus_name, path, proxy):
        self.identifier = identifier
        self.bus_name = bus_name
        self.path = path
        self.proxy = proxy
        self.title = ""
        self.pixbuf = None
        self.menu_path = None
        self.menu = None


class TrayHost:
    WATCHER = "org.kde.StatusNotifierWatcher"
    WATCHER_PATH = "/StatusNotifierWatcher"
    WATCHER_IFACE = "org.kde.StatusNotifierWatcher"
    ITEM_IFACE = "org.kde.StatusNotifierItem"

    def __init__(self, on_change):
        self.on_change = on_change
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.watcher = None
        self._using_own_watcher = False
        self._by_id = {}
        self.items = []
        self._theme = Gtk.IconTheme.get_default()
        # Try to create our own watcher if none exists
        try:
            if dbus is not None:
                sb = dbus.SessionBus()
                self._own_watcher = SNIWatcher(sb)
            else:
                self._own_watcher = None
        except Exception:
            self._own_watcher = None
        self._connect_watcher()
        GLib.timeout_add_seconds(20, self._periodic)

    def _periodic(self):
        self.refresh()
        return True

    def _connect_watcher(self):
        try:
            self.watcher = Gio.DBusProxy.new_for_bus_sync(
                Gio.BusType.SESSION,
                Gio.DBusProxyFlags.NONE,
                None,
                self.WATCHER,
                self.WATCHER_PATH,
                self.WATCHER_IFACE,
                None,
            )
            self.watcher.connect("g-signal", self._on_watcher_signal)
            self._using_own_watcher = False
            self.refresh()
        except Exception:
            self.watcher = None
            if self._own_watcher:
                self._using_own_watcher = True
                self.refresh()
            else:
                GLib.timeout_add_seconds(5, self._retry)

    def _retry(self):
        if self.watcher is None:
            self._connect_watcher()
        return False

    def _use_own_watcher(self):
        self._using_own_watcher = True
        self.refresh()

    def _on_watcher_signal(self, proxy, sender, signal_name, params):
        if signal_name in (
            "StatusNotifierItemRegistered",
            "StatusNotifierItemUnregistered",
        ):
            self.refresh()

    def _service_strings(self):
        if self.watcher is None and not getattr(self, "_using_own_watcher", False):
            return []
        if getattr(self, "_using_own_watcher", False) and self._own_watcher:
            return list(self._own_watcher._items)
        try:
            reply = self.bus.call_sync(
                self.WATCHER,
                self.WATCHER_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                GLib.Variant(
                    "(ss)", (self.WATCHER_IFACE, "RegisteredStatusNotifierItems")
                ),
                None,
                Gio.DBusCallFlags.NONE,
                1000,
                None,
            )
            return list(reply.unpack()[0])
        except Exception:
            prop = self.watcher.get_cached_property("RegisteredStatusNotifierItems")
            return list(prop.unpack()) if prop else []

    def refresh(self):
        services = self._service_strings()
        ids = []
        for service in services:
            if "/" in service:
                name, rest = service.split("/", 1)
                path = "/" + rest
            else:
                name, path = service, "/StatusNotifierItem"
            ident = name + path
            ids.append(ident)
            if ident in self._by_id:
                continue
            try:
                proxy = Gio.DBusProxy.new_for_bus_sync(
                    Gio.BusType.SESSION,
                    Gio.DBusProxyFlags.NONE,
                    None,
                    name,
                    path,
                    self.ITEM_IFACE,
                    None,
                )
            except Exception:
                continue
            item = TrayItem(ident, name, path, proxy)
            proxy.connect(
                "g-properties-changed", lambda *a, it=item: self._update_item(it)
            )
            proxy.connect("g-signal", lambda *a, it=item: self._update_item(it))
            self._by_id[ident] = item
            self._update_item(item, notify=False)
            self._prepare_menu(item)

        for ident in list(self._by_id):
            if ident not in ids:
                del self._by_id[ident]

        self.items = [self._by_id[i] for i in ids if i in self._by_id]
        if self.on_change:
            GLib.idle_add(self.on_change)

    def _update_item(self, item, notify=True):
        try:
            title = item.proxy.get_cached_property("Title")
            ident = item.proxy.get_cached_property("Id")
            raw = str((title or ident).unpack()) if (title or ident) else ""
            item.title = self._pretty_title(raw)

            mpath = item.proxy.get_cached_property("Menu")
            item.menu_path = str(mpath.unpack()) if mpath else None

            pixbuf = None
            pix = item.proxy.get_cached_property("IconPixmap")
            if pix is not None:
                pixbuf = self._pixbuf_from_pixmap(pix.unpack())
            if pixbuf is None:
                name = item.proxy.get_cached_property("IconName")
                if name is not None:
                    pixbuf = self._pixbuf_from_name(str(name.unpack()))
            item.pixbuf = pixbuf
        except Exception:
            pass
        if notify and self.on_change:
            GLib.idle_add(self.on_change)

    @staticmethod
    def _pretty_title(raw):
        t = raw.replace("_status_icon", "").replace("-status-icon", "")
        if "_" in t and " " not in t:
            t = t.split("_")[0]
        t = t.replace("_", " ").strip()
        if t and t[0].islower():
            t = t[0].upper() + t[1:]
        return t or raw

    def _pixbuf_from_name(self, name):
        if not name or self._theme is None:
            return None
        try:
            return self._theme.load_icon(name, 64, 0)
        except Exception:
            return None

    @staticmethod
    def _pixbuf_from_pixmap(arr):
        if not arr:
            return None
        best = None
        for entry in arr:
            try:
                pw, ph, data = entry
            except Exception:
                continue
            if best is None or pw * ph > best[0] * best[1]:
                best = (pw, ph, data)
        if best is None:
            return None
        pw, ph, data = best
        if hasattr(data, "get_data"):
            data = data.get_data()
        buf = bytearray(data)
        need = pw * ph * 4
        if len(buf) < need:
            return None
        for i in range(0, need, 4):
            a, r, g, b = buf[i], buf[i + 1], buf[i + 2], buf[i + 3]
            buf[i], buf[i + 1], buf[i + 2], buf[i + 3] = r, g, b, a
        try:
            pb = GdkPixbuf.Pixbuf.new_from_bytes(
                GLib.Bytes.new(buf),
                GdkPixbuf.Colorspace.RGB,
                True,
                8,
                pw,
                ph,
                pw * 4,
            )
        except Exception:
            return None
        if pw > 64 or ph > 64:
            pb = pb.scale_simple(64, 64, GdkPixbuf.InterpType.BILINEAR)
        return pb

    def _prepare_menu(self, item):
        if item.menu is not None or not item.menu_path or item.menu_path == "/":
            return
        owner = item.proxy.get_name_owner() or item.bus_name
        try:
            item.menu = DbusmenuGtk3.Menu.new(owner, item.menu_path)
        except Exception as exc:
            print(f"[island] tray menu prepare: {exc}")

    def open_menu(self, identifier, event):
        item = self._by_id.get(identifier)
        if item is None:
            return
        if item.menu is None:
            self._prepare_menu(item)
        if item.menu is not None:
            try:
                item.menu.popup_at_pointer(event)
                return
            except Exception as exc:
                print(f"[island] tray menu: {exc}")
        try:
            item.proxy.call_sync(
                "ContextMenu",
                GLib.Variant("(ii)", (int(event.x_root), int(event.y_root))),
                Gio.DBusCallFlags.NONE,
                1000,
                None,
            )
        except Exception:
            pass

    def activate(self, identifier):
        item = self._by_id.get(identifier)
        if item is None:
            return
        try:
            item.proxy.call_sync(
                "Activate",
                GLib.Variant("(ii)", (0, 0)),
                Gio.DBusCallFlags.NONE,
                1000,
                None,
            )
        except Exception:
            try:
                item.proxy.call_sync(
                    "SecondaryActivate",
                    GLib.Variant("(ii)", (0, 0)),
                    Gio.DBusCallFlags.NONE,
                    1000,
                    None,
                )
            except Exception:
                pass


# cairo helpers


def rounded_path(cr, x, y, w, h, r):
    r = max(0.0, min(r, w / 2, h / 2))
    cr.new_sub_path()
    cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    cr.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    cr.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    cr.close_path()


def fill_rounded(cr, x, y, w, h, r, rgba):
    rounded_path(cr, x, y, w, h, r)
    cr.set_source_rgba(*rgba)
    cr.fill()


def stroke_rounded(cr, x, y, w, h, r, rgba, width=1.0):
    rounded_path(cr, x + 0.5, y + 0.5, w - 1, h - 1, r)
    cr.set_source_rgba(*rgba)
    cr.set_line_width(width)
    cr.stroke()


def draw_text(
    cr,
    text,
    x,
    y,
    size,
    color=COL_TEXT,
    weight=Pango.Weight.NORMAL,
    maxw=None,
    family=FONT,
    align_right=False,
    align_center=False,
):
    layout = PangoCairo.create_layout(cr)
    desc = Pango.FontDescription()
    desc.set_family(family)
    desc.set_size(int(size * Pango.SCALE))
    desc.set_weight(weight)
    layout.set_font_description(desc)
    if maxw is not None:
        layout.set_width(int(maxw * Pango.SCALE))
        layout.set_ellipsize(Pango.EllipsizeMode.END)
    else:
        layout.set_width(-1)
    layout.set_text(text, -1)
    ink, logical = layout.get_extents()
    w = logical.width / Pango.SCALE
    if align_right:
        x -= w
    elif align_center:
        x -= w / 2
    cr.set_source_rgba(*color)
    cr.move_to(x, y)
    PangoCairo.show_layout(cr, layout)
    return w, logical.height / Pango.SCALE


def draw_pixbuf(cr, pb, x, y, size, opacity=1.0):
    if pb is None:
        return
    pw, ph = pb.get_width(), pb.get_height()
    if pw <= 0 or ph <= 0:
        return
    cr.save()
    cr.translate(x, y)
    cr.scale(size / pw, size / ph)
    Gdk.cairo_set_source_pixbuf(cr, pb, 0, 0)
    cr.paint_with_alpha(opacity)
    cr.restore()


def draw_art(cr, pixbuf, x, y, size, radius, opacity=1.0):
    cr.save()
    rounded_path(cr, x, y, size, size, radius)
    cr.clip()
    if pixbuf is not None:
        draw_pixbuf(cr, pixbuf, x, y, size, opacity)
    else:
        grad = cairo.LinearGradient(x, y, x + size, y + size)
        grad.add_color_stop_rgba(0, 0.22, 0.24, 0.30, opacity)
        grad.add_color_stop_rgba(1, 0.10, 0.11, 0.15, opacity)
        cr.set_source(grad)
        cr.paint()
        draw_text(
            cr,
            "♪",
            x + size / 2,
            y + size / 2 - size * 0.22,
            size * 0.34,
            (1, 1, 1, 0.5 * opacity),
            align_center=True,
        )
    cr.restore()
    stroke_rounded(cr, x, y, size, size, radius, (1, 1, 1, 0.10 * opacity))


def draw_real_eq(cr, x, y, h, levels, color, opacity=1.0):
    widths = 3.0
    gap = 3.0
    for i in range(min(4, len(levels))):
        frac = max(0.05, min(1.0, float(levels[i]) * 1.2))
        bh = max(2.0, h * frac)
        bx = x + i * (widths + gap)
        by = y + (h - bh)
        fill_rounded(
            cr,
            bx,
            by,
            widths,
            bh,
            widths / 2,
            (color[0], color[1], color[2], color[3] * opacity),
        )


def draw_eq(cr, x, y, h, t, color, opacity=1.0, playing=False):
    widths = 3.0
    gap = 3.0
    phases = (0.0, 0.9, 1.8, 0.4)
    for i, ph in enumerate(phases):
        if playing:
            frac = 0.30 + 0.70 * (0.5 + 0.5 * math.sin(t * 3.4 + ph))
        else:
            frac = 0.15
        bh = max(2.0, h * frac)
        bx = x + i * (widths + gap)
        by = y + (h - bh)
        fill_rounded(
            cr,
            bx,
            by,
            widths,
            bh,
            widths / 2,
            (color[0], color[1], color[2], color[3] * opacity),
        )


def draw_play_icon(cr, cx, cy, s, playing, color):
    if playing:
        bw = s * 0.22
        bh = s * 1.0
        for dx in (-s * 0.22, s * 0.22):
            fill_rounded(cr, cx + dx - bw / 2, cy - bh / 2, bw, bh, bw / 2, color)
    else:
        cr.set_source_rgba(*color)
        cr.move_to(cx - s * 0.28, cy - s * 0.5)
        cr.line_to(cx - s * 0.28, cy + s * 0.5)
        cr.line_to(cx + s * 0.48, cy)
        cr.close_path()
        cr.fill()


def draw_skip_icon(cr, cx, cy, s, direction, color):
    d = direction
    base_x = cx - d * s * 0.30
    apex_x = cx + d * s * 0.45
    cr.set_source_rgba(*color)
    cr.move_to(base_x, cy - s * 0.5)
    cr.line_to(base_x, cy + s * 0.5)
    cr.line_to(apex_x, cy)
    cr.close_path()
    cr.fill()
    bar_x = cx + d * s * 0.62
    fill_rounded(cr, bar_x - 1.4, cy - s * 0.5, 2.8, s, 1.4, color)


def draw_headphone(cr, cx, cy, s, color):
    cr.save()
    cr.set_line_cap(cairo.LINE_CAP_ROUND)
    cr.set_line_width(s * 0.11)
    cr.set_source_rgba(*color)
    cr.arc(cx, cy + s * 0.05, s * 0.40, math.pi, 2 * math.pi)
    cr.stroke()
    fill_rounded(cr, cx - s * 0.50, cy + s * 0.02, s * 0.17, s * 0.40, s * 0.085, color)
    fill_rounded(cr, cx + s * 0.33, cy + s * 0.02, s * 0.17, s * 0.40, s * 0.085, color)
    cr.restore()


# Real spectrum analyzer using PipeWire

SAMPLE_RATE = 44100
BAND_EDGES_HZ = (20, 250, 2000, 6000, 16000)
DB_FLOOR, DB_CEIL = -70.0, -15.0
DECAY_PER_FRAME = 0.08


class RealSpectrum:
    def __init__(self):
        self.analyzer = None
        self.levels = np.array([0.0, 0.0, 0.0, 0.0], dtype=np.float32)
        self.smooth_levels = np.array([0.0, 0.0, 0.0, 0.0], dtype=np.float32)
        self.running = False
        self.proc = None
        self._init_analyzer()

    def _init_analyzer(self):
        try:
            chunk = 1024
            rate = SAMPLE_RATE
            edges = BAND_EDGES_HZ
            self.chunk = chunk
            self.window = np.hanning(chunk).astype(np.float32)
            self.win_norm = float(self.window.sum()) / 2.0
            freqs = np.fft.rfftfreq(chunk, 1.0 / rate)
            self.slices = []
            for lo, hi in zip(edges[:-1], edges[1:]):
                idx = np.flatnonzero((freqs >= lo) & (freqs < hi))
                if idx.size > 0:
                    self.slices.append(slice(int(idx[0]), int(idx[-1]) + 1))
        except Exception as e:
            print(f"spectrum init error: {e}")

    def start_capture(self):
        if self.running:
            return
        self.running = True
        thread = threading.Thread(target=self._capture_loop, daemon=True)
        thread.start()

    def stop_capture(self):
        self.running = False
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.terminate()
            except:
                pass
        self.proc = None
        self.smooth_levels = np.array([0.0, 0.0, 0.0, 0.0], dtype=np.float32)

    def _capture_loop(self):
        try:
            # Try to capture from default source or find player stream
            # Use pw-record with a longer buffer approach
            self.proc = subprocess.Popen(
                [
                    "pw-record",
                    "--format=s16",
                    f"--rate={SAMPLE_RATE}",
                    "--channels=1",
                    "-",
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            frame_bytes = self.chunk * 2

            while self.running and self.proc and self.proc.poll() is None:
                try:
                    raw = self.proc.stdout.read(frame_bytes)
                    if len(raw) < frame_bytes:
                        break
                    pcm = np.frombuffer(raw, dtype="<i2")
                    if len(pcm) == 0:
                        continue
                    x = pcm.astype(np.float32) / 32768.0
                    spec = np.abs(np.fft.rfft(x * self.window)) / self.win_norm
                    out = np.empty(len(self.slices), dtype=np.float32)
                    for i, sl in enumerate(self.slices):
                        amp = math.sqrt(float(np.sum(spec[sl] ** 2)))
                        db = 20.0 * math.log10(max(amp, 1e-9))
                        out[i] = (db - DB_FLOOR) / (DB_CEIL - DB_FLOOR)
                    self.levels = np.clip(np.nan_to_num(out), 0.0, 1.0)
                    # Smooth decay
                    # Smooth attack/decay
                    for i in range(len(self.smooth_levels)):
                        if self.levels[i] > self.smooth_levels[i]:
                            self.smooth_levels[i] = self.smooth_levels[i] + 0.6 * (
                                self.levels[i] - self.smooth_levels[i]
                            )
                        else:
                            self.smooth_levels[i] = self.smooth_levels[i] + 0.12 * (
                                self.levels[i] - self.smooth_levels[i]
                            )
                    self.smooth_levels = np.maximum(self.smooth_levels, 0.0)
                except Exception:
                    break
        except Exception as e:
            print(f"capture error: {e}")
        finally:
            self.running = False
            if self.proc and self.proc.poll() is None:
                try:
                    self.proc.terminate()
                except:
                    pass


# The island


class Island(Gtk.Window):
    MUSIC, SYS, TRAY = 0, 1, 2
    VIEW_NAMES = ("music", "system", "tray")

    def __init__(self):
        super().__init__(type=Gtk.WindowType.TOPLEVEL)
        self.set_decorated(False)
        self.set_resizable(False)
        self.set_skip_taskbar_hint(True)
        self.set_skip_pager_hint(True)
        self.set_keep_above(True)
        self.set_app_paintable(True)

        visual = self.get_screen().get_rgba_visual()
        if visual:
            self.set_visual(visual)

        GtkLayerShell.init_for_window(self)
        GtkLayerShell.set_layer(self, GtkLayerShell.Layer.TOP)
        GtkLayerShell.set_anchor(self, GtkLayerShell.Edge.TOP, True)
        GtkLayerShell.set_margin(self, GtkLayerShell.Edge.TOP, PILL_TOP + 4)
        # Reserve fixed space so windows underneath adjust
        GtkLayerShell.set_exclusive_zone(self, COMPACT_H + 8)
        GtkLayerShell.set_keyboard_mode(self, GtkLayerShell.KeyboardMode.NONE)
        GtkLayerShell.set_namespace(self, "dynamic-island")

        self.set_size_request(WIN_W, WIN_H)

        self.area = Gtk.DrawingArea()
        self.area.set_size_request(WIN_W, WIN_H)
        self.area.add_events(
            Gdk.EventMask.BUTTON_PRESS_MASK
            | Gdk.EventMask.BUTTON_RELEASE_MASK
            | Gdk.EventMask.POINTER_MOTION_MASK
            | Gdk.EventMask.ENTER_NOTIFY_MASK
            | Gdk.EventMask.LEAVE_NOTIFY_MASK
            | Gdk.EventMask.SCROLL_MASK
        )
        self.area.connect("draw", self._on_draw)
        self.area.connect("button-press-event", self._on_press)
        self.area.connect("button-release-event", self._on_release)
        self.area.connect("motion-notify-event", self._on_motion)
        self.area.connect("enter-notify-event", self._on_enter)
        self.area.connect("leave-notify-event", self._on_leave)
        self.area.connect("scroll-event", self._on_scroll)
        self.add(self.area)

        self.s_width = Spring(COMPACT_W, 330, 30)
        self.s_height = Spring(COMPACT_H, 330, 30)
        self.s_radius = Spring(COMPACT_H / 2, 330, 30)
        self.s_content = Spring(0.0, 260, 30)

        self.view = self.MUSIC
        self.expanded = False
        self.hover = False
        self.expand_until = 0.0
        self.press_pos = None
        self.click_consumed = False
        self.hit = {}
        self.mouse = (-1000.0, -1000.0)
        self._geom_dirty = True
        self._last_poll = 0.0
        self._last_bluez = 0.0

        self.mpris = MPRIS(self._on_media_change, self._on_art_ready)
        self.tray = TrayHost(self._on_tray_change)
        self.bluez = BlueZ(self._on_bluez_change)

        # Audio visualizer
        self.audio_levels = np.zeros(4, dtype=np.float32)
        self.audio_playing = False
        self.audio_viz = AudioVisualizer(
            lambda: self.mpris,
            self._on_audio_levels,
        )
        self.audio_viz.start()

        self._refresh(initial=True)
        self.show_all()
        self.area.queue_draw()

        GLib.timeout_add(16, self._tick)
        GLib.timeout_add(250, self._poll)
        GLib.timeout_add(1000, self._slow_tick)

    # -- size / state ------------------------------------------------------
    def _update_exclusive_zone(self):
        # Always reserve fixed compact space for bar-like behavior
        try:
            GtkLayerShell.set_layer(self, GtkLayerShell.Layer.TOP)
            GtkLayerShell.set_exclusive_zone(self, COMPACT_H + 8)
        except Exception:
            pass

    def _targets(self):
        if self.expanded:
            if self.view == self.MUSIC:
                return EXP_W, EXP_H, 32, 1.0
            if self.view == self.SYS:
                return SYS_EXP_W, SYS_EXP_H, 32, 1.0
            n = len(self.tray.items)
            w = max(500, 64 + n * 72) if n else 500
            return min(w, WIN_W - 40), TRAY_EXP_H, 32, 1.0

        if self.view == self.MUSIC:
            if self.mpris.has_media():
                return MEDIA_W, MEDIA_H, 24, 0.0
            return COMPACT_W, COMPACT_H, COMPACT_H / 2, 0.0
        if self.view == self.SYS:
            return SYS_W, SYS_H, SYS_H / 2, 0.0
        n = len(self.tray.items)
        w = max(210, 56 + n * 38) if n else 210
        return min(w, WIN_W - 40), TRAY_H, TRAY_H / 2, 0.0

    def _refresh(self, initial=False):
        w, h, r, c = self._targets()
        if initial:
            self.s_width.snap(w)
            self.s_height.snap(h)
            self.s_radius.snap(r)
            self.s_content.snap(c)
        else:
            self.s_width.set(w)
            self.s_height.set(h)
            self.s_radius.set(r)
            self.s_content.set(c)
        self._update_exclusive_zone()

    def expand(self, auto=False):
        if self.expanded:
            return
        self.expanded = True
        self._refresh()
        self._update_exclusive_zone()
        self.expand_until = (time.time() + 5.0) if auto else 0.0

    def collapse(self):
        if not self.expanded:
            return
        self.expanded = False
        self._refresh()
        self._update_exclusive_zone()
        self.expand_until = 0.0

    def cycle_view(self, direction=1):
        self.view = (self.view + direction) % 3
        if not self.expanded:
            self.expanded = True
            self._refresh()
            self.expand_until = time.time() + 3.5
        else:
            self._refresh()
            self.expand_until = time.time() + 4.0
        self.area.queue_draw()

    def _can_expand(self):
        return self.view != self.MUSIC or self.mpris.has_media()

    # -- callbacks ---------------------------------------------------------
    def _on_media_change(self, track_changed):
        self.area.queue_draw()
        if self.view != self.MUSIC:
            return False
        has = self.mpris.has_media() or self.mpris.is_playing()
        if self.expanded:
            if not has:
                self.collapse()
            return False
        self._refresh()
        return False

    def _on_art_ready(self):
        self.area.queue_draw()
        return False

    def _on_tray_change(self):
        self.area.queue_draw()
        if self.view == self.TRAY:
            self._refresh()
        return False

    def _on_bluez_change(self):
        self.area.queue_draw()
        return False

    def _on_audio_levels(self, levels, is_playing):
        self.audio_levels = levels
        self.audio_playing = is_playing
        # Only queue_draw when playing (for animation) or when state changes
        if is_playing:
            self.area.queue_draw()

    # -- loops -------------------------------------------------------------
    def _tick(self):
        moving = False
        for s in (self.s_width, self.s_height, self.s_radius, self.s_content):
            if s.update(1 / 60):
                moving = True
        if not moving and not self._geom_dirty:
            if (
                self.expanded
                and not self.hover
                and self.expand_until
                and time.time() > self.expand_until
            ):
                self.collapse()
        if moving:
            self._geom_dirty = True
        if self._geom_dirty:
            self._apply_input_region()
            self._update_exclusive_zone()
            self.area.queue_draw()
            self._geom_dirty = False
        elif self.view == self.MUSIC and self.mpris.is_playing():
            self.area.queue_draw()
        return True

    def _poll(self):
        now = time.time()
        if now - self._last_poll >= 0.5:
            self._last_poll = now
            self.mpris.poll_position()
            if self.expanded and self.view == self.MUSIC:
                self.area.queue_draw()
        return True

    def _slow_tick(self):
        if time.time() - self._last_bluez > 15:
            self._last_bluez = time.time()
            self.bluez.refresh()
        self.area.queue_draw()
        return True

    def _pill_rect(self):
        w = self.s_width.value
        h = self.s_height.value
        return (WIN_W - w) / 2, 0.0, w, h

    def _apply_input_region(self):
        gdk_window = self.get_window()
        if gdk_window is None:
            return False
        x, y, w, h = self._pill_rect()
        rect = cairo.RectangleInt(int(x), int(y), max(1, int(w)), max(1, int(h)))
        gdk_window.input_shape_combine_region(cairo.Region(rect), 0, 0)
        return True

    # -- drawing -----------------------------------------------------------
    def _on_draw(self, widget, cr):
        w, h = self.s_width.value, self.s_height.value
        x, y = (WIN_W - w) / 2, 0.0
        r = self.s_radius.value
        t = max(0.0, min(1.0, self.s_content.value))

        cr.set_operator(cairo.OPERATOR_CLEAR)
        cr.paint()
        cr.set_operator(cairo.OPERATOR_OVER)

        self._draw_shadow(cr, x, y, w, h, r)
        self._draw_body(cr, x, y, w, h, r)

        cr.save()
        rounded_path(cr, x, y, w, h, r)
        cr.clip()

        compact_a = max(0.0, 1.0 - t / 0.30)
        expanded_a = max(0.0, (t - 0.38) / 0.62)
        if compact_a > 0.005:
            self._draw_compact(cr, x, y, w, h, compact_a)
        if expanded_a > 0.005:
            self._draw_expanded(cr, x, y, w, h, expanded_a)
        cr.restore()

        stroke_rounded(cr, x, y, w, h, r, COL_BORDER)
        return False

    def _draw_shadow(self, cr, x, y, w, h, r):
        for i in range(10, 0, -1):
            alpha = 0.020 * (1 - i / 11)
            rounded_path(
                cr, x - i * 0.6, y + 2 + i * 0.9, w + i * 1.2, h + i * 1.2, r + i * 0.6
            )
            cr.set_source_rgba(0, 0, 0, alpha)
            cr.fill()

    def _draw_body(self, cr, x, y, w, h, r):
        grad = cairo.LinearGradient(x, y, x, y + h)
        grad.add_color_stop_rgba(0, *COL_BG_TOP)
        grad.add_color_stop_rgba(1, *COL_BG_BOT)
        rounded_path(cr, x, y, w, h, r)
        cr.set_source(grad)
        cr.fill()

        sheen = cairo.LinearGradient(x, y, x, y + min(h, 30))
        sheen.add_color_stop_rgba(0, 1, 1, 1, 0.055)
        sheen.add_color_stop_rgba(1, 1, 1, 1, 0.0)
        rounded_path(cr, x, y, w, h, r)
        cr.set_source(sheen)
        cr.fill()

    def _draw_compact(self, cr, x, y, w, h, alpha):
        if alpha <= 0.01:
            return
        if self.view == self.MUSIC:
            self._compact_music(cr, x, y, w, h, alpha)
        elif self.view == self.SYS:
            self._compact_sys(cr, x, y, w, h, alpha)
        else:
            self._compact_tray(cr, x, y, w, h, alpha)

    def _draw_expanded(self, cr, x, y, w, h, alpha):
        if self.view == self.MUSIC:
            self._expanded_music(cr, x, y, w, h, alpha)
        elif self.view == self.SYS:
            self._expanded_sys(cr, x, y, w, h, alpha)
        else:
            self._expanded_tray(cr, x, y, w, h, alpha)
        self._draw_mode_dots(cr, x, y, w, alpha)

    def _draw_mode_dots(self, cr, x, y, w, alpha):
        for i in range(3):
            cx = x + w - 26 - (2 - i) * 10
            active = i == self.view
            cr.arc(cx, y + 18, 2.4, 0, 2 * math.pi)
            if active:
                cr.set_source_rgba(COL_ACCENT[0], COL_ACCENT[1], COL_ACCENT[2], alpha)
            else:
                cr.set_source_rgba(1, 1, 1, 0.18 * alpha)
            cr.fill()

    # -- compact views -----------------------------------------------------
    def _compact_music(self, cr, x, y, w, h, alpha):
        if self.mpris.has_media():
            art = 44
            ax, ay = x + 9, y + (h - art) / 2
            draw_art(cr, self.mpris.art_pixbuf, ax, ay, art, 12, alpha)

            tx = ax + art + 12
            maxw = w - (tx - x) - (50 if self.mpris.is_playing() else 16)
            draw_text(
                cr,
                self.mpris.get_title() or "Unknown",
                tx,
                y + 10,
                13.5,
                (1, 1, 1, 0.95 * alpha),
                Pango.Weight.BOLD,
                maxw,
            )
            draw_text(
                cr,
                self.mpris.get_artist() or "Unknown artist",
                tx,
                y + 32,
                11,
                (1, 1, 1, 0.55 * alpha),
                Pango.Weight.NORMAL,
                maxw,
            )

            # Use real audio levels when playing
            if self.audio_playing and hasattr(self, "audio_levels"):
                draw_real_eq(
                    cr,
                    x + w - 34,
                    y + h / 2 - 8,
                    16,
                    self.audio_levels,
                    COL_ACCENT,
                    alpha,
                )
            else:
                draw_eq(
                    cr,
                    x + w - 34,
                    y + h / 2 - 8,
                    16,
                    time.time(),
                    COL_ACCENT,
                    alpha,
                    self.mpris.is_playing(),
                )
        else:
            txt = time.strftime("%H:%M")
            draw_text(
                cr,
                txt,
                x + w / 2,
                y + (h - 22) / 2,
                16,
                (1, 1, 1, 0.95 * alpha),
                Pango.Weight.BOLD,
                None,
                CLOCK_FONT,
                align_center=True,
            )

    def _compact_sys(self, cr, x, y, w, h, alpha):
        # center clock between left edge and divider (divider at x+106)
        draw_text(
            cr,
            time.strftime("%H:%M"),
            x + 53,
            y + (h - 24) / 2,
            17,
            (1, 1, 1, 0.95 * alpha),
            Pango.Weight.BOLD,
            None,
            CLOCK_FONT,
            align_center=True,
        )

        fill_rounded(cr, x + 106, y + 14, 1, h - 28, 0.5, (1, 1, 1, 0.10 * alpha))

        draw_headphone(
            cr,
            x + 134,
            y + h / 2,
            24,
            (1, 1, 1, (0.85 if self.bluez.connected else 0.35) * alpha),
        )

        tx = x + 156
        maxw = w - (tx - x) - 18
        if self.bluez.connected:
            name = self.bluez.name
            if self.bluez.percentage is not None:
                pct = f"{self.bluez.percentage}%"
            else:
                pct = "—"
        else:
            name = "No headphones"
            pct = "--"
        draw_text(
            cr,
            name,
            tx,
            y + 11,
            10.5,
            (1, 1, 1, 0.52 * alpha),
            Pango.Weight.NORMAL,
            maxw,
        )
        draw_text(
            cr,
            pct,
            tx,
            y + 27,
            16,
            (1, 1, 1, (0.95 if self.bluez.connected else 0.45) * alpha),
            Pango.Weight.BOLD,
            maxw,
        )

    def _compact_tray(self, cr, x, y, w, h, alpha):
        items = self.tray.items
        if not items:
            draw_text(
                cr,
                "No tray items",
                x + w / 2,
                y + (h - 16) / 2,
                11.5,
                (1, 1, 1, 0.45 * alpha),
                align_center=True,
            )
            return
        for ident, rect in self._tray_rects(x, y, w, h, False):
            item = self._tray_item(ident)
            if item is None:
                continue
            ix, iy, iw, ih = rect
            hovered = self.hover and self._inside(self.mouse, rect)
            if hovered:
                fill_rounded(
                    cr, ix - 5, iy - 5, iw + 10, ih + 10, 9, (1, 1, 1, 0.07 * alpha)
                )
            draw_pixbuf(cr, item.pixbuf, ix, iy, iw, alpha)

    # -- expanded views ----------------------------------------------------
    def _music_geometry(self, x, y, w, h):
        art = 150
        ax = x + 28
        ay = y + (h - art) / 2
        tx = ax + art + 26
        cw = x + w - 28 - tx
        cy = y + h - 46
        cxm = tx + cw / 2
        return art, ax, ay, tx, cw, cy, cxm

    def _expanded_music(self, cr, x, y, w, h, t):
        art, ax, ay, tx, cw, cy, cxm = self._music_geometry(x, y, w, h)
        has_media = self.mpris.has_media()
        if has_media:
            draw_art(cr, self.mpris.art_pixbuf, ax, ay, art, 24 * t, t)
        else:
            draw_art(cr, None, ax, ay, art, 24 * t, t)

        title = self.mpris.get_title() if has_media else "Nothing playing"
        artist = (
            self.mpris.get_artist() if has_media else "Start something on your desktop"
        )
        album = self.mpris.get_album() if has_media else ""

        draw_text(cr, title, tx, y + 24, 19, (1, 1, 1, 0.97 * t), Pango.Weight.BOLD, cw)
        draw_text(
            cr, artist, tx, y + 56, 13, (1, 1, 1, 0.58 * t), Pango.Weight.NORMAL, cw
        )
        if album and album != title:
            draw_text(
                cr, album, tx, y + 79, 11, (1, 1, 1, 0.34 * t), Pango.Weight.NORMAL, cw
            )

        py = y + 112
        length = self.mpris.get_length() if has_media else 0
        pos = self.mpris.get_position() if has_media else 0
        frac = max(0.0, min(1.0, pos / length)) if length > 0 else 0.0
        # smoother bar
        bar_h = 6
        bar_r = bar_h / 2
        fill_rounded(
            cr, tx, py + (10 - bar_h) / 2, cw, bar_h, bar_r, (1, 1, 1, 0.16 * t)
        )
        if frac > 0:
            fill_rounded(
                cr,
                tx,
                py + (10 - bar_h) / 2,
                max(bar_h, cw * frac),
                bar_h,
                bar_r,
                (COL_ACCENT[0], COL_ACCENT[1], COL_ACCENT[2], t),
            )
            cr.arc(tx + cw * frac, py + 5, 4.0, 0, 2 * math.pi)
            cr.set_source_rgba(1, 1, 1, t)
            cr.fill()

        draw_text(cr, self._fmt(pos), tx, py + 12, 10.5, (1, 1, 1, 0.45 * t))
        draw_text(
            cr,
            self._fmt(length),
            tx + cw,
            py + 12,
            10.5,
            (1, 1, 1, 0.45 * t),
            align_right=True,
        )

        self._update_hit_regions(x, y, w, h)
        mx, my = self.mouse
        for name, bx in (("prev", cxm - 50), ("play", cxm), ("next", cxm + 50)):
            hx, hy, hr = self.hit.get(name, (bx, cy, 0))
            hovered = self.hover and (mx - hx) ** 2 + (my - hy) ** 2 <= hr * hr
            if name == "play":
                fill_rounded(
                    cr,
                    bx - 18,
                    cy - 18,
                    36,
                    36,
                    18,
                    (1, 1, 1, (0.17 if hovered else 0.11) * t),
                )
            elif hovered:
                fill_rounded(cr, bx - 16, cy - 16, 32, 32, 16, (1, 1, 1, 0.07 * t))

        col = (1, 1, 1, 0.92 * t)
        draw_skip_icon(cr, cxm - 50, cy, 16, -1, col)
        draw_play_icon(cr, cxm, cy, 14, self.mpris.is_playing(), col)
        draw_skip_icon(cr, cxm + 50, cy, 16, 1, col)

    def _expanded_sys(self, cr, x, y, w, h, t):
        draw_text(
            cr,
            time.strftime("%H:%M"),
            x + 36,
            y + 28,
            48,
            (1, 1, 1, 0.97 * t),
            Pango.Weight.BOLD,
            None,
            CLOCK_FONT,
        )
        draw_text(
            cr, time.strftime("%A, %d %B"), x + 38, y + 104, 12, (1, 1, 1, 0.50 * t)
        )
        draw_text(
            cr,
            "Right-click to switch view",
            x + 38,
            y + h - 30,
            10,
            (1, 1, 1, 0.28 * t),
        )

        fill_rounded(cr, x + 226, y + 30, 1, h - 60, 0.5, (1, 1, 1, 0.09 * t))

        rx = x + 250
        draw_headphone(
            cr,
            rx + 19,
            y + 50,
            38,
            (1, 1, 1, (0.9 if self.bluez.connected else 0.35) * t),
        )

        if self.bluez.connected:
            name = self.bluez.name
            sub = "Bluetooth audio"
        else:
            name = "No headphones"
            sub = "Connect a Bluetooth device"
        maxw = x + w - 30 - (rx + 50)
        draw_text(
            cr, name, rx + 50, y + 32, 15, (1, 1, 1, 0.95 * t), Pango.Weight.BOLD, maxw
        )
        draw_text(
            cr, sub, rx + 50, y + 55, 11, (1, 1, 1, 0.46 * t), Pango.Weight.NORMAL, maxw
        )

        if self.bluez.percentage is not None:
            draw_text(
                cr,
                f"{self.bluez.percentage}%",
                rx,
                y + 78,
                32,
                (1, 1, 1, 0.97 * t),
                Pango.Weight.BOLD,
            )
            bar_x = rx
            bar_w = x + w - 30 - bar_x
            fill_rounded(cr, bar_x, y + 132, bar_w, 8, 4, (1, 1, 1, 0.14 * t))
            fill_rounded(
                cr,
                bar_x,
                y + 132,
                max(8, bar_w * self.bluez.percentage / 100.0),
                8,
                4,
                (COL_ACCENT[0], COL_ACCENT[1], COL_ACCENT[2], t),
            )
        elif self.bluez.connected:
            draw_text(cr, "Battery unknown", rx, y + 86, 13, (1, 1, 1, 0.45 * t))
        else:
            draw_text(cr, "--", rx, y + 78, 32, (1, 1, 1, 0.40 * t), Pango.Weight.BOLD)

    def _expanded_tray(self, cr, x, y, w, h, t):
        draw_text(cr, "System tray", x + 30, y + 18, 11, (1, 1, 1, 0.40 * t))
        items = self.tray.items
        if not items:
            draw_text(
                cr,
                "No tray items",
                x + w / 2,
                y + h / 2 - 8,
                13,
                (1, 1, 1, 0.45 * t),
                align_center=True,
            )
            return

        for ident, rect in self._tray_rects(x, y, w, h, True):
            item = self._tray_item(ident)
            if item is None:
                continue
            ix, iy, iw, ih = rect
            hovered = self.hover and self._inside(self.mouse, rect)
            fill_rounded(
                cr, ix, iy, iw, ih, 15, (1, 1, 1, (0.11 if hovered else 0.05) * t)
            )
            draw_pixbuf(cr, item.pixbuf, ix + (iw - 34) / 2, iy + (ih - 34) / 2, 34, t)
            draw_text(
                cr,
                item.title or "app",
                ix + iw / 2,
                iy + ih + 8,
                9.5,
                (1, 1, 1, (0.62 if hovered else 0.42) * t),
                maxw=iw + 10,
                align_center=True,
            )

    # -- helpers -----------------------------------------------------------
    def _fmt(self, seconds):
        seconds = max(0, int(seconds))
        return f"{seconds // 60}:{seconds % 60:02d}"

    @staticmethod
    def _inside(pos, rect):
        px, py = pos
        rx, ry, rw, rh = rect
        return rx <= px <= rx + rw and ry <= py <= ry + rh

    def _tray_item(self, ident):
        for item in self.tray.items:
            if item.identifier == ident:
                return item
        return None

    def _tray_rects(self, x, y, w, h, expanded):
        items = [it for it in self.tray.items if it.pixbuf is not None]
        rects = []
        if not items:
            return rects
        if expanded:
            tile = 56
            step = 72
            total = len(items) * step
            start = x + (w - total) / 2 + (step - tile) / 2
            ty = y + (h - 78) / 2
            for i, item in enumerate(items):
                rects.append((item.identifier, (start + i * step, ty, tile, tile)))
        else:
            icon = 26
            step = 38
            total = (len(items) - 1) * step + icon
            start = x + (w - total) / 2
            ty = y + (h - icon) / 2
            for i, item in enumerate(items):
                rects.append((item.identifier, (start + i * step, ty, icon, icon)))
        return rects

    def _update_hit_regions(self, x, y, w, h):
        if not self.expanded or self.view != self.MUSIC:
            self.hit = {}
            return
        art, ax, ay, tx, cw, cy, cxm = self._music_geometry(x, y, w, h)
        self.hit = {
            "prev": (cxm - 50, cy, 14),
            "play": (cxm, cy, 16),
            "next": (cxm + 50, cy, 14),
            "progress": (tx, y + 108, cw, 18),
        }

    def _hit_at(self, px, py):
        for name, val in self.hit.items():
            if name == "progress":
                continue
            bx, by, r = val
            if (px - bx) ** 2 + (py - by) ** 2 <= r * r:
                return name
        return None

    # -- events ------------------------------------------------------------
    def _on_press(self, widget, event):
        if event.button == 3:
            self.press_pos = None
            x, y, w, h = self._pill_rect()
            if self.view == self.TRAY:
                for ident, rect in self._tray_rects(x, y, w, h, self.expanded):
                    if self._inside((event.x, event.y), rect):
                        self.tray.open_menu(ident, event)
                        return True
            direction = -1 if event.x < x + w / 2 else 1
            self.cycle_view(direction)
            return True

        x, y, w, h = self._pill_rect()
        self.press_pos = (event.x, event.y)
        self.click_consumed = False

        if self.view == self.TRAY:
            for ident, rect in self._tray_rects(x, y, w, h, self.expanded):
                if self._inside((event.x, event.y), rect):
                    self.tray.activate(ident)
                    self.click_consumed = True
                    return True

        if self.expanded:
            self._update_hit_regions(x, y, w, h)
            if self.view == self.MUSIC and self.mpris.has_media():
                # 1) Progress bar — приоритет
                pb = self.hit.get("progress")
                if pb:
                    px, pyb, pw, ph = pb
                    if px <= event.x <= px + pw and pyb <= event.y <= pyb + ph:
                        frac = max(0.0, min(1.0, (event.x - px) / pw if pw > 0 else 0))
                        self.mpris.set_position(frac)
                        self.click_consumed = True
                        self.expand_until = 0.0
                        return True

                # 2) Кнопки управления
                hit = self._hit_at(event.x, event.y)
                if hit == "play":
                    self.mpris.play_pause()
                    self.click_consumed = True
                    return True
                elif hit == "prev":
                    self.mpris.previous()
                    self.click_consumed = True
                    return True
                elif hit == "next":
                    self.mpris.next()
                    self.click_consumed = True
                    return True
                # 3) Клик по обложке больше НЕ переключает play/pause
            self.expand_until = 0.0
        return True

    def _on_release(self, widget, event):
        if self.press_pos is None:
            return True
        dx = event.x - self.press_pos[0]
        dy = event.y - self.press_pos[1]
        moved = math.hypot(dx, dy) > 6

        if self.expanded:
            if moved and dy < -40 and abs(dy) > abs(dx):
                self.collapse()
            elif (
                moved
                and abs(dx) > 70
                and abs(dx) > abs(dy) * 1.5
                and self.view == self.MUSIC
            ):
                if dx > 0:
                    self.mpris.previous()
                else:
                    self.mpris.next()
            elif not moved and not self.click_consumed:
                self.collapse()
        else:
            if not self.click_consumed and self._can_expand():
                if not moved or dy > 30:
                    self.expand()

        self.press_pos = None
        return True

    def _on_motion(self, widget, event):
        self.mouse = (event.x, event.y)
        if self.press_pos is None:
            if self.expanded or self.view == self.TRAY:
                self.area.queue_draw()
            return True
        dx = event.x - self.press_pos[0]
        dy = event.y - self.press_pos[1]
        if not self.expanded and self._can_expand() and dy > 34 and abs(dy) > abs(dx):
            self.expand()
        return True

    def _on_enter(self, widget, event):
        self.hover = True
        self.mouse = (event.x, event.y)
        self.area.queue_draw()
        return False

    def _on_leave(self, widget, event):
        self.hover = False
        if self.expanded:
            self.expand_until = time.time() + 0.5
        self.area.queue_draw()
        return False

    def _on_scroll(self, widget, event):
        if self.view != self.MUSIC:
            return False
        if event.direction == Gdk.ScrollDirection.SMOOTH:
            if event.delta_y < 0:
                self.mpris.nudge_volume(0.05)
            elif event.delta_y > 0:
                self.mpris.nudge_volume(-0.05)
            else:
                return False
        elif event.direction == Gdk.ScrollDirection.UP:
            self.mpris.nudge_volume(0.05)
        elif event.direction == Gdk.ScrollDirection.DOWN:
            self.mpris.nudge_volume(-0.05)
        else:
            return False
        self.area.queue_draw()
        return True


def main():
    island = Island()

    def on_destroy(*_):
        island.audio_viz.stop()

    island.connect("destroy", on_destroy)
    GLib.timeout_add(60, lambda: (island._apply_input_region(), False)[1])
    Gtk.main()


if __name__ == "__main__":
    main()
