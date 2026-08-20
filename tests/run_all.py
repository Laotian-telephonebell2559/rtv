#!/usr/bin/env python3
"""Unified runner for the rtv integration suite.

Runs every integration test with a single command and a final
PASS/FAIL/SKIP report. Generates the shared fixtures once (h264 and
hevc video with audio) and honors the given binary.

Usage:
  python3 tests/run_all.py [rtv_path] [-k pattern] [--quick] [--list]

  rtv_path   binary under test (default: target/release/rtv, or $RTV_BIN)
  -k pattern run only the tests whose name contains the pattern
  --quick    skip the slow tests (stress_exit_hang, resize kitty/blocks)
  --list     list the tests and exit

Each test can still be launched individually as always; this runner is
sugar for local development. Requirements: ffmpeg on PATH. The GUI
test skips itself if Xvfb/xdotool/ImageMagick are missing or the
binary wasn't built with --features gui.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def make_fixtures(tmp):
    """Generates the shared videos: h264+aac 30 s and hevc 1080p 8 s."""
    h264 = os.path.join(tmp, "fixture_h264.mp4")
    hevc = os.path.join(tmp, "fixture_hevc.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error",
         "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30",
         "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
         "-t", "30", "-c:v", "libx264", "-preset", "veryfast",
         "-c:a", "aac", "-movflags", "+faststart", h264],
        check=True)
    r = subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error",
         "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30",
         "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=48000",
         "-t", "8", "-c:v", "libx265", "-preset", "ultrafast",
         "-tag:v", "hvc1", "-c:a", "aac", "-movflags", "+faststart", hevc])
    if r.returncode != 0:
        hevc = None  # no libx265: stress_exit_hang gets skipped
    return h264, hevc


def have(*cmds):
    return all(shutil.which(c) for c in cmds)


def gui_capable(rtv):
    """True if the binary supports --gui and a headless X11 stack exists."""
    if not have("Xvfb", "xdotool", "import", "compare"):
        return False
    try:
        out = subprocess.run([rtv, "--help"], capture_output=True,
                             text=True, timeout=10).stdout
        return "--gui" in out
    except Exception:
        return False


def build_plan(rtv, h264, hevc, quick):
    """[(name, argv, env_extra, skip_reason|None)]"""
    py = sys.executable
    t = lambda name: os.path.join(HERE, name + ".py")
    plan = [
        ("integration_sync", [py, t("integration_sync"), h264], {}, None),
        ("integration_resize_ascii",
         [py, t("integration_resize"), h264, "ascii"], {}, None),
        ("integration_resize_blocks",
         [py, t("integration_resize"), h264, "blocks"], {},
         "--quick" if quick else None),
        ("integration_resize_kitty",
         [py, t("integration_resize"), h264, "kitty"], {},
         "--quick" if quick else None),
        ("integration_resize_ux", [py, t("integration_resize_ux"), h264], {}, None),
        ("integration_grow_quality",
         [py, t("integration_grow_quality"), h264], {}, None),
        ("integration_hwdec", [py, t("integration_hwdec"), h264], {}, None),
        ("integration_mouse_seek", [py, t("integration_mouse_seek"), h264], {}, None),
        ("integration_start_offset",
         [py, t("integration_start_offset"), rtv], {}, None),
        ("integration_audio_only",
         [py, t("integration_audio_only"), rtv], {}, None),
        ("integration_rotation", [py, t("integration_rotation"), rtv], {}, None),
        ("integration_backends_subs",
         [py, t("integration_backends_subs"), rtv], {}, None),
        ("integration_tracks", [py, t("integration_tracks")], {}, None),
        ("stress_exit_hang", [py, t("stress_exit_hang")],
         {"STRESS_VIDEO": hevc or ""},
         "--quick" if quick else (None if hevc else "ffmpeg lacks libx265")),
        ("stress_hwdec_bench", [py, t("stress_hwdec_bench"), rtv], {},
         "--quick" if quick else None),
        ("integration_gui", [py, t("integration_gui"), rtv, h264], {},
         None if gui_capable(rtv) else "missing Xvfb/xdotool/ImageMagick or binary built without --features gui"),
    ]
    return plan


def main():
    args = sys.argv[1:]
    quick = "--quick" in args
    do_list = "--list" in args
    pattern = None
    if "-k" in args:
        pattern = args[args.index("-k") + 1]
    pos = [a for a in args if not a.startswith("-")
           and (("-k" not in args) or a != pattern)]
    rtv = os.path.abspath(pos[0]) if pos else (
        os.environ.get("RTV_BIN")
        or os.path.join(HERE, "..", "target", "release", "rtv"))
    rtv = os.path.abspath(rtv)

    if not do_list and not os.path.exists(rtv):
        print(f"error: binary {rtv} does not exist", file=sys.stderr)
        print("build with `cargo build --release` or pass the path / $RTV_BIN",
              file=sys.stderr)
        sys.exit(2)
    if not do_list and not have("ffmpeg"):
        print("error: ffmpeg is required on PATH (fixtures)", file=sys.stderr)
        sys.exit(2)

    tmp = tempfile.mkdtemp(prefix="rtv-suite-")
    h264, hevc = (os.path.join(tmp, "x"), None) if do_list else make_fixtures(tmp)
    plan = build_plan(rtv, h264, hevc, quick)
    if pattern:
        plan = [p for p in plan if pattern in p[0]]

    if do_list:
        for name, _, _, skip in plan:
            print(f"  {name}" + (f"  [skip: {skip}]" if skip else ""))
        return 0

    results = []
    t0 = time.time()
    for name, argv, env_extra, skip in plan:
        if skip:
            print(f"─── {name}: SKIP ({skip})")
            results.append((name, "SKIP", 0.0))
            continue
        print(f"─── {name} ...", flush=True)
        env = dict(os.environ, RTV_BIN=rtv, **env_extra)
        start = time.time()
        try:
            r = subprocess.run(argv, env=env, timeout=900,
                               capture_output=True, text=True)
            ok = r.returncode == 0
        except subprocess.TimeoutExpired:
            ok, r = False, None
        dur = time.time() - start
        status = "PASS" if ok else "FAIL"
        results.append((name, status, dur))
        print(f"    {status} in {dur:.1f}s")
        if not ok:
            tail = ((r.stdout or "") + "\n" + (r.stderr or "")).strip() \
                if r else "(timeout 900s)"
            for line in tail.splitlines()[-15:]:
                print(f"    | {line}")

    print("\n════ SUMMARY " + "═" * 47)
    width = max(len(n) for n, _, _ in results)
    for name, status, dur in results:
        mark = {"PASS": "✓", "FAIL": "✗", "SKIP": "-"}[status]
        print(f"  {mark} {name:<{width}}  {status:<4}  {dur:6.1f}s")
    fails = [n for n, s, _ in results if s == "FAIL"]
    total = time.time() - t0
    print(f"\n  {len(results)} tests, {len(fails)} failures, {total:.0f}s total")
    shutil.rmtree(tmp, ignore_errors=True)
    if fails:
        print("  FAILING: " + ", ".join(fails))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
