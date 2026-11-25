"""Lightair vs OBS benchmark harness (docs/benchmarks/lightair-vs-obs.md): the same streamer
scenario built and measured in both, through the same obs-websocket v5 requests (OBS's own
server; Lightair's compatible one). Everything it makes goes in target/bench.

  python3 tools/bench/bench.py prepare                  OBS copy, clean OBS settings, media
  ./target/bench/obs-swap.sh in                         clean OBS settings in place (OBS runs only)
  python3 tools/bench/bench.py setup obs|lightair       build the scenario (once)
  python3 tools/bench/bench.py startup obs|lightair N   launch N times: time to window and ready
  python3 tools/bench/bench.py run obs|lightair TAG     idle 60 s, then record 60 s cycling scenes
  python3 tools/bench/bench.py quick obs|lightair TAG   30 s idle (for iterating)
  python3 tools/bench/bench.py profile obs|lightair TAG macOS sample of the main process
  ./target/bench/obs-swap.sh out                        your own OBS settings back
"""
import json, os, re, statistics, subprocess, sys, time

TOOLS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(TOOLS))
B = os.environ.get("LIGHTAIR_BENCH_DIR", os.path.join(ROOT, "target", "bench"))
sys.path.insert(0, TOOLS)
from obsws import ObsWs, Rpc  # noqa: E402

M = os.path.join(B, "media")
LIGHTAIR_APP = os.environ.get("LIGHTAIR_APP", os.path.join(ROOT, "target", "release", "Lightair.app"))
OBS_APP = os.path.join(B, "obs/OBS.app")
LA_WS = os.path.join(B, "la-ws")
PORT = {"obs": 4460, "lightair": 4466}
# Lightair runs with the Keychain off (LIGHTAIR_NO_KEYCHAIN), so nothing asks for it: its remote
# API takes this password, given to it at launch, instead of the one saved in the Keychain.
LA_PASSWORD = os.environ.get("BENCH_PASSWORD", "lightair-bench")
SCENES = ["Starting Soon", "Gameplay", "Desktop", "Just Chatting", "BRB", "Ending"]


def main_display_uuid():
    # (CGDisplayCreateUUIDFromDisplayID moved to ColorSync)
    out = subprocess.run(["swift", "-e", "import CoreGraphics; import ColorSync; let id = CGMainDisplayID(); "
                          "print(CFUUIDCreateString(nil, CGDisplayCreateUUIDFromDisplayID(id).takeRetainedValue())!)"],
                         capture_output=True, text=True).stdout.strip()
    assert out, "couldn't read the main display's UUID"
    return out


def media(name):
    return os.path.join(M, name)


def inputs(display):
    font = {"face": "Helvetica Neue", "size": 64, "flags": 1, "style": "Bold"}
    big = dict(font, size=120)
    return {
        "Background": ("image_source", {"file": media("background.png")}),
        "Overlay Tint": ("color_source_v3", {"color": 0x60000000, "width": 1920, "height": 1080}),
        "Gameplay Video": ("ffmpeg_source", {"local_file": media("gameplay.mp4"), "looping": True, "is_local_file": True}),
        "Display": ("screen_capture", {"type": 0, "display_uuid": display, "display": display, "show_cursor": True}),
        "Webcam": ("ffmpeg_source", {"local_file": media("webcam.mp4"), "looping": True, "is_local_file": True}),
        "Cam Frame": ("image_source", {"file": media("camframe.png")}),
        "Logo": ("image_source", {"file": media("logo.png")}),
        "Alerts": ("browser_source", {"url": "file://" + media("alerts.html"), "width": 1920, "height": 1080,
                                      "fps": 30, "fps_custom": True}),
        "Chat": ("browser_source", {"url": "file://" + media("chat.html"), "width": 400, "height": 800,
                                    "fps": 30, "fps_custom": True}),
        "Title": ("text_ft2_source_v2", {"text": "LIVE: Benchmark Stream", "font": font}),
        "Starting Text": ("text_ft2_source_v2", {"text": "Starting Soon", "font": big}),
        "BRB Text": ("text_ft2_source_v2", {"text": "Be Right Back", "font": big}),
        "Ending Text": ("text_ft2_source_v2", {"text": "Thanks for watching", "font": big}),
        "Music": ("ffmpeg_source", {"local_file": media("music.m4a"), "looping": True, "is_local_file": True}),
        "Mic": ("ffmpeg_source", {"local_file": media("mic.m4a"), "looping": True, "is_local_file": True}),
    }


FULL = (0, 0, 1920, 1080)
# back to front: (input, (x, y, w, h) | None for audio | (x, y) for text)
LAYOUT = {
    "Starting Soon": [("Background", FULL), ("Overlay Tint", FULL), ("Logo", (760, 160, 400, 400)),
                      ("Starting Text", (620, 640)), ("Alerts", FULL), ("Music", None)],
    "Gameplay": [("Gameplay Video", FULL), ("Webcam", (1400, 770, 480, 270)), ("Cam Frame", (1380, 750, 520, 310)),
                 ("Logo", (40, 40, 120, 120)), ("Title", (40, 980)), ("Chat", (1480, 60, 400, 640)),
                 ("Alerts", FULL), ("Mic", None), ("Music", None)],
    "Desktop": [("Display", FULL), ("Webcam", (1400, 770, 480, 270)), ("Cam Frame", (1380, 750, 520, 310)),
                ("Logo", (40, 40, 120, 120)), ("Title", (40, 980)), ("Chat", (1480, 60, 400, 640)),
                ("Alerts", FULL), ("Mic", None), ("Music", None)],
    "Just Chatting": [("Background", FULL), ("Webcam", (80, 140, 1280, 720)), ("Chat", (1420, 140, 440, 800)),
                      ("Title", (80, 900)), ("Alerts", FULL), ("Mic", None), ("Music", None)],
    "BRB": [("Background", FULL), ("Gameplay Video", (1200, 80, 640, 360)), ("Overlay Tint", FULL),
            ("Logo", (760, 160, 400, 400)), ("BRB Text", (600, 640)), ("Music", None)],
    "Ending": [("Background", FULL), ("Logo", (760, 160, 400, 400)), ("Ending Text", (520, 640)), ("Music", None)],
}


def connect(app, wait=60):
    until = time.time() + wait
    last = None
    while time.time() < until:
        try:
            return ObsWs(port=PORT[app], password=password(app), timeout=30)
        except Exception as e:  # not up yet
            last = e
            time.sleep(0.05)
    raise RuntimeError(f"{app}: no websocket ({last})")


def password(app):
    return None if app == "obs" else LA_PASSWORD


def setup(app):
    o = connect(app)
    display = main_display_uuid()
    made = set()
    have = {s["sceneName"] for s in o.req("GetSceneList")["scenes"]}
    for scene in SCENES:
        if scene not in have:
            o.req("CreateScene", {"sceneName": scene})
    for scene in SCENES:
        for name, box in LAYOUT[scene]:
            if name not in made:
                kind, settings = inputs(display)[name]
                r = o.req("CreateInput", {"sceneName": scene, "inputName": name, "inputKind": kind, "inputSettings": settings})
                made.add(name)
                item = r["sceneItemId"]
            else:
                item = o.req("CreateSceneItem", {"sceneName": scene, "sourceName": name})["sceneItemId"]
            if box is None:
                continue
            t = {"positionX": box[0], "positionY": box[1], "alignment": 5}
            if len(box) == 4:
                t.update({"boundsType": "OBS_BOUNDS_STRETCH", "boundsWidth": box[2], "boundsHeight": box[3], "boundsAlignment": 0})
            o.req("SetSceneItemTransform", {"sceneName": scene, "sceneItemId": item, "sceneItemTransform": t})
    o.req("SetInputVolume", {"inputName": "Music", "inputVolumeDb": -20})
    if app == "lightair":
        # recordings into target/bench, never the user's folder
        r = Rpc(port=PORT[app], password=password(app))
        rec = r.call("workspace.get")["outputs"]["recording"]
        rec["folder"] = os.path.join(B, "rec-la")
        r.call("commands.execute", {"commands": [{"SetRecordingSettings": {"settings": rec}}]})
    o.req("SetCurrentProgramScene", {"sceneName": "Gameplay"})
    # anything left from before (OBS's default scene, Lightair's starter one)
    for s in o.req("GetSceneList")["scenes"]:
        if s["sceneName"] not in SCENES:
            o.req("RemoveScene", {"sceneName": s["sceneName"]})
    print(app, "scenes:", [s["sceneName"] for s in o.req("GetSceneList")["scenes"]])


# ------------------------------------------------------------------ processes

def launch(app):
    log = os.path.join(B, f"{app}-stdout.log")
    if app == "obs":
        cmd = ["open", "-n", "-a", OBS_APP, "--args", "--disable-updater", "--disable-shutdown-check",
               "--disable-missing-files-check"]
    else:
        # in the background (never in front of what you're doing), the Keychain off
        env = {"LIGHTAIR_PROJECT": LA_WS, "LIGHTAIR_REMOTE_PORT": str(PORT["lightair"]),
               "LIGHTAIR_LOG": "info,libobs=info", "LIGHTAIR_STARTER": "plain",
               "LIGHTAIR_BACKGROUND": "1", "LIGHTAIR_NO_KEYCHAIN": "1", "LIGHTAIR_REMOTE_PASSWORD": LA_PASSWORD, **{k: v for k, v in os.environ.items() if k.startswith(("LIGHTAIR_TEMP", "LIGHTAIR_GPUI_PREVIEW", "LIGHTAIR_GRAPHICS", "LIGHTAIR_MEDIA"))}}
        cmd = ["open", "-n", "-g"]
        for k, v in env.items():
            cmd += ["--env", f"{k}={v}"]
        cmd += ["--stdout", log, "--stderr", log, LIGHTAIR_APP, "--args", "-ApplePersistenceIgnoreState", "YES"]
    t0 = time.time()
    subprocess.run(cmd, check=True)
    return t0


def main_pid(app):
    pat = "bench/obs/OBS.app/Contents/MacOS/OBS" if app == "obs" else "Lightair.app/Contents/MacOS/lightair"
    for line in subprocess.run(["pgrep", "-f", pat], capture_output=True, text=True).stdout.split():
        pid = int(line)
        if app == "obs":
            return pid
        env = subprocess.run(["ps", "eww", "-p", str(pid)], capture_output=True, text=True).stdout
        if f"LIGHTAIR_PROJECT={LA_WS}" in env:
            return pid
    return None


def window_up(pid):
    r = subprocess.run([os.path.join(B, "winup"), str(pid)], capture_output=True, text=True)
    return r.returncode == 0 and r.stdout.strip() != ""


def tree(pid):
    """pid and its descendants: {pid: (cpu_seconds, rss_kb, command)}"""
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,time=,rss=,comm="], capture_output=True, text=True).stdout
    procs, kids = {}, {}
    for line in out.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        p, pp, t, rss, comm = int(parts[0]), int(parts[1]), parts[2], int(parts[3]), parts[4]
        procs[p] = (cpu_seconds(t), rss, comm)
        kids.setdefault(pp, []).append(p)
    out, todo = {}, [pid]
    while todo:
        p = todo.pop()
        if p in procs:
            out[p] = procs[p]
            todo += kids.get(p, [])
    return out


def system_procs():
    """CPU seconds of system processes that work for whoever renders/encodes."""
    out = subprocess.run(["ps", "-A", "-o", "time=,comm="], capture_output=True, text=True).stdout
    total = {}
    for line in out.splitlines():
        t, _, comm = line.strip().partition(" ")
        name = os.path.basename(comm.strip())
        if name in ("WindowServer", "VTEncoderXPCService", "VTDecoderXPCService", "coreaudiod"):
            total[name] = total.get(name, 0) + cpu_seconds(t)
    return total


def cpu_seconds(t):
    days = 0
    if "-" in t:
        d, t = t.split("-")
        days = int(d)
    parts = [float(x) for x in t.split(":")]
    secs = 0.0
    for p in parts:
        secs = secs * 60 + p
    return days * 86400 + secs


_LIBC = None


def energy(pids):
    """The kernel's energy estimate (nJ) and instructions retired, summed over `pids` (same-user
    processes: proc_pid_rusage RUSAGE_INFO_V6). CPU % counts time, whatever the core and clock:
    light work spread over slow efficiency cores looks big; energy and instructions don't."""
    import ctypes
    global _LIBC
    if _LIBC is None:
        _LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    nj = ins = 0
    for p in pids:
        buf = (ctypes.c_uint64 * 80)()
        if _LIBC.proc_pid_rusage(p, 6, ctypes.byref(buf)) == 0:
            f = buf[2:]  # after the uuid
            ins += f[29]
            nj += f[40]
    return nj, ins


def gpu_util():
    out = subprocess.run(["ioreg", "-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"], capture_output=True, text=True).stdout
    m = re.search(r'"Device Utilization %"=(\d+)', out)
    return int(m.group(1)) if m else None


def quit_app(app, pid):
    t0 = time.time()
    if app == "obs":
        subprocess.run(["osascript", "-e", 'tell application id "com.obsproject.obs-studio" to quit'], capture_output=True)
    else:
        os.kill(pid, 15)
    while time.time() - t0 < 30:
        if not tree(pid):
            break
        time.sleep(0.05)
    else:
        os.kill(pid, 9)
    took = time.time() - t0
    # helpers gone too
    time.sleep(2)
    return took


def wait_ready(app, o, timeout=60):
    """Ready: the scene collection is loaded (all six scenes, one on air)."""
    until = time.time() + timeout
    while time.time() < until:
        try:
            r = o.req("GetSceneList")
            names = {s["sceneName"] for s in r["scenes"]}
            if set(SCENES) <= names and r["currentProgramSceneName"] in SCENES:
                return True
        except Exception:
            pass
        time.sleep(0.02)
    return False


def startup(app, n):
    results = []
    for i in range(n):
        assert main_pid(app) is None, f"{app} already running"
        t0 = launch(app)
        pid, t_window, t_ready = None, None, None
        while time.time() - t0 < 60 and t_window is None:
            pid = pid or main_pid(app)
            if pid and window_up(pid):
                t_window = time.time() - t0
            else:
                time.sleep(0.02)
        o = connect(app)
        t_ws = time.time() - t0
        wait_ready(app, o)
        t_ready = time.time() - t0
        time.sleep(3)
        rss = sum(v[1] for v in tree(pid).values()) / 1024
        t_quit = quit_app(app, pid)
        results.append({"window_s": t_window, "websocket_s": t_ws, "ready_s": t_ready, "rss_mb_at_3s": rss, "quit_s": t_quit})
        print(app, i, json.dumps(results[-1]))
        time.sleep(5)
    keys = results[0].keys()
    summary = {k: statistics.median([r[k] for r in results if r[k] is not None]) for k in keys}
    out = {"app": app, "runs": results, "median": summary}
    json.dump(out, open(os.path.join(B, f"startup-{app}.json"), "w"), indent=1)
    print(app, "median", json.dumps(summary))


def sample(pid, seconds, o, during=None):
    """Per-second samples of the app's process tree, the GPU and system helpers."""
    rows = []
    prev_tree = tree(pid)
    prev_energy = energy(prev_tree)
    prev_sys = system_procs()
    prev_t = time.time()
    start = prev_t
    stats0 = o.req("GetStats")
    k = 0
    while time.time() - start < seconds:
        time.sleep(1)
        if during:
            during(k)
        k += 1
        now = time.time()
        cur = tree(pid)
        cur_energy = energy(cur)
        sys_now = system_procs()
        dt = now - prev_t
        cpu = sum(v[0] - prev_tree.get(p, (v[0],))[0] for p, v in cur.items()) / dt * 100
        main_cpu = (cur[pid][0] - prev_tree[pid][0]) / dt * 100 if pid in cur and pid in prev_tree else None
        sys_cpu = {n: (sys_now.get(n, 0) - prev_sys.get(n, 0)) / dt * 100 for n in sys_now}
        power_mw = (cur_energy[0] - prev_energy[0]) / dt / 1e6
        instr_g = (cur_energy[1] - prev_energy[1]) / dt / 1e9
        rows.append({"t": now - start, "cpu_tree": cpu, "cpu_main": main_cpu, "power_mw": power_mw, "instr_g": instr_g,
                     "rss_mb": sum(v[1] for v in cur.values()) / 1024,
                     "procs": len(cur), "gpu": gpu_util(), **{f"sys_{n}": v for n, v in sys_cpu.items()}})
        prev_tree, prev_sys, prev_t, prev_energy = cur, sys_now, now, cur_energy
    stats1 = o.req("GetStats")
    return rows, stats0, stats1


def summarize(rows):
    keys = [k for k in rows[0] if k not in ("t",)]
    out = {}
    for k in keys:
        vals = [r[k] for r in rows if r.get(k) is not None]
        if vals:
            out[k] = {"mean": statistics.fmean(vals), "p95": sorted(vals)[int(len(vals) * 0.95) - 1], "max": max(vals)}
    return out


def ffprobe(path):
    r = subprocess.run(["ffprobe", "-v", "error", "-count_packets", "-select_streams", "v:0", "-show_entries",
                        "stream=nb_read_packets,avg_frame_rate,width,height:format=duration,size,bit_rate", "-of", "json", path],
                       capture_output=True, text=True)
    return json.loads(r.stdout or "{}")


def run(app, tag):
    assert main_pid(app) is None, f"{app} already running"
    rec_dir = os.path.join(B, "rec-obs" if app == "obs" else "rec-la")
    if app == "lightair":
        # never a recording outside target/bench
        launch(app)
        connect(app)
        r = Rpc(port=PORT[app], password=password(app))
        folder = r.call("workspace.get")["outputs"]["recording"].get("folder")
        quit_app(app, main_pid(app))
        assert folder == rec_dir, f"Lightair records to {folder}, not {rec_dir}: run setup first"
    before = set(os.listdir(rec_dir))
    t0 = launch(app)
    o = connect(app)
    wait_ready(app, o)
    pid = main_pid(app)
    print(app, "ready in", round(time.time() - t0, 2), "s; settling")
    time.sleep(20)
    o.req("SetCurrentProgramScene", {"sceneName": "Gameplay"})
    time.sleep(5)
    idle, s0, s1 = sample(pid, 60, o)
    print(app, "idle", json.dumps({k: round(v["mean"], 1) for k, v in summarize(idle).items()}))
    cycle = ["Just Chatting", "Desktop", "BRB", "Gameplay", "Starting Soon", "Gameplay"]

    def switch(k):
        if k % 10 == 9:
            o.req("SetCurrentProgramScene", {"sceneName": cycle[(k // 10) % len(cycle)]})

    o.req("StartRecord")
    rec, r0, r1 = sample(pid, 60, o, during=switch)
    stop = o.req("StopRecord")
    print(app, "recording", json.dumps({k: round(v["mean"], 1) for k, v in summarize(rec).items()}))
    time.sleep(3)
    files = sorted(set(os.listdir(rec_dir)) - before)
    path = stop.get("outputPath") or (os.path.join(rec_dir, files[-1]) if files else None)
    probe = ffprobe(path) if path and os.path.exists(path) else {}
    t_quit = quit_app(app, pid)
    out = {
        "app": app, "tag": tag,
        "idle": {"summary": summarize(idle), "stats_start": s0, "stats_end": s1, "rows": idle},
        "recording": {"summary": summarize(rec), "stats_start": r0, "stats_end": r1, "rows": rec,
                      "file": path, "probe": probe},
        "quit_s": t_quit,
    }
    json.dump(out, open(os.path.join(B, f"run-{app}-{tag}.json"), "w"), indent=1)
    print(app, "file", path, json.dumps(probe))


if __name__ == "__main__" and sys.argv[1] != "prepare":
    cmd, app = sys.argv[1], sys.argv[2]
    if cmd == "setup":
        setup(app)
    elif cmd == "startup":
        startup(app, int(sys.argv[3]))
    elif cmd == "run":
        run(app, sys.argv[3])


def profile(app, tag, seconds=8):
    """Gameplay on air, settled, then macOS `sample` of the main process."""
    assert main_pid(app) is None
    launch(app)
    o = connect(app)
    wait_ready(app, o)
    time.sleep(10)
    o.req("SetCurrentProgramScene", {"sceneName": "Gameplay"})
    time.sleep(8)
    pid = main_pid(app)
    out = os.path.join(B, f"sample-{app}-{tag}.txt")
    t0 = tree(pid)
    subprocess.run(["sample", str(pid), str(seconds), "-mayDie", "-file", out], capture_output=True)
    t1 = tree(pid)
    cpu = sum(v[0] - t0.get(p, (v[0],))[0] for p, v in t1.items()) / seconds * 100
    print(app, "tree cpu during sample", round(cpu, 1), "%", "status", o.req("GetCurrentProgramScene")["sceneName"])
    quit_app(app, pid)
    return out


if __name__ == "__main__" and sys.argv[1] == "profile":
    profile(sys.argv[2], sys.argv[3])


def quick(app, tag, seconds=30):
    """Gameplay on air: mean CPU, GPU and render time over `seconds` (for iterating)."""
    p = main_pid(app)
    if p:
        quit_app(app, p)
    launch(app)
    o = connect(app)
    wait_ready(app, o)
    time.sleep(12)
    # BENCH_SCENE: another scene on air (Desktop: the display captured)
    o.req("SetCurrentProgramScene", {"sceneName": os.environ.get("BENCH_SCENE", "Gameplay")})
    time.sleep(5)
    pid = main_pid(app)
    rows, s0, s1 = sample(pid, seconds, o)
    sm = summarize(rows)
    wid = subprocess.run([os.path.join(B, "winup"), str(pid)], capture_output=True, text=True).stdout.strip()
    if wid:
        subprocess.run(["screencapture", "-x", "-o", f"-l{wid}", os.path.join(B, f"win-{app}-{tag}.png")])
    out = {k: round(sm[k]["mean"], 2 if k == "instr_g" else 1)
           for k in ("power_mw", "instr_g", "cpu_tree", "cpu_main", "gpu", "rss_mb", "sys_WindowServer") if k in sm}
    out["render_ms"] = round(s1["averageFrameRenderTime"], 2)
    out["lagged"] = s1["renderSkippedFrames"] - s0["renderSkippedFrames"]
    print(app, tag, json.dumps(out))
    quit_app(app, pid)
    json.dump(out, open(os.path.join(B, f"quick-{app}-{tag}.json"), "w"))


if __name__ == "__main__" and sys.argv[1] == "quick":
    quick(sys.argv[2], sys.argv[3])


def prepare():
    """A copy of /Applications/OBS.app, clean OBS settings (no third-party plugins, the benchmark
    profile and an empty scene collection, obs-websocket on 4460), the media, the swap script."""
    os.makedirs(os.path.join(B, "media"), exist_ok=True)
    for d in ("rec-obs", "rec-la"):
        os.makedirs(os.path.join(B, d), exist_ok=True)
    if not os.path.exists(OBS_APP):
        os.makedirs(os.path.dirname(OBS_APP), exist_ok=True)
        subprocess.run(["ditto", "/Applications/OBS.app", OBS_APP], check=True)
    subprocess.run(["swiftc", "-O", os.path.join(TOOLS, "winup.swift"), "-o", os.path.join(B, "winup")], check=True)
    subprocess.run(["sh", os.path.join(TOOLS, "make_media.sh"), os.path.join(B, "media")], check=True)
    for page in ("alerts.html", "chat.html"):
        subprocess.run(["cp", os.path.join(TOOLS, page), os.path.join(B, "media", page)], check=True)
    support = os.path.expanduser("~/Library/Application Support")
    c = os.path.join(B, "obscfg")
    for d in ("basic/profiles/Bench", "basic/scenes", "plugin_config/obs-websocket"):
        os.makedirs(os.path.join(c, d), exist_ok=True)
    files = {
        "global.ini": f"""[General]
Pre31Migrated=true
Pre33.0Migrated=true
MaxLogs=10
EnableAutoUpdates=false
MacOSPermissionsDialogLastShown=1
LastVersion=553648128

[Video]
Renderer=OpenGL

[Locations]
Configuration={support}
SceneCollections={support}
Profiles={support}
PluginManagerSettings={support}
""",
        "user.ini": """[General]
FirstRun=true
ConfirmOnExit=false

[Basic]
Profile=Bench
ProfileDir=Bench
SceneCollection=Bench
SceneCollectionFile=Bench

[BasicWindow]
PreviewEnabled=true
WarnBeforeStoppingRecord=false
""",
        "basic/profiles/Bench/basic.ini": f"""[General]
Name=Bench

[Video]
BaseCX=1920
BaseCY=1080
OutputCX=1920
OutputCY=1080
FPSType=0
FPSCommon=60
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo

[Output]
Mode=Advanced

[AdvOut]
RecType=Standard
RecFilePath={os.path.join(B, "rec-obs")}
RecFormat2=mp4
RecEncoder=com.apple.videotoolbox.videoencoder.ave.avc
RecAudioEncoder=CoreAudio_AAC
RecTracks=1
Track1Bitrate=192
""",
        "basic/profiles/Bench/recordEncoder.json": json.dumps({"bitrate": 12000, "rate_control": "ABR", "profile": "high", "keyint_sec": 2, "bframes": True}),
        "plugin_config/obs-websocket/config.json": json.dumps({"alerts_enabled": False, "auth_required": False, "first_load": False, "server_enabled": True, "server_password": "", "server_port": 4460}),
        "basic/scenes/Bench.json": json.dumps({"name": "Bench", "current_scene": "Scene", "current_program_scene": "Scene",
            "scene_order": [{"name": "Scene"}], "sources": [{"id": "scene", "versioned_id": "scene", "name": "Scene",
            "uuid": "2f7c1b7e-0d3a-4b8e-9a51-1c2d3e4f5a6b", "settings": {"items": [], "id_counter": 0}}],
            "transitions": [], "current_transition": "Fade", "transition_duration": 300}),
    }
    for name, text in files.items():
        open(os.path.join(c, name), "w").write(text)
    swap = os.path.join(B, "obs-swap.sh")
    open(swap, "w").write(f"""#!/bin/sh
# in: the clean benchmark OBS settings in place of yours (yours renamed, never deleted); out: yours back
AS="$HOME/Library/Application Support"
U="$AS/obs-studio"; K="$AS/obs-studio.lightair-bench-backup"
case "$1" in
  in) if [ -d "$K" ]; then echo "already swapped"; exit 0; fi
      mv "$U" "$K" && ln -s "{c}" "$U" && echo swapped-in ;;
  out) if [ -L "$U" ] && [ -d "$K" ]; then rm "$U" && mv "$K" "$U" && echo restored; else echo "nothing to restore"; fi ;;
esac
""")
    os.chmod(swap, 0o755)
    print("prepared", B)


if __name__ == "__main__" and sys.argv[1] == "prepare":
    prepare()


def compare(variants, rounds=2, scenes=("Gameplay", "Desktop"), seconds=30):
    """Lightair under each variant (a tag and its environment), alternating `rounds` times; one
    launch per variant and round, every scene sampled in that session. Results:
    compare-<tag>-<scene>-<round>.json and a summary of the means."""
    results = {}
    for rnd in range(1, rounds + 1):
        for tag, env in variants:
            saved = {k: os.environ.get(k) for k in env}
            os.environ.update({k: v for k, v in env.items() if v is not None})
            for k, v in env.items():
                if v is None:
                    os.environ.pop(k, None)
            try:
                p = main_pid("lightair")
                if p:
                    quit_app("lightair", p)
                launch("lightair")
                o = connect("lightair")
                wait_ready("lightair", o)
                time.sleep(12)
                pid = main_pid("lightair")
                for scene in scenes:
                    o.req("SetCurrentProgramScene", {"sceneName": scene})
                    time.sleep(5)
                    rows, s0, s1 = sample(pid, seconds, o)
                    sm = summarize(rows)
                    out = {k: round(sm[k]["mean"], 2 if k == "instr_g" else 1)
                           for k in ("power_mw", "instr_g", "cpu_tree", "cpu_main", "gpu", "rss_mb", "sys_WindowServer") if k in sm}
                    out["render_ms"] = round(s1["averageFrameRenderTime"], 2)
                    out["lagged"] = s1["renderSkippedFrames"] - s0["renderSkippedFrames"]
                    print(tag, scene, rnd, json.dumps(out), flush=True)
                    json.dump(out, open(os.path.join(B, f"compare-{tag}-{scene}-{rnd}.json"), "w"))
                    results.setdefault((tag, scene), []).append(out)
                quit_app("lightair", pid)
            finally:
                for k, v in saved.items():
                    if v is None:
                        os.environ.pop(k, None)
                    else:
                        os.environ[k] = v
            time.sleep(5)
    for (tag, scene), runs in sorted(results.items(), key=lambda r: (r[0][1], r[0][0])):
        keys = runs[0].keys()
        print("mean", tag, scene, json.dumps({k: round(statistics.mean(r[k] for r in runs), 2) for k in keys}))


# `compare-graphics [rounds]`: libobs's OpenGL renderer against its Metal one (LIGHTAIR_GRAPHICS).
if __name__ == "__main__" and sys.argv[1] == "compare-graphics":
    compare([("opengl", {"LIGHTAIR_GRAPHICS": None}), ("metal", {"LIGHTAIR_GRAPHICS": "metal"})],
            rounds=int(sys.argv[2]) if len(sys.argv) > 2 else 2)
