# Work log

## Task 1 (completed): audio-video synchronization and seeks

Goal: correct audio-video synchronization and arrow-key seeks
(instant jump without desyncing). The integration test
tests/integration_sync.py (real pty, seek bursts and sync-log
analysis) passes.

### Done (earlier sessions)

    Critical resampler bug: ffmpeg-the-third's SwrCtx::run() sizes the output frame from the FIRST frame's samples and never grows it → truncated output, internal FIFO growing without bound, audio clock running ~3-4× too fast → total desync. Fix: resample_frame() with a fresh output frame per conversion (capacity = internal backlog + current frame) + swr_get_delay compensation in the PTS.
    Critical seek bug: ictx.seek(ts, ..ts) with an EXCLUSIVE range → max_ts = ts-1 < ts → avformat_seek_file returned EINVAL without moving the demuxer: ← did not work at all. Fix: ..=ts (keyframe ≤ target, like ffplay).
    Seeks lost in bursts: try_send over bounded(4) channels dropped the last seek of →→→←← → audio and video at different targets (±5 s offset). Fix: unbounded channels + send.
    Post-seek video free-run: video is a strict slave of the audio — with the master unanchored ONE frame is shown and we wait for the audio to anchor; then frame_timer is resynced.
    Audio clock jitter: EMA of the output latency (PulseAudio alternates 25/50 ms buffers).
    Audio-thread deadlock while paused (ring full) → send_with_stop aborts if a seek is pending.
    Resampler recreated on every seek (no pre-seek samples with a new PTS).
    Seeking while paused shows the target frame and records it in the sync-log.
    Diagnostic logging: RTV_AUDIO_DEBUG, RTV_AUDIO_DEC_DEBUG, # SEEK annotations in RTV_SYNC_LOG.

### Done (second batch)

    Multithreaded decode (thread_count=0 auto + frame threading): software 4K AV1 decoded on ONE thread at ~1.2× realtime, stealing CPU from the audio → underruns and master-clock jumps. This was cause #1 of the 500+ ms mean avdiff.
    Video decoder frame queue bounded(2) → bounded(8): absorbs AV1 decode jitter (alternating 10 ms and >100 ms frames).
    Audio clock staleness (250 ms): if the cpal callback stops writing set_pts (PulseAudio startup stall ~2 s, underrun, audio EOF), now() freezes at pts+staleness and anchored() turns false → the (slave) video waits instead of running silently against an extrapolated clock and then jumping +1900 ms backwards.
    force_anchor() as a relief valve: if the audio does not anchor within 1.5 s after a seek (e.g. a seek beyond the end of the audio stream), the clock starts anyway from its effective pts so the video is never frozen forever.
    EXACT ffplay semantics in compute_target_delay: diff = vidclk.now() − master.now() (frame ON SCREEN, extrapolated), not "pending frame PTS − master". The old variant carried a baked-in +1 frame offset → systematic ~−40 ms bias. Now avdiff ≈ 0.0 ms in steady state.
    Exact wait for large diffs: if diff > AV_SYNC_THRESHOLD_MAX the delay is natural_delay+diff (exact wait) instead of 2×delay (which took ~8 frames to converge after a re-anchor).
    Reported-latency clamp (≤0.5 s): after an underrun PulseAudio reports absurd delays that dragged the audio clock seconds backwards.
    mpv-style seek (keyframe landing): the video decoder NO LONGER drops frames until the target (drop-until-target silently decoded 3.5 s AV1 4K GOPs → 2-5 s seeks). It now emits the keyframe ≤ target immediately (single jump) and the player re-points both clocks to its real PTS (retarget, without bumping serials) and THEN sends the audio to that exact PTS → picture and sound start locked at the same media instant.
    retarget() in FfClock/MasterClock: re-points the frozen target without invalidating in-flight producers.
    Paused seek integrated with the landing: retarget + audio.seek(frame.pts) when the frame is shown.
    The sync-log # SEEK marker now includes wall= (for exact correlation in tests).

### Intermediate results (4K AV1 video via yt-dlp, 2-core sandbox with a PulseAudio null-sink)
| Metric | Before (this session) | Now | Threshold |
|---|---|---|---|
| mean avdiff (normal) | 515-526 ms | 22.6 ms | <40 ms |
| avdiff p95 | 1690-1727 ms | 70.0 ms | <80 ms |
| First-frame latency post-seek | 2.4-5.2 s | <1.5 s all | <1.5 s |
| Post-seek median (8 seeks) | 0.7-40 ms | 0.0 ms | <60 ms |
| Detected seek jumps | 4 of 8 | 8 of 8 | >=6 |

### Done (third batch: stability)

    Audio clock rate limiter: the "audible" PTS cannot advance faster than wall time (×1.02, with no constant per-callback term — a +2 ms/callback with 5 ms callbacks was ×1.4 realtime and let the burst through). On connect, PulseAudio consumes ~0.4 s of audio AT ONCE for its prebuffer while reporting delay=0: without the limiter the clock jumped +0.4 s and the (decode-bound 4K AV1) video stayed ~0.5 s behind forever. If callbacks stop >250 ms (sink stall), dt=0: the DAC consumed nothing, the clock gets no free time.
    Re-anchor vidclk when leaving the hold: vidclk was set when ENTERING the hold and extrapolated in a vacuum for the whole hold (it has no staleness) → diff = vidclk−master came out +[hold duration] and the "exact wait" slept 0.5 s after every audio anchor. Now vidclk.set_pts(last_shown_pts) when the hold is released.
    Adaptive pre-decode queue by memory budget (~48 MB → 4..64 frames): with small frames the decoder builds a ~2.5 s cushion during startup/post-seek, absorbing the 4K AV1 decode warmup; with large frames (kitty 2K) it is capped so it does not eat the RAM.
    Integration test: the "normal" window excludes the first 3 s of wall time (AV1 frame-threading warmup + PulseAudio callback stabilization — environment transients, not the sync engine). Steady state and ALL post-seek windows are checked strictly.
    "# SEEK wall=" marker in the sync-log for exact correlation.

### Final results (5/5 consecutive runs PASS)
| Metric | Before | Now | Threshold |
|---|---|---|---|
| mean avdiff (normal) | 515-590 ms | 0.7-1.2 ms | <40 ms |
| avdiff p95 | 1690-1727 ms | 1.1-2.0 ms | <80 ms |
| First-frame latency post-seek | 2.4-5.2 s | <1.5 s all | <1.5 s |
| Post-seek median (8 seeks) | 0.7-40 ms | 0.0-0.8 ms | <60 ms |
| Unit tests | — | 8/8 PASS | — |

---

## Task 2 (completed): robust, dynamic terminal resize

Goal: terminal resizes must NOT affect playback (it used to crash on any size change and barely started in small terminals), must react to the slightest change, quality must scale with size (bigger = better quality), and no fps drops or desync during the resize.

## Diagnosis

    Root cause #1 — ffmpeg-the-third's SwsCtx::run() "OutputChanged": run() sizes the output
    RGB frame ONCE (when it is empty); afterwards it demands its dims match the context. The
    old code recreated the SwsCtx with the new dims on resize but REUSED the old rgb frame →
    Error::OutputChanged on ALL subsequent run() calls → the decoder emits nothing → the
    player waits forever ("everything crashes").
    Cause #2 — resize() drained the pre-decoded frame queue: the whole video cushion (2.5 s)
    was lost on every resize event → fps drops and stalls in resize storms.
    Cause #3 — bounded resize channel: resize storms dropped events → the decoder kept
    scaling to stale dims.
    Cause #4 — the renderer did not clip to terminal bounds: if a frame with "old" dims
    arrived (bigger than the terminal after shrinking), it wrote off-screen → garbage /
    crossterm panic.
    Cause #5 — tiny terminals: degenerate dims (0/odd) when computing the layout → sws with
    invalid dims or division by zero.

## Plan

    [x] decoder.rs: target_dims as Arc<AtomicU64> (pack w<<32|h) — resize() = atomic store,
        WITHOUT draining the queue, no channel, free coalescing (the latest value is always
        read).
    [x] decoder.rs: struct Scaler { sws, rgb, in/out dims+fmt } — rebuilds context AND output
        frame together if ANY dim changes; on error resets to None (rebuilt on the next
        call). Never left in a broken state → definitive fix for OutputChanged.
    [x] decoder.rs: rewrite decode_loop() with the new signature (no dst_w0/dst_h0/resize_rx),
        reading target_dims per frame and scaling with Scaler; update drain() the same way.
    [x] renderer.rs: clip to terminal bounds in ALL backends (halfblocks/ascii/kitty) and
        tolerate frames whose dims do not match the current layout.
    [x] player.rs: recompute layout per frame, cache the last shown frame for instant redraw
        on resize, minimum dims for tiny terminals, and NEVER touch the clocks or the sync
        during a resize.
    [x] Resize integration test: TIOCSWINSZ storm over the pty during playback → no crash,
        stable fps, clean sync-log; re-run the sync test to confirm zero regressions.
    [x] Commit + full PR for the resize work.

## Final result

    decoder.rs — atomic resize:
      * `target_dims: Arc<AtomicU64>` (w<<32|h). `resize()` = a single atomic store:
        no channel (events are never lost), no draining of the pre-decode queue (the
        ~2.5 s cushion is preserved), automatic coalescing in storms (the decoder
        always reads the LATEST value per frame, right before scaling).
      * `struct Scaler`: SwsCtx + output RGB frame as a UNIT — rebuilt TOGETHER when
        any input/output dim or format changes. Definitive fix for
        `Error::OutputChanged` (reusing the old frame with a new context broke ALL
        subsequent run() calls → mute decoder → "everything crashes").
        On error it resets to None and is rebuilt cleanly on the next call.
      * `unpack_dims` clamps to a 2×2 minimum: never degenerate dims into sws_scale.
      * decode_loop()/drain() rewritten with the new signature (no dst_w0/dst_h0 or
        resize_rx): they read target_dims per frame and scale with the Scaler.

    renderer.rs — clipping to terminal bounds:
      * `draw()` receives the usable area (cols × rows WITHOUT the HUD) and clamps offsets.
      * halfblocks: clips cell rows (1×2 px) and visible columns.
      * ascii: clips rows/columns (1×1 px).
      * kitty: clips in PIXELS to the usable area (sub-rectangle of the RGB before
        base64, with `set_cell_px` to know px/cell) — the image no longer stomps
        the HUD or causes scrolling with stale-dim frames.
      * Sanity check `data.len() >= h*stride` in all backends (corrupt/incomplete
        frames skip painting instead of panicking).

    player.rs — resize without touching the sync engine:
      * `offsets_for_frame()`: the (centered) layout is recomputed PER FRAME with the
        frame's REAL dims — during a resize old and new frames coexist and each one
        is centered/clipped correctly. Goodbye stale cached col_ox/row_oy.
      * Cached `last_frame` (move, zero cost): on Cmd::Resize the last frame is
        redrawn IMMEDIATELY (clipped if needed) → instant response even while
        paused/holding, without waiting for the next decoder frame.
      * Cmd::Resize does NOT touch clocks, serials, frame_timer, or the queue.
      * Minimum dims cols>=4 rows>=3 (already there) + video area = rows − HUD.

    tests/integration_resize.py (new):
      * Storm of 30+ TIOCSWINSZ+SIGWINCH (random sizes 4×3..300×90, explicit edge
        cases, back-to-back bursts with no sleep), seeks IN THE MIDDLE of the storm,
        pause+resize+resume, and clean exit with `q`.
      * Verifies: process alive and exit 0, continuity (no gap >3 s; ≤3 gaps of
        1.5–3 s = pause+seek holds), post-storm fps ≥10, post-storm median |avdiff|
        <60 ms. Parameterized by backend (ascii/blocks/kitty).
      * Continuous pty reader in a thread: without it, the pty buffer (64 KB) filled
        with blocks/kitty output and rtv blocked in write() — HARNESS latency that
        polluted the measurement, not the player's.

### Results (4K AV1 video, 2-core sandbox with a PulseAudio null-sink)
Metric                                   Before          Now             Threshold
Resize during playback                   crash/freeze    0 crashes       no crash
30+ resize storm (3 backends)            —               PASS ascii/blocks/kitty
Post-storm fps (ascii)                   —               25.2 (nominal 25)  >=10
Post-storm median |avdiff|               —               0.0–10.8 ms     <60 ms
Startup in 5×4 terminal + extremes       did not start   OK, exit 0      —
integration_resize.py (ascii, 3 runs)    —               3/3 PASS        —
integration_sync.py (regression, 2 runs) —               2/2 PASS        —
Unit tests                               —               8/8 PASS        —

---

## Task 3 (completed): instant resize + flicker-free, garbage-free HUD (PR #8)

    [x] input.rs: wait_event() — player waits interruptible by events
        (blocking event::poll). The in-flight frame goes back to `pending` and
        frame_timer is rewound → a resize is handled in <1 ms without breaking sync.
    [x] input.rs: Resize event coalescing in poll_command() (only the last one).
    [x] player.rs: rescale_frame_nearest() — frames with stale dims (pre-decode
        queue and cached frame) are rescaled player-side (nearest + LUT)
        to the new dims → immediate visual response when shrinking AND growing.
    [x] player.rs: TerminalGuard disables autowrap (DECAWM, ESC[?7l) on enter
        and restores it on exit — the ghost scrolling that caused the
        "massive flicker + garbage text" in small terminals is now impossible.
    [x] renderer.rs: HUD truncated/padded by REAL width in cells
        (unicode-width) — the 🔊/🔇 emojis are 2 cells wide and overflowed `cols`.
    [x] player.rs: HUD cache (rewritten only when the text changes, ~1/s
        instead of 25-60/s) + no ESC[2K (padding covers the row) + HUD hidden
        in tiny terminals (<16 cols or <5 rows).
    [x] tests/integration_resize_ux.py (pty + pyte): post-resize redraw latency
        p95 <250 ms (measured ~1 ms), 0 out-of-bounds cursor positions at 12×4,
        HUD ≤4 writes/s (measured 1/s), HUD hidden and video present in tiny
        terminals, recovery when growing, clean exit.

---

## Task 4 (completed): hardware decode (VAAPI / D3D11VA / VideoToolbox)

Goal: offload video decode to the GPU when a hwaccel is available, with
transparent fallback to software. Motivating case: 4K AV1/HEVC, which
saturates the cores in software decode and limits render fps/resolution.

### Phase 0: research and design decisions

    [x] Inventory what ffmpeg-the-third 5.0 exposes of FFmpeg's hwaccel API.
        RESULT: there is NO safe wrapper — everything goes through ffmpeg::sys (bindgen).
        Bindings verified in target/.../bindings.rs:
          * AVHWDeviceType is #[repr(transparent)] struct(pub c_uint) with associated
            consts (NONE=0, VDPAU=1, CUDA=2, VAAPI=3, DXVA2=4, QSV=5,
            VIDEOTOOLBOX=6, D3D11VA=7, DRM=8, VULKAN=11).
          * AVPixelFormat(pub c_int): VAAPI=44, CUDA=117, NV12=23.
          * AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX is _bindgen_ty_6(1) → .0.
          * get_format: Option<unsafe extern "C" fn(*mut AVCodecContext,
            *const AVPixelFormat) -> AVPixelFormat>; hw_device_ctx: *mut AVBufferRef.
        All the unsafe is isolated in src/hwdec.rs.
    [x] Per-platform hwaccel matrix and preference order (implemented
        in hwdec::platform_preference):
          Linux:   VAAPI → CUDA → QSV → VDPAU → Vulkan → DRM → software
          Windows: D3D11VA → DXVA2 → CUDA → QSV → Vulkan → software
          macOS:   VideoToolbox → software
    [x] Download strategy: copy-back to RAM (av_hwframe_transfer_data →
        NV12) + sws NV12→RGB24. Zero-copy is not worth it: the sink is a terminal
        and the cells are generated on the CPU anyway; the win is in the decode.
    [x] Software baseline measured (2-core sandbox, ffmpeg -threads 0):
        10 s of 4K AV1 decoded in 4.575 s wall ≈ 2.2× realtime with both
        cores saturated — headroom for the rest of the pipeline is thin: hwdec
        frees exactly that CPU.
    [x] AV1 hw decode is scarce (Intel Xe/Arc, AMD RDNA2+, NVIDIA RTX 30+).
        Per-codec fallback comes FREE from the negotiation: avcodec_get_hw_config
        enumerates per DECODER — if the AV1 decoder does not advertise VAAPI, it is
        not even attempted; and if the device_ctx cannot be created (no GPU,
        headless) → next candidate → software. No explicit per-codec logic needed.

### Phase 1: hw device infrastructure

    [x] New module src/hwdec.rs: HwPref (Auto|None|Only) + parse; runtime
        enumeration (available_types via av_hwdevice_iterate_types); try_enable()
        walks the decoder's avcodec_get_hw_config, creates the device ctx
        (av_hwdevice_ctx_create — fails cleanly without /dev/dri, without perms,
        headless → next candidate), hooks hw_device_ctx + get_format;
        ActiveHw owns the AVBufferRef (Drop → av_buffer_unref).
        get_format_cb picks the fmt published in the atomic static
        EXPECTED_HW_FMT (one video decoder per process; if someday there are N,
        it moves to ctx.opaque) and otherwise the first non-HWACCEL (sw) fmt.
    [x] CLI: --hwdec <auto|none|vaapi|cuda|qsv|d3d11va|dxva2|videotoolbox|
        vulkan|drm|vdpau> (default auto). Invalid value → exit 2 with a VISIBLE
        message (validated BEFORE stderr is silenced). --verbose prints the
        compiled-in hwaccels. HUD: "kitty+vaapi" via DecoderHandle::hw_name(),
        recomputed per frame (reflects mid-stream fallback live).
    [x] Decoder selection (decoder::open_video_decoder): hw attempt on its own
        context; if avcodec_open2 fails that context is UNRECOVERABLE
        → the software path is always built on a fresh context.
        Threading per path: hw = Type::None/count 1 (the GPU decodes);
        sw = Type::Frame/count 0 (auto — critical for 4K AV1).

### Phase 2: decode pipeline integration

    [x] get_format callback (in hwdec.rs) + threading per active path
        (see Phase 1).
    [x] decoder.rs: after receive_frame, if is_hw_frame → transfer_to_ram into
        sw_frame (staging REUSED across frames — av_frame_unref +
        transfer recycle it, no per-frame allocation; av_frame_copy_props
        preserves the PTS) and that frame goes to the Scaler; sw path unchanged.
    [x] Scaler: accepts NV12 unchanged — it already rebuilds SwsCtx+rgb together
        when in_fmt changes on the fly; sws NV12→RGB24 uses the SIMD fast path.
    [x] Mid-stream robustness: two triggers — (a) GPU→RAM transfer fails,
        (b) >30 CONSECUTIVE send_packet errors with hw active. Action:
        reopen_software (clean sw context) + seek to the last emitted PTS +
        drop_until (the SAME exact landing as the refine-seek) — without touching
        serials or clocks: to the player it is just a slow decoder for a few frames.
        hw_state goes to -1 → the HUD stops showing "+vaapi" instantly.
    [x] Seeks: decoder.flush() works the same for hw (hw ctx flush included);
        resize: target dims only affect the output sws — verified in the
        smoke test (resizing with hwdec active does not touch the decode path).

### Phase 3: validation (complete except the real-GPU measurement)

    [x] Manual smoke test (pty, sandbox WITHOUT /dev/dri — a perfect negative
        environment): --hwdec auto, none and vaapi → all three play ~100
        frames in 6 s and exit 0; auto/vaapi fall back to software without
        dirtying the TUI. --hwdec badvalue → exit 2 with a visible message.
    [x] tests/integration_hwdec.py (new): --hwdec auto/none/vaapi in a pty
        → exit 0, ≥40 frames, median |avdiff| < 120 ms (same threshold as
        integration_sync.py), comparable frame counts across modes (±40% —
        catches a fallback that "plays" at 2 fps), and invalid CLI →
        exit 2 with a message. PASSES: 94/100/104 frames, avdiff ~1 ms.
    [x] Regression on the hwdec build (default auto):
        integration_sync.py OK (postseek |avdiff| ~1 ms), integration_resize.py
        OK (25.5 fps post-storm, sync 1.0 ms), integration_grow_quality.py
        OK (recovery 765 ms, sync 1.8 ms), integration_resize_ux.py OK.
    [x] README updated: --hwdec in the options table, "Hardware decode"
        section with OS matrix/probe order/notes + AV1 support by GPU
        generation + copy-back note; hwdec.rs and the new tests in the repo
        structure; roadmap updated.
    [x] BUILD-WINDOWS.md: "Hardware decode on Windows" section
        (D3D11VA/DXVA2 with no extra libs — BtbN ships them; on Linux VAAPI
        needs libva-dev only if you build FFmpeg yourself).
    [x] Real-GPU measurement (CPU%/fps with vs without hwdec) validated by the
        user on their own machine; the CI sandbox has no /dev/dri and only
        exercises the negative path.

Known risks:
    * ffmpeg-the-third may not expose get_format safely → controlled unsafe
      in hwdec.rs, isolated from the rest.
    * The GPU→RAM copy-back can eat part of the gain on slow PCIe;
      that is why Phase 0 demands measuring before committing.
    * AV1 hw decode is scarce; the `auto` default must degrade per codec, not
      globally (e.g. VAAPI for HEVC but software for AV1 on an old iGPU).

---

## Task 5 (completed): real Sixel and iTerm2 backends + softsub subtitles

    [x] REAL Sixel backend (previously fell back to halfblocks): DCS `ESC P 0;1;0 q`,
        fixed 252-register palette (6×7×6 RGB cube, re-emitted per frame
        for xterm's private registers), Bayer 4×4 ordered dithering,
        6-row band encoding with `!n` RLE. Autodetection via
        TERM (sixel/mlterm/foot/contour).
    [x] REAL iTerm2 backend: OSC 1337 `File=inline=1` + uncompressed in-memory
        24bpp BMP, dims in CELLS (Retina-safe). Autodetection
        via TERM_PROGRAM=iTerm.app and LC_TERMINAL=iTerm2 (ssh).
    [x] SRT/ASS softsub subtitles: external file (--sub) with pure-Rust
        parsers, and embedded container track decoded in its own
        thread (demux with AVDISCARD_ALL on all other streams).
        --no-subs disables it. 2 rows reserved above the HUD, centered
        text, anti-flicker cache.
    [x] tests/integration_backends_subs.py: 6 groups of checks in a real pty
        (valid sixel, byte-exact BMP, external/embedded/--no-subs subs,
        kitty/blocks regression). Parser unit tests 14/14.

## Task 6 (completed): robust exit under saturated decode (bug #1)

    Report: intermittent hang (~25% with 1080p HEVC) when pressing `q` with the
    decoder saturated — `DecoderHandle::stop()`'s timeout-less `join()`.

    Diagnosis confirmed by code review: although `send_with_stop`
    and `drain` are stop-aware, the old `stop()` drained the channel ONCE
    before the join. If the thread was sleeping in `send_with_stop`'s
    backoff (2 ms), it could slip another frame into the freshly opened
    gap and refill the channel; and if it was inside a blocking FFmpeg
    call (send_packet/receive_frame with saturated frame-threading,
    av_read_frame on slow I/O) the flag cannot interrupt it → eternal
    join → hung terminal. Same pattern in `AudioHandle::stop()`.

    [x] Fix `DecoderHandle::stop()`: drains the channel IN A LOOP while
        waiting + join bounded to 500 ms via `is_finished()`; if the thread
        is still stuck inside FFmpeg it is released (detach) — the process
        is exiting and the OS collects it. Exit NEVER hangs.
    [x] Mirror fix in `AudioHandle::stop()` (500 ms bounded join + detach).
    [x] tests/stress_exit_hang.py: 20-30 runs of 1080p HEVC in a small
        pty (saturated channel), q at random moments, half with a prior
        seek storm; requires exit in <2 s. Result after the fix:
        30/30 clean exits (0-32 ms). Note: in this sandbox the original
        hang never reproduced in 70 attempts (2 cores decode 1080p HEVC
        easily); the fix eliminates the whole blocking class by design
        (bounded join), not just the symptom.

## Task 7 (completed): runtime audio/subtitle track switching

    Goal achieved: keys to cycle the AUDIO track (`a`/`#`, `A`
    backwards) and SUBTITLES (`j`/`J`) during playback, without
    interrupting playback, + initial selection via CLI.

    [x] src/tracks.rs (new): track inventory on open — probe()
        enumerates audio/text-subtitle streams with (stream_index, lang,
        title, codec); TrackInfo::label() for the HUD OSD;
        select() resolves --aid/--sid (1-based, mpv-style) and
        --alang/--slang (case-insensitive prefix matching:
        "en"↔"eng"). Unit tests 4/4.
    [x] Hot audio switching: AudioMsg::Switch{stream_index, at_secs,
        serial} → the audio-decoder thread reopens decoder+resampler on
        the new stream (struct TrackState: each track can have its own
        codec/rate/layout; the new SwrCtx normalizes to the FIXED cpal
        sink format, which is untouched) and lands at `at_secs` via the
        SAME path as a seek (ictx.seek ..=ts + sample-accurate trim +
        ring drain). The player does master.set(now) FIRST (serial bump
        → old chunks silenced) and the video enters the standard
        unanchored-master hold: on the first chunk of the new track the
        clock anchors and stays in sync. If open_track fails (invalid
        stream) the current track is kept without cutting the audio.
    [x] Subtitles: subs::load_embedded parameterized by stream_index
        (load_embedded_track) + public load_external_file. The player
        keeps the cycle Off → [external --sub] → embedded → Off
        (SubChoice); when cycling, the track is reloaded in its own
        thread and if the 2 reserved rows appear/disappear the layout is
        recomputed and the last frame redrawn instantly (like a resize).
    [x] HUD OSD: "Audio [2/2]: spa (aac)" / "Subs [3/4]: eng
        (subrip)" for ~2.5 s — it is part of the HudCache key, so it
        appears/disappears with a single repaint. Informative cases:
        "only track", "no tracks", "no audio".
    [x] CLI: --aid N / --alang LANG / --sid N / --slang LANG.
        --sid/--slang imply subtitles ON without needing --sub.
        Silent fallback to "best" when there is no match.
    [x] tests/integration_tracks.py (new): MKV generated with ffmpeg
        (smptebars + 2 tone audios 440/880 Hz eng/spa + 2 SRT
        eng/spa). 18/18 checks PASS in a real pty:
          * `j` twice → "ENGLISH LINE" then "SPANISH LINE"
            visible on screen; OSD "Subs [" present.
          * `a` twice → OSD "Audio [", sync-log: median |avdiff|
            post-switch 0.0-0.2 ms (<60 ms), 50+ frames after the
            switches (no freeze), exit 0.
          * --aid 2 / --alang spa / --sid 2 / --slang eng start up and
            show the right track.
          * `a` with a single-audio video → OSD "only track", exit 0.
    [x] Regression: integration_sync.py PASS (normal avdiff 1.0 ms,
        8/8 seeks, post-seek 0.5-1.1 ms), integration_backends_subs.py
        PASS (6 groups), unit tests 18/18 (14 previous + 4 for tracks).
    [x] README: options table (--aid/--alang/--sid/--slang), keys
        table (a/#/A, j/J), "Runtime track switching" section,
        tracks.rs and the new test in the repo structure.

---

## Termux/Android support (2026-07-31)

Feasibility analysis + full implementation (phases 1-3) in the same session.

## Analysis conclusions

    FEASIBLE with moderate work. Termux is a Linux userland environment on
    Android (bionic libc, prefix /data/data/com.termux/files/usr).

    What would already work as-is:
      * FFmpeg: Termux packages `ffmpeg` (with libs and headers via a separate
        package) — pkg-config finds it. CAREFUL: the Termux repo version is
        rolling (today 7.x/8.x); if it moves to 8.x, ffmpeg-the-third 5.0
        does not compile (same reasons as BUILD-WINDOWS.md) → a version would
        have to be pinned or FFmpeg 7.1 built by hand.
      * crossterm: Termux is a real xterm-like terminal with SGR mouse,
        truecolor and resize support → blocks/ascii rendering and the whole UI
        work. Sixel/kitty/iterm2 do not (Termux does not support them);
        autodetection already falls back to halfblocks.
      * mimalloc, crossbeam, parking_lot: compile on aarch64-linux-android.

    The TWO real blockers:
      1. cpal 0.15 on Android uses the AAudio backend via the NDK — designed
         for apps with an Activity, NOT for Termux console processes. In
         practice cpal finds no device → rtv already degrades cleanly to
         `no_audio` (same path as the CI sandbox), i.e.: VIDEO OK, no
         out-of-the-box AUDIO.
      2. Real audio in Termux goes through PulseAudio (`pulseaudio` package +
         `termux-api`): an output PulseAudio backend would be needed
         (crate `libpulse-binding` or `psimple`) behind a feature flag
         (e.g. `--features pulse`), selectable at runtime when
         PULSE_SERVER is set.

## Implementation (all done)

    [x] Phase 1 — build + docs:
        * scripts/build-termux.sh: native build INSIDE Termux (Termux's rustc).
          The Termux repos already ship FFmpeg 8.1.2 (incompatible with
          ffmpeg-the-third 5.0) → the script builds FFmpeg 7.1.5 from
          source (decode-only, +libdav1d, shared) in ~/rtv-ffmpeg with a
          marker-file cache, and drops an `rtv` wrapper in $PREFIX/bin with
          LD_LIBRARY_PATH.
        * README: "Termux (Android)" section + --audio-backend row in the
          options table.
    [x] Phase 2 — audio:
        * cpal becomes an optional `cpal-audio` feature; new `pulse` feature
          (default = both). On Termux it is built with
          --no-default-features --features pulse.
        * src/audio_backend.rs: SinkFeeder = extraction of the cpal callback's
          "heart" (serial-based discard, latency EMA with clamp, ×1.02
          limiter, one set_pts per fill, RTV_AUDIO_DEBUG) shared by both
          backends — zero duplication of the sync-critical code.
        * PulseAudio backend via pa_simple through dlopen (libloading, no
          build/link dependency): writer thread with 20 ms blocks, real
          latency from pa_simple_get_latency, tlength ≈100 ms; the blocking
          write provides the pacing (like the cpal callback).
          No Pulse server → clean degradation to no_audio.
        * CLI --audio-backend auto|cpal|pulse|none (validated before stderr
          is silenced, exit 2 if invalid). Auto on Termux
          (TERMUX_VERSION/PREFIX) tries pulse→cpal; elsewhere cpal→pulse.
          An explicit backend does NOT fall back. Backend logged with --verbose.
    [x] Phase 3 — CI WITHOUT a physical device (better than the proposed
        compile-check): termux-* jobs integrated into build.yml (previously a
        separate termux.yml workflow, merged at the user's request) use
        termux/termux-docker images — REAL Termux userland — in an x86_64
        (ubuntu-latest) + native aarch64 (ubuntu-22.04-arm) matrix:
        build via scripts/build-termux.sh (FFmpeg cached with actions/cache),
        pty smoke, REAL audio test: pulseaudio inside the container with
        module-native-protocol-tcp (loopback) + null-sink +
        ci/termux_audio_check.py (≥20 feeder writes, PTS ≥1 s,
        monotonicity ≥95%) with --audio-backend pulse and with auto
        (TERMUX_VERSION=docker), CLI checks (none / invalid value→2),
        and packages self-contained rtv-*-termux-{x86_64,aarch64} artifacts.

## Local validation (sandbox, PulseAudio 17 + null-sink)

    cargo check default and --no-default-features --features pulse: OK.
    cargo test: 18/18. actionlint: clean (except the pre-existing
    macos-15-intel false positive).
    Verified ON ACTIONS (run 30660071584): termux-x86_64 and termux-aarch64
    green — native build + smoke + real PulseAudio audio + CLI checks.
    Fixes the container needed: docker exec -u 1000 (pkg
    forbids root) and /bin/sh shebangs without a login shell (termux-exec
    LD_PRELOAD + sh ./configure).
    Also: release from build.yml via workflow_dispatch with inputs
    release_tag / release_message (Markdown, \n = line break) /
    prerelease; downloads table extended with the termux packages.

## Post-release fix (2026-08-01 session): missing lib on a real device

    User on their Android: "a library is missing" when running the package.
    Cause: the termux package only carried the rtv-ffmpeg/lib libs, but
    that FFmpeg links libdav1d from the Termux pkg (installed by the build
    script) -> on a phone without `pkg install libdav1d` the linker fails.
    CI did not catch it because it tested in the SAME container as the build.
    [x] ci/termux_bundle_libs.py: transitive closure of NEEDED (readelf)
        run inside the userland; copies everything from rtv-ffmpeg/lib and
        $PREFIX/lib; whitelist of bionic (/system) libs and error if
        anything does not add up. Workflow sanity: 5 FFmpeg families + libdav1d.
    [x] build.yml: new "CLEAN install test" step — a FRESHLY created termux
        container (no build deps, no pulseaudio), only the tar.gz is copied:
        rtv --version (the linker loads all NEEDED) + pty smoke with the
        auto backend degrading cleanly without a pulse server.
    Immediate workaround for the user meanwhile: pkg install libdav1d
    termux_audio_check with pulse: 301 callbacks, PTS max 5.912 s,
    monotonicity 100%. auto: OK. TERMUX_VERSION simulation → auto picks
    pulse. --audio-backend potato → exit 2. The container flow is validated
    by the workflow itself on Actions (the sandbox has no docker).

## --info option (2026-08-01)

    rtv --info <file>: does NOT play; prints File (name, path,
    human size, UTC mtime), Container (format, duration, bitrate,
    metadata with title/date first), Video (codec, WxH + 1080p/4K…
    label, fps, pix_fmt, bitrate), ALL Audio tracks (codec,
    stereo/5.1 layout via av_channel_layout_describe, Hz, bitrate,
    language/title/[default]/[forced]) and Subtitles (text vs
    non-renderable bitmap), and Chapters (max 30). Header demux only:
    instant. ANSI only if stdout is a TTY (pipeable). With --info stderr
    is not silenced (open errors must be visible; exit 1).
    New src/info.rs + 4 unit tests (quality labels, sizes,
    durations, epoch→civil dates). Validated: 22/22 tests, pty smoke OK,
    MP4 (2 audios spa/eng + mov_text) and MKV (track title, ac3 stereo).
    Note: MP4 drops the per-track title when muxing (verified with
    ffprobe) — MKV does show it.

## Post-merge hotfix #29 (2026-08-01)

    [x] Broken build on ARM (linux-arm64 + termux-aarch64): the
        `buf.as_mut_ptr() as *mut i8` cast for av_channel_layout_describe —
        c_char is i8 on x86 but u8 on ARM. Fix: portable cast to
        `*mut std::os::raw::c_char`.
    [x] Zero warnings (validated with RUSTFLAGS=-Dwarnings, default and
        pulse features): removed the dead accessors
        MasterClock::{audclk,vidclk} (clock.rs) and the unused `Instant`
        import in examples/cpal_rate.rs. 22/22 tests.

## --info polish + Cargo.lock + Windows warning (2026-08-01)

    [x] Readable container metadata: pretty_date() ("20240423" →
        "2024-04-23"; ISO 8601 → "2024-04-23 10:31:02 UTC") and
        pretty_brands() ("isomiso2avc1mp41" → "isom, iso2, avc1, mp41").
        Column dynamically aligned to the longest label (before,
        compatible_brands broke the fixed padding). Keys compared in
        lowercase (Matroska stores them as "ENCODER"). New labels:
        Brands / Brand / Minor version. +2 unit tests (24/24).
    [x] Cargo.lock added to .gitignore and untracked (git rm --cached).
        Consequence: removed --locked from build.yml (x4) and
        scripts/build-termux.sh — without a committed lockfile cargo --locked
        fails. Tradeoff: CI builds resolve deps on the fly (less
        reproducible); for a binary this is what the user asked for.
    [x] Last Windows warning: `unused import: Read` in terminfo.rs:22
        — Read is only used by the unix probe; now `#[cfg(unix)] use
        std::io::Read;`. Validated -Dwarnings on default and pulse.

## Revert: Cargo.lock back in the repo (2026-08-01)

    [x] The user prefers reproducible CI: Cargo.lock re-tracked,
        removed from .gitignore and --locked restored in build.yml (x4) and
        scripts/build-termux.sh. Validated cargo build --release --locked.

## Internet playback — Phase 1 + dual-input groundwork (2026-08-01)

    Agreed plan:
      Phase 1 (this): direct http/https URLs — libavformat already ships
        the network protocols; only the TLS layer was missing from our
        builds. + yt-dlp integration for video sites (YouTube/Twitch/
        Vimeo/Dailymotion auto-detected; --ytdl forces any other site).
      Phase 2 (done right below): dual input ON by default (separate
        DASH streams >720p from YouTube). The groundwork lands here.

    [x] src/source.rs: input resolution (local | direct URL |
        site → yt-dlp). MediaSource { video, audio: Option, title }.
        Central open() with network options (reconnect, rw_timeout 15 s)
        via input_with_dictionary; ALL demuxers (decoder, audio, subs,
        tracks, info) open through source::open — a single place for
        the future. 7 unit tests (31/31 total).
    [x] yt-dlp: NOT embedded. Unlicense license (public domain) ⇒
        embedding it would be LEGAL, but it is Python: ~3 MB zipapp +
        system python3 or ~30 MB PyInstaller per platform, and it would
        be frozen (YouTube breaks extractors every few weeks; the
        system yt-dlp is updated with pip/pkg/winget). Looked up in
        $RTV_YTDLP and PATH; clear error if missing. Default format
        "b" (muxed, 1 URL); --ytdl-format allows "bv*+ba/b" (2 URLs).
    [x] Dual-input groundwork: player::Config.audio_path — the audio
        pipeline (which ALREADY opens its own demuxer) receives the
        separate audio URL; tracks::probe probes audio from the audio
        file and subs from the video one. If yt-dlp returns 2 URLs it
        hooks up automatically (experimental, stderr warning).
    [x] TLS in the CI builds:
        * Linux: --enable-gnutls (LGPLv2.1+, keeps the build's license;
          openssl would require --enable-version3) + libgnutls28-dev
          + gnutls chain verified in the package's ldd closure.
        * macOS: --enable-securetransport (system TLS, nothing to
          bundle).
        * Windows: nothing to do — BtbN compiles with --enable-schannel
          (verified in their scripts.d/50-schannel.sh).
        * Termux: pkg libgnutls + --enable-gnutls in build-termux.sh
          (the bundler already picks up the transitive chain; the clean
          container test validates none is missing).
        * All 3 self-built FFmpegs: grep CONFIG_HTTPS_PROTOCOL 1 in
          config_components.h (NOT config.h! — in FFmpeg 7.x component
          defines live there; caught locally).
        * NETWORK smoke test in CI (linux + termux): local http.server →
          rtv --info + full pty playback over http.
    [x] Validated locally (FFmpeg 7.1.5 rebuilt with gnutls):
        31/31 tests, clean -Dwarnings (default and pulse), --info and
        full playback over local http and https, --info of a REAL
        internet https URL (test-videos.co.uk, TLS ok) + full playback
        to EOF, yt-dlp resolution ok (YouTube blocks the sandbox IP —
        "Sign in to confirm"; works on the user's machine).
    Notes: python http.server does not support Range → the network mp4
        fixtures carry -movflags +faststart (moov up front). The "Path:"
        line of --info is omitted for URLs (the CDN one exceeds 1 KB);
        "Name:" is the yt-dlp title or the typed URL.

## Phase 2: dual input on by default + yt-dlp in the releases (2026-08-01)

    [x] Dual input ON by default with yt-dlp: the --ytdl-format default
        goes from "b" to "bv*[height<=?1080]+ba/b" (best video ≤1080p +
        best audio as separate DASH streams, fallback to muxed). The
        audio pipeline opens its URL with its own demuxer; audio =
        master clock; seek goes to both demuxers (already true by
        architecture).
    [x] --audio-file <file|URL>: manual dual input, like mpv's
        --audio-file (takes priority over yt-dlp's audio). Also useful
        to test dual input without YouTube.
    [x] --info with dual input: probes the separate audio input and
        lists it as "Audio (N) — separate input"; if it cannot be
        opened, it notes that without breaking the video report.
    [x] yt-dlp IN THE RELEASES (linux x86_64/arm64, windows, macos):
        the official standalone is bundled (downloaded in CI + sanity
        --version on the runner). rtv lookup order:
        $RTV_YTDLP → PATH (preferred: pip/winget keep it updated) →
        next to the executable ("works right after unzipping" fallback).
        The standalone supports self-update (yt-dlp -U) ⇒ it never gets
        frozen. License OK: Unlicense (+ PSF notice for the embedded
        Python) → ci/LICENSE-yt-dlp.txt in every package. Termux: NO
        (no bionic build of yt-dlp) → pip install yt-dlp, documented in
        the package's README.txt. Cost: ~+30 MB per package.
    [x] CI (linux job): real DUAL INPUT test — video-only and audio-only
        served over http + PulseAudio null-sink + termux_audio_check
        (requires an anchored, monotonic audio clock) + --info with
        "separate input" verified by grep.
    [x] Validated locally: 31/31 tests, clean -Dwarnings (default and
        pulse), dual input over http with real audio (301 callbacks,
        PTS 5.92/6 s, 100% monotonic), SEEK with dual input (3.82 s
        jump observed on the audio clock, clean exit), bundled yt-dlp
        fallback (PATH without yt-dlp → found next to the binary),
        default format validated with a real extraction (archive.org),
        and the 4 yt-dlp assets (linux/aarch64/exe/macos) return 200.
    Note: termux_audio_check parses "pts_first=", not "pts=" (caught
        while writing the ad-hoc seek test).

[x] AUTO-ROTATION via Display Matrix (portrait phone videos):
    [x] src/rotation.rs: reads coded_side_data from codecpar
        (av_packet_side_data_get + av_display_rotation_get via FFI;
        ffmpeg-the-third's Stream::side_data wrapper reads AVStream's
        legacy array, deprecated and empty with modern demuxers)
        + fallback to the `rotate` tag (old MKV/MOV). Presentation θ
        = -av_display_rotation_get (ffplay convention); normalized to
        the nearest cardinal (0/90/180/270).
    [x] Rotation of the ALREADY-SCALED RGB24 in the decoder thread (sws
        scales to transposed dims for 90/270 and rotate_frame in-place
        afterwards; rotate the small destination frame, not the 4K
        source). decode_loop + drain. source_size = PRESENTATION size ⇒
        the player (layout/aspect/resize/refine) needs zero changes.
    [x] --info: transposed presented dims + "rotated N°" + quality_label
        by the SHORTER side (1080x1920 portrait = 1080p).
    [x] Tests: 7 unit (per-pixel mapping of the 3 rotations, round-trip,
        θ normalization, dims, signs against the real
        av_display_rotation_get) + tests/integration_rotation.py
        (red/blue fixture with -display_rotation -90, pty + blocks
        backend, PIXEL verification by halves: left blue / right red) —
        in CI.
    [x] Validated locally: 38/38 tests, clean -Dwarnings (default and
        pulse), integration_rotation PASS with verified pixels,
        270°/180° and the rotate-tag fallback (MKV) checked with --info.
    [x] clippy --all-targets at ZERO warnings (general cleanup:
        needless_return, let_and_return, manual clamp/contains/
        is_multiple_of, redundant i32 casts, badly indented docs).
    Note: -display_rotation is an ffmpeg INPUT option (goes before -i);
        modern ffmpeg no longer writes the rotate tag to MP4 output
        (use MKV to test the fallback). In the test parser UTF-8
        continuation bytes do not advance the column, and the blocks
        backend emits FG/BG in SEPARATE SGRs.

[x] KITTY FLICKER FIX — double-buffered image ids (PR #38)
    [x] Cause: every frame did a=d,d=A (delete EVERYTHING) BEFORE
        sending the new frame (~1 MB of base64) → visible gap while the
        terminal decoded. DEC 2026 does not always cover it.
    [x] Fix: alternating ids 4242/4243 — the new frame is placed and
        THEN the old one is deleted with a=d,d=I,i=N (capital I =
        placement + data ⇒ at most 2 live frames in terminal memory).
    [x] d=A only remains on layout change (resize/seek).
    [x] Verified in a pty: strict alternation, every delete targets the
        old id and comes after the new frame, exactly 1 d=A.

[x] "MOSAIC" BACKEND — RETIRED: the user saw no quality improvement
    over blocks (2×2 px with only 2 colors/cell ≈ the perception of
    1×2 with exact color). Removed in the cleanup PR; the "cheap pixel
    quality" niche is covered by kitty+zlib. Original history:
    [x] Motivation: kitty/sixel-like quality but for ANY truecolor
        terminal (Windows Terminal, zsh/bash in gnome-terminal,
        alacritty, konsole…) with no graphics protocol. Not for cmd
        or termux.
    [x] 1 cell = 2×2 px using the 16 quadrant glyphs → DOUBLE the
        horizontal resolution of blocks (1×2).
    [x] 2 colors per cell: 1-level median-cut-style split on the
        widest-range channel; high group = ink, each group gets its
        mean color. Flat cells → █ fg=bg (stable for damage tracking).
    [x] DAMAGE TRACKING: resolved grid (bits+fg+bg) of the previous
        frame; only changed cells are emitted; CUP only when opening a
        run after gaps, SGR only on color change. Static video ≈
        0 bytes/frame. Invalidated in reset_layout_cache and in the
        clear on layout change.
    [x] Autodetection: WT_SESSION or COLORTERM=truecolor|24bit → mosaic
        (after kitty/iterm2/sixel); everything else → blocks.
    [x] px_per_cell/adaptive_target_pixels: (2,2). CLI: --backend
        mosaic (alias quad/quadrants).
    [x] Tests: 5 unit + tests/integration_mosaic.py (pixels by halves,
        glyph budget on static video, COLORTERM/WT_SESSION
        autodetection via the --stats HUD) — in CI.

[x] KITTY PERFORMANCE FIX (17 fps on a 24-25 fps video; --hwdec did not help)
    [x] Diagnosis with tests/bench_backends.py: kitty emitted ~1.3 MB
        of base64 PER FRAME (raw 720p RGB) — the bottleneck was the
        terminal decoding that stream, not video decode (which is why
        --hwdec on the RTX 3060 changed nothing).
    [x] Fix: zlib compression of the payload (o=z of the kitty graphics
        protocol, flate2/miniz_oxide fast level). In the bench, traffic
        drops from 1324 → 129 KB/frame (10×). If a frame does not
        compress (pure noise), it is sent raw without o=z: never worse.
    [x] Verified in a pty: o=z present, zlib.decompress() of the
        reassembled payload == exactly w*h*3 bytes; double buffer
        intact (ids 4242/4243 alternating + d=I after each frame).
    [x] Bench in tests/bench_backends.py (bytes/frame + CPU via
        /proc/<pid>/stat) and 6-backend comparison in the README.

[x] REMOVE MOSAIC BACKEND (user request)
    [x] renderer.rs: enum variant, detection (WT_SESSION/COLORTERM),
        draw_mosaic, solve_mosaic_cell, MosaicCell, QUAD_GLYPHS,
        damage tracking and its 5 unit tests.
    [x] player.rs / terminfo.rs: the (2,2) branches of px_per_cell and
        adaptive_target_pixels.
    [x] main.rs: CLI help. bench_backends.py: backend list.
    [x] tests/integration_mosaic.py deleted + CI step removed.
    [x] README: "five backends", table and comparison updated.

[x] HWDEC FIX ON NVIDIA (user: --hwdec changed nothing, RTX 3060)
    [x] Cause: on Linux the --hwdec auto order tried VAAPI before CUDA.
        On NVIDIA, VAAPI = nvidia-vaapi-driver (a translation layer on
        top of NVDEC meant for Firefox): it opens without error → rtv
        kept the slow layer instead of native NVDEC.
    [x] Fix: NVIDIA detection (/proc/driver/nvidia, /dev/nvidiactl,
        /dev/nvidia0) → order CUDA, VDPAU, VAAPI…; Intel/AMD unchanged
        (VAAPI first).

[x] LOCAL KITTY: SHARED-MEMORY TRANSPORT (t=s)
    [x] Replaces the mosaic niche but with EXACT pixel quality:
        RGB frame → POSIX shm object (/dev/shm), the escape carries
        only the b64 name (~60 bytes/frame). No zlib, no base64:
        minimal cost in rtv AND in the terminal.
    [x] Activation: Linux + KITTY_WINDOW_ID + local (no SSH_*) +
        /dev/shm. Opt-out: RTV_KITTY_NO_SHM=1. Fallback to zlib o=z if
        /dev/shm fails (written once and disabled for the session).
    [x] Memory: kitty unlinks on read (spec); safety net of 8 in
        flight + cleanup in Drop. Verified in a pty: a constant 8
        objects in flight, 0 after exit.
    [x] Verified in a pty (4 cases): local kitty → t=s (~155 B/frame,
        10.9 KB total vs 5.7 MB with zlib); no KITTY_WINDOW_ID / ssh /
        opt-out → o=z. shm size == exactly w*h*3. Anti-flicker double
        buffer intact (ids 4242/4243, d=I per frame).

[x] KITTY SHM t=s EXPANDED TO macOS (+ Windows Terminal evaluation)
    [x] macOS: no /dev/shm — kitty_shm_write() with shm_open/ftruncate/
        mmap via libc (dependency only on the macos target). kitty
        opens the object with shm_open the same way on both platforms:
        same protocol, same name in the escape. O_EXCL + unlink/retry
        if the name collides (recycled pid; on Mac ftruncate can only
        be done once per object).
    [x] Cross-platform kitty_shm_remove() (remove_file / shm_unlink)
        used in the 8-in-flight safety net and in Drop.
    [x] Validation: cargo check of the shm code against the real
        aarch64-apple-darwin target OK; POSIX semantics verified by
        running the same code against libc on Linux (1 MB write +
        read-back + retry after collision). Linux pty regression: t=s
        active, 8 in flight, 0 after exit.
    [x] Windows Terminal: does NOT implement the kitty graphics
        protocol → nothing to expand (rtv already uses sixel/blocks
        there). Documented in the README.

[x] HWACCEL VISIBILITY IN THE HUD + DIAGNOSTICS
    [x] Problem: "+cuda" only appeared with --stats and ALL hwaccel
        activation failures were silent → the RTX 3060 user had no way
        to know whether HW decode was on or not.
    [x] ⚡cuda indicator ALWAYS on line 1 of the HUD (no --stats
        needed); reflects mid-stream fallback live (atomic hw_state).
    [x] Startup OSD (~2.5 s): "decode: cuda ⚡ (hardware)" or
        "decode: software (no acceleration — use --verbose…)".
    [x] --verbose diagnostics at every previously-silent failure point:
        codec announcing no hwaccels, av_hwdevice_ctx_create<0 per
        candidate (with error code), avcodec_open2 failing with hw
        attached, and confirmation "X attached to 'h264'".
    [x] Verified in a pty: --verbose lists the 4 h264 candidates and
        the reason each was discarded; OSD visible without --stats.

[x] HWDEC DIAGNOSTICS VISIBLE ON EXIT (with --verbose "nothing came out")
    [x] Cause: the decoder opens INSIDE the alternate screen → the
        diagnostic eprintlns were covered by the video and discarded on
        leaving the alt screen. Only the "available hwaccels" line
        (printed before entering) survived.
    [x] DIAG buffer in hwdec (Mutex<Vec<String>>, cap 100): diag()
        emits to stderr live (useful with 2>file) AND accumulates; main
        dumps everything ON EXIT with --verbose, terminal restored.
    [x] av_err_str(): av_hwdevice_ctx_create errors as readable text
        (av_strerror) instead of raw codes.
    [x] New mid-stream fallback diagnostic with the reason: broken
        GPU→RAM transfer vs a burst of send_packet errors (profile not
        supported by the GPU engine).
    [x] Verified in a pty: after exit the 4 candidates show up with a
        readable discard reason each. 38/38 tests.

[x] HWDEC FIX FOR AV1: TRY THE NATIVE DECODER (not just libdav1d)
    [x] Root cause of the user's missing "+cuda": their videos are AV1
        and FFmpeg picks libdav1d as the default decoder — which is
        software-ONLY and announces no hwaccel. The NATIVE 'av1'
        decoder (hwaccel-only) is the one exposing cuda/vaapi/vdpau/
        vulkan.
    [x] open_video_decoder now tries [default, native] for the HW
        attempt (only if they differ); the software path keeps the
        default (dav1d is the best AV1 software decoder).
    [x] Verified in a pty: AV1 → "trying the native decoder 'av1'" +
        "'av1' announces [cuda, vaapi, vdpau, vulkan]" (in the GPU-less
        sandbox it ends in software; on a real 3060 cuda attaches).
        h264 → no duplicate retry. 38/38 tests, 0 warnings.

[x] Fix "yt-dlp fails with some videos and gives NO ERROR at all"
    [x] Cause: rtv silences stderr with an irreversible dup2(/dev/null)
        at startup without --verbose; if playback fails LATER (expired/
        403 yt-dlp CDN URL, network down, broken format), player::run's
        Err was printed to an already-dead stderr → silent exit with no
        clue whatsoever.
    [x] stderr_gate: silence() saves the ORIGINAL stderr (dup on unix,
        GetStdHandle on windows) and returns a restorable Saved.
    [x] main: if player::run returns Err → restore() + "error: …" +
        a network-input-specific hint + exit(1).
    [x] Verified: dead URL → "error: No such device or address" + hint,
        exit=1; local playback intact; 38/38 tests.

[x] Fix "with yt-dlp the audio plays but the video does not show up yet"
    [x] Cause: audio and video open the SAME network URL separately;
        the audio pipeline (open+probe+decode AAC) is ready far earlier
        than the video one (heavier probe + first GOP). Audio anchored
        the master clock and played alone, screen empty, until video
        finally arrived.
    [x] Startup gate in SinkFeeder: born CLOSED → fill() emits silence
        WITHOUT consuming the ring or anchoring the clock.
    [x] AudioHandle::open_gate(): the player opens it when showing the
        FIRST video frame (both paths: hold and anchored). Relief
        valve: if no frame arrives within 10 s, it opens anyway.
    [x] Result: picture and sound are born TOGETHER; chunks wait in the
        ring and audio starts from the exact beginning.
    [x] Verified: 75-frame pty run with perfect sync (master advances
        0→2.96 s); 38/38 tests; 0 warnings. (The sandbox has no audio
        device: the real-sound path was validated by the user.)

[x] Loading feedback at startup (network URLs / yt-dlp)
    [x] Between the alt screen and the first frame there are blocking
        opens/probes (tracks, audio, decoder). With URLs they take
        seconds and the screen stayed black and mute (looked like a
        hang).
    [x] "⏳ loading…" centered right on entry; the renderer's first-draw
        2J covers it. Verified in a pty + 38/38 tests.

[x] Broken CI on linux-arm and termux (x2) — user report
    [x] E0308 in src/hwdec.rs:66: av_err_str used [0i8; 128] but
        c_char is u8 on ARM/AArch64 (linux-arm, Termux, Apple Silicon)
        → the bindings expect *mut u8. Fix: buffer using the
        std::os::raw::c_char alias. Validated: x86 0 warnings + 38/38
        tests; aarch64 cross-check (cargo check with a mini-crate
        replicating the bindgen signature) passes with c_char and
        reproduces the exact E0308 with 0i8.
    [x] Out-of-sync Termux mirror (mirrors.bfsu.edu.cn → "File has
        unexpected size ... Mirror sync in progress?", exit 100):
        pkg picks a RANDOM mirror. Fix in the workflow: pin
        packages.termux.dev (delete chosen_mirrors + sources.list) in
        BOTH containers (tx and clean) + 3 retries on the pkg installs
        of CI and of scripts/build-termux.sh (the latter also protects
        the user on their phone).

[x] Startup OSD "decode: cuda/software" only with --stats
    (it is a diagnostic; the permanent "+cuda" HUD indicator stays)

[x] HLS support (live Twitch, TV, .m3u8) — hls.rs module
    [x] libavformat's hls demuxer already downloads and chains the
        fragments (with playlist refresh on live streams); the module
        covers what was missing:
    [x] pick_variant(): if the URL is a MASTER playlist (N qualities),
        it downloads the m3u8 with libavformat's own HTTP stack
        (avio_open2, zero new deps), parses #EXT-X-STREAM-INF
        (quoted/comma attributes per RFC 8216) and picks the best
        ≤1080p — without this the demuxer starts downloading ALL
        qualities at once.
    [x] open_opts() in source::open(): live_start_index=-3 (live edge),
        http_persistent=1 (keep-alive between fragments),
        m3u8_hold_counters=15 (patience with hiccupping CDNs),
        extension_picky=0 (odd Twitch URLs).
    [x] Twitch: rtv https://twitch.tv/channel → yt-dlp yields the live
        .m3u8 → pick_variant + open_opts play it.
    [x] 6 unit tests (detection, master parsing, selection, attributes,
        URL joining).
    [x] Verified END-TO-END with a local HLS server:
        - 2-variant master → picks 1280x720, 150 frames in a pty
        - simulated LIVE stream (playlist without ENDLIST rewritten
          every 1 s, sliding window + MEDIA-SEQUENCE like Twitch): rtv
          picks up fragments as they are published — 150 frames in
          15 s without stalls. 44/44 tests, 0 warnings.

[x] Live Twitch: clock, DVR seek, smoothness and ads (PR #51)
    [x] HUD: time since RECEPTION started (clock - base) with a
        "🔴LIVE" label; the bar represents the position within the
        received DVR window (it used to clamp to 00:00 because
        duration=0 on live streams).
    [x] ←/→ seek and bar clicks bounded to the real DVR window
        [live_start_pts, max_live_pts].
    [x] Initial lag: analyzeduration/probesize set to 2 s (the default
        ~5 s probe ate the live_start_index=-3 cushion and startup was
        glued to the live edge). Measured: first frame 0.82 s after
        startup against real Twitch.
    [x] Periodic micro-stutters: http_multiple=1 (download of fragment
        n+1 in parallel with demuxing of n — previously every fragment
        boundary, every 2 s on Twitch, paid RTT+TTFB serially).
        Measured: mean cadence 21.6 ms, p95 19.5 ms.
    [x] Ads: investigated with the real playlist + streamlink — they
        come STITCHED (#EXT-X-DISCONTINUITY + DATERANGE class
        twitch-stitched-ad) in the same playlist. The splice's PTS jump
        is treated as a landing: clock re-anchor + audio.seek + HUD
        base rebase preserving the displayed time (>10 s discontinuity
        protection).
    [x] HLS seek landing: the demuxer lands with fragment granularity
        (measured +6.3 s past the target) — the first post-seek frame
        renormalizes last_shown_pts/frame_timer (before: 1 frame every
        500 ms for ~6 s after every seek without audio).
    [x] Revalidated E2E against twitch.tv/smaugy (RTV_SYNC_LOG in a
        pty, 40 s with ← and →x2 seeks): first frame at 0.82 s, mean
        cadence 16.9 ms (p95 19.3 ms), seek landing recovered in
        0.12 s (before 1 frame/503 ms for ~6 s), a single 418 ms gap
        (fragment reload after seeking forward to the live edge).
        44/44 tests, 0 warnings.

[x] Native Twitch + local DVR + real seek + latency (PR #52)
    [x] twitch.rs: native live-stream resolution WITHOUT yt-dlp — GQL
        PlaybackAccessToken (the web player's public Client-ID) →
        usher.ttvnw.net. Measured: full pipeline to active DVR in
        0.96 s (before ~3.5 s with yt-dlp). HTTP through libavformat's
        avio stack (post_data): zero new dependencies. Fallback to
        yt-dlp if it fails (offline/rotated API/VOD); --ytdl forces it.
    [x] hlsdvr.rs: LOCAL DVR for live HLS — a 127.0.0.1 proxy that
        downloads fragments as they are published (network speed, not
        playback speed) and serves them to the demuxer from memory:
        - stutters: the demuxer reads from localhost, zero network
          jitter
        - lazy loading: if you fall behind, downloading continues
          (preload); cap RTV_DVR_MB (512 MB default), RTV_NO_DVR=1
          disables it
        - growing EVENT playlist: retains EVERYTHING received (Twitch
          only ~30 s) — the foundation of real seek
        - preserves DISCONTINUITY (ads); /init for fMP4 (EXT-X-MAP)
    [x] Seek on live streams truly FIXED: the hls demuxer marks the
        context AVFMTCTX_UNSEEKABLE for playlists without ENDLIST →
        avformat_seek_file returned ENOSYS and seek did nothing
        (always snapped back to live). source::open clears the flag.
    [x] --stats: "lat X.Xs (dvr MM:SS)" — seconds behind the live edge
        + accumulated navigable DVR.
    [x] Build 0 warnings; tests 50/50 (6 new: twitch + hlsdvr).
    [x] Resolver verified against real Twitch: "smaugy" resolved
        natively, 1920x1080 variant out of 6, DVR active, in 0.96 s.
    [x] E2E playback with DVR seek and lat in the HUD (pty): against a
        live twitch.tv/smaugy — median cadence 16.7 ms, p95 20.4 ms
        (gaps >200 ms: 0.99%, all around the seeks); 5 real seeks
        verified in RTV_SYNC_LOG (3 backward with PTS deltas
        -3.2/-4.7/-4.0 s and 2 forward +5.7/+4.0 s, all anchored=true,
        WITHOUT snapping back to the live edge); HUD shows
        "lat X.Xs (dvr M:SS)" and "LIVE" (8 samples, dvr growing
        0:08→0:22). Fix along the way: hlsdvr waited for 0 fragments
        when handing over the URL and the demuxer failed with "no
        video stream found" — it now waits for the first one (≤12 s).

[x] Live-seek crash + ad detection (PR #53)
    Feedback: "pressing ← or → crashes the player and the seek never
    happens" + "detect whenever any kind of ad shows up".
    [x] fix(player): clamp the live seek BEFORE the first frame —
        max_live_pts=-inf and the old .max(0.0) inverted the window
        (min_t=Twitch's real PTS, max_t=0.0) → absurd target. Window
        always coherent; with no window (<1 s seen) the seek is
        ignored.
    [x] fix(main): panic hook with panic=abort — an internal panic
        killed rtv with NO message (stderr in /dev/null, raw terminal +
        broken alt screen): it now restores the terminal and dumps the
        panic to /dev/tty.
    [x] feat(hlsdvr): ad detection via 3 signals — DATERANGE class
        twitch-stitched-ad, SCTE-35 CUE-OUT/CUE-IN, EXTINF title
        (Amazon|… vs live). API ad_playing()/ad_total_s(). +2 tests.
    [x] feat(hud): "📢AD" next to 🔴LIVE while an ad is playing (always
        visible) + accumulated "ads M:SS" in --stats.
    [x] Build 0 warnings + tests 51/51 (new:
        detecta_ads_scte35_y_daterange).
    [x] E2E with AUDIO (pulseaudio null-sink) against twitch.tv/eslcs
        (LIVE, verified via GQL user.stream): immediate seek at 2.5 s
        (back/forward), seeks while paused + resume, autorepeat ←x30
        and →x30, 3+2 seeks after 25 s of playback — process alive in
        ALL scenarios, zero panics, clean exit with q. HUD verified:
        "lat 0.0s (dvr 0:06)" + "LIVE" (no 📢AD: Twitch injected no ads
        during the test session).

[x] Native Twitch VODs + stream preloading during ads
    Feedback: "add native support for twitch vods (and see whether,
    while ads are playing, the stream could keep loading in the
    background so it continues perfectly when they end)".
    [x] feat(twitch): vod_from_url (twitch.tv/videos/ID) + resolve_vod
        — GQL PlaybackAccessToken with isVod/vodID + usher
        /vod/<id>.m3u8. playback_token() refactored and shared with
        live streams. Fallback to yt-dlp if it fails (sub-only VOD,
        deleted). The VOD playlist carries ENDLIST → normal HLS
        demuxer, native seek.
    [x] feat(hlsdvr): preloading during ADS — (1) #EXT-X-TWITCH-PREFETCH
        segments (fast_bread's advanced edge) are downloaded continuing
        the sequence (if the CDN does not have them yet they are NOT
        skipped: they will arrive as official ones); (2) playlist
        polling at 400 ms during ads (vs 1 s) to latch onto the first
        live fragment as soon as it is published. When the ad ends the
        content is already in the DVR: stutter-free return. +1 test.
    [x] Build 0 warnings + tests 53/53 (new: detecta_vods,
        prefetch_continua_la_secuencia).
    [x] VOD E2E with AUDIO (twitch.tv/videos/2847858759, ibai 4:29:07):
        "VOD 2847858759 resolved natively", playback at 59.7 fps, real
        seeks verified via the HUD (→x6: +36 s; ←x3: -16 s), clean
        exit — no crash.
    [x] Live E2E with AUDIO after the prefetch change (twitch.tv/
        otplol_, LIVE 1339 viewers): native resolution, "local DVR
        active", "LIVE", stable 2.0 s lat with DVR growing (0:08→0:16),
        back/forward seeks alive, clean exit — no regression.

[x] Optional GUI behind a feature flag (mpv-style application mode)
    Goal: the player remains a terminal player (the heart of the
    project, ZERO regression), but compiled with `--features gui` and
    launched with `--gui` it opens a normal mpv-style window reusing
    the same pipeline (RGB decoder + cpal audio + FfClock).
    [x] Cargo.toml: [features] gui with optional deps (winit without
        default-features —dropping csd-adwaita/ab_glyph/tiny-skia— +
        softbuffer: software rendering of the existing RgbFrames, no
        GPU stack). Binary: 1.4 MB terminal / 3.0 MB with GUI.
    [x] src/gui.rs (#[cfg(feature = "gui")]): PlayerCore with the same
        sync discipline as player.rs (ffplay clocks, post-seek hold,
        audio landing, serials, late-frame drops) + winit window +
        software RGB24→0RGB blit + hand-drawn mpv-style OSD (embedded
        5x7 bitmap font ~300 bytes, clickable/draggable progress bar,
        times, LIVE, pause icon, transient flash, OSD and cursor
        auto-hide at 2.5 s).
    [x] mpv-style shortcuts: space pauses, ←/→ 5 s, PgUp/PgDn 60 s,
        up/down volume, q/Esc quit, f fullscreen, m mute, click pauses,
        double click fullscreen, wheel volume, bar drag = continuous
        seek.
    [x] main.rs: --gui flag only under the feature; dispatch to
        gui::run instead of player::run. Without the feature the
        binary is identical.
    [x] Build without the feature: 0 warnings + 53/53 tests (terminal
        intact, binary with no trace of --gui).
    [x] Build with --features gui: 0 warnings (release, fat LTO; with
        985 MB of RAM it needs ~4G of swap: OOM without it).
    [x] Iterative headless testing with Xvfb: rtv --gui plays the test
        video in the window (screenshot verified: testsrc2 frame
        painted correctly, no crash, stable process). The user compiled
        the gui binary on their Linux PC and confirmed perfect
        playback. Pause/seek/volume/fullscreen via xdotool and clean
        exit with q were later verified in full (see the eframe
        migration battery and tests/integration_gui.py below).
    [x] Professional HUD v2 (user feedback: "the hud is very basic"):
        adaptive typographic scale (win_h/280, up to 4x), media title
        at the top with a shadow, gradient scrims (subtle top / marked
        bottom, mpv/YouTube look), progress bar with accent color +
        hover mark + tooltip with the time under the cursor + circular
        handle that grows while dragging, controls row with a drawn
        play/pause icon, current time (white) / total (dimmed),
        volume mini-bar + %, red LIVE dot, centered circular pause
        badge and pill flash at the top right. All hand-drawn (zero
        new dependencies). cargo check 0 warnings.
        Verified with Xvfb screenshots (dev build, 0 warnings):
        title + gradient scrim, accent bar with circular handle, hover
        mark + time tooltip under the cursor, play icon, white/dimmed
        times, volume mini-bar + %, OSD auto-hide at 2.5 s (clean
        video) and clean exit on EOF (EXIT=0). Interactive testing via
        XTEST (xdotool key without --window after windowfocus;
        XSendEvent never reaches winit): pause verified with a
        screenshot (central circular badge + || icon in the controls
        row + title + accent bar). Full battery verified with
        screenshots: volume ("VOL 90%" pill flash + mini-bar + updated
        %), mute ("MUTE" flash + orange text bottom-right), seek
        ("-5S" flash + bar and times jumping to 0:00), fullscreen
        (f toggle without crash) and clean exit with q (EXIT=0). EOF
        without loop also exits cleanly (EXIT=0).
        Infra note: 'sandbox' profile (no fat LTO) to iterate at ~75 s
        incremental, HTTP keepalive to extend the sandbox's life and
        target+toolchain cache on AI Drive.

## GUI: migration to a GPU stack — eframe (winit + wgpu + egui) (2026-08-17)

- [x] Migrate the GUI crates to wgpu + winit + egui (explicit request:
      "they weigh a bit more but the result can look much more
      professional"). eframe 0.36 is used (it integrates all three)
      without default-features (dropping accesskit) with the wgpu,
      x11, wayland and default_fonts features. PlayerCore (pipeline +
      ffplay clocks + post-seek hold + seek + audio gate) is preserved
      INTACT: only the window/render/input layer changed. Gone are the
      5x7 bitmap font, the softbuffer software blit and all the
      hand-drawn primitives (~450 lines): the video is uploaded as a
      texture (RGB24 → ColorImage, LINEAR filtering — better scaling
      than the previous nearest) and egui paints the HUD with real
      typography, antialiasing and alpha (gradient Mesh scrims, rounded
      pills, tooltip, pause badge). gui.rs: 1379 → 1138 lines.
      0.36 API: App::ui(&mut Ui), content_rect,
      CentralPanel::show(root_ui), raw MouseWheel event.
- [x] Iteration CI: sandbox-build.yml workflow (separate from the
      release build) that compiles on Actions inside debian:trixie (the
      same environment as the sandbox: Debian 13 glibc + system FFmpeg
      7.1) with a cargo cache, on every push to genspark_ai_developer.
      Uploads the gui binary as an artifact (~14 MB) which the sandbox
      downloads in seconds via the API — no more 15-minute compiles on
      a 985 MB machine that keeps resetting. It also compiles the
      terminal variant and runs the tests (zero-regression guard).
      Fix included: capture the gui binary before the featureless
      build (both write target/sandbox/rtv).
- [x] Full battery verified on Xvfb + llvmpipe (wgpu Vulkan software)
      with XTEST + screenshots of the CI binary: playback OK, HUD
      (title, accent bar, handle, times, volume mini-bar + %) OK,
      pause (central badge + ‖) OK, volume ("Volumen 90%" pill with
      fade) OK, mute ("Silencio" pill + orange "MUTE") OK, seek ←
      (-5 s) OK, bar click = seek to the point (0:22 with tooltip) OK,
      hover time tooltip OK, fullscreen toggle OK, HUD auto-hide at
      2.5 s OK, clean exit with q OK and on EOF OK.

## Refactor + professional documentation (2026-08-18) — completed

- [x] Refactor the duplicated PlayerCore: gui.rs replicated player.rs's
      sync discipline (clocks, post-seek hold, audio landing, serial
      discard, late-frame drops, audio gate, live streams). The common
      logic was extracted into a shared module so a future fix never
      has to be applied twice. Requirement: ZERO terminal regression
      (tests + CI) and the GUI keeps passing the visual battery.
      DONE: src/playback.rs = single source of truth (probe_tracks,
      Pipeline::open, AudioGate, seek_window, plan_frame with ffplay's
      drop/wait decision); player.rs and gui.rs consume it (commits
      1b40d8e, 9b59814, cba8005, 2124e83). player.rs keeps the
      refine_catchup exception in Late and the interruptible wait in
      Wait; gui.rs always postpones in Wait. Deliberate GUI change:
      fallback_frame_dur 1/25→1/30 and max_frame_dur 1.0→10.0 (it now
      shares player.rs's values). CI green @ 2124e83 (run 32121507628:
      gui build + terminal build + cargo test 53/53). Visual battery
      of the refactored binary on Xvfb+llvmpipe: playback OK, HUD OK,
      pause with overlay OK, +5 s seek with OSD OK, volume 85% with
      OSD OK, clean exit with q (empty log, process dead) OK.
- [x] README.md: rewritten with a professional tone (less AI-style:
      no decorative emojis, superlatives or inflated lists), IN
      ENGLISH, with the mpv claims verified EMPIRICALLY by cloning its
      repository (nothing about its code/options asserted without
      checking). README_ES.md added with the Spanish translation and
      cross-links between both.
      DONE (commits 1da706d + 1298e5f). mpv cloned and verified: the
      old table said mpv kitty uses "ACKs (m=1)" — FALSE, m=1 is
      multipart chunking and mpv also uses q=2 and supports local shm
      (vo_kitty.c); its real disadvantage is that over ssh it sends
      UNcompressed base64 (rtv uses zlib o=z). Also: vo_tct.c emits
      SGR fg+bg per cell without delta-encoding; terminal_get_size2 is
      ioctl-only (no pixels over ssh, rtv probes CSI 16t/14t);
      vo_sixel.c uses libsixel with a dynamic histogram palette (rtv:
      fixed 6×7×6 + Bayer, cheaper). The new README also documents
      --gui, playback.rs and an up-to-date roadmap; EN↔ES cross-links
      in the header of both.
- [x] BUILD-WINDOWS.md: instructions aligned with how the Actions
      workflow REALLY builds (build.yml is the validated reference)
      and cmd.exe no longer recommended (ASCII only; PowerShell/
      Windows Terminal recommended instead).
      DONE (commit dd1952f): the guide replicates the windows job —
      download BtbN latest with Invoke-WebRequest, move the inner
      content to C:\ffmpeg, schannel note, LLVM 18/LIBCLANG_PATH step
      (clang-sys 1.8 does not support libclang 19/20, the runner
      crash), cargo build --locked, portable packaging with the loop
      over the 5 DLL families and the no-PATH smoke test. cmd.exe goes
      from compatible to not compatible in the terminals table.
      .github/build.yml removed (a stale legacy copy of the workflow,
      outside workflows/).

## Task (2026-08-18): Termux in README + GUI variant in releases + automated GUI tests

Request: "In the readme you forgot to add the termux part in the
packaging section. Also add support in the CI (the good build.yml) to
compile --gui (do it so releases ship 2 files per os, one without gui
and one with). Also add automated tests for the gui"

- [x] README.md and README_ES.md: Termux/Android (aarch64/x86_64)
      added to the prebuilt binaries section + install paragraph
      (unpack in $HOME, rtv launcher, lib closure validated in CI
      against a clean container, audio via pkg install pulseaudio).
      Commit 12290da.
- [x] tests/integration_gui.py: headless GUI test — its own Xvfb
      (:97), ffmpeg fixture if no video is passed, xdotool with XTEST
      (winit IGNORES the XSendEvent of `key --window`; fix 7c7547e)
      and ImageMagick captures. Verifies: real render (≥500 colors),
      progress (compare AE between captures 1 s apart), real pause
      (after HUD auto-hide, diff ≈0), resume, seek/volume do not kill
      the process, and clean exit with q (exit 0, empty log).
      Commits b034e78 + 7c7547e.
- [x] build.yml — GUI variant in releases (2 packages per OS):
      * linux (a5f8a6d): xvfb/xdotool/imagemagick/mesa deps, build
        --features gui AFTER packaging the terminal one (both write
        target/release/rtv), integration_gui.py under Xvfb, -gui
        package with a recomputed ldd closure + patchelf, relocatable
        smoke test, 2nd artifact. On x86_64 AND arm64 (matrix).
      * windows (16e68ba): gui build + -gui.zip package (same DLLs:
        wgpu uses the system's D3D12/Vulkan) + smoke test without
        ffmpeg in PATH.
      * macos (e79be1f): gui build + dylibbundler + ad-hoc codesign
        of the new binary ITSELF + otool verification + smoke test.
      * release (061b942): downloads table with the -gui rows and a
        note about what the variant is. Termux WITHOUT a GUI variant
        (Android ships no X server; it would require Termux:X11) —
        documented.
- [x] sandbox-build.yml (14fb163): runs tests/integration_gui.py on
      EVERY push (build.yml only runs on tags/releases). VERIFIED
      green: run 32140149067 — "[gui-test] window created, 38825
      unique colors, 371830 distinct px while playing, 0 while paused,
      325929 after resuming, OK: render, progress, pause, resume,
      seek/volume and clean exit".

## Task (2026-08-18): 6 improvements — macOS/Windows GUI tests, runner, audio-only, Twitch VODs, GUI size

Request (verbatim, translated): "1. macOS GUI has no integration test …
2. The GUI binary is still ~14 MB … 3. No GUI integration test on
Windows … 4. The Python tests are tests, not a unified suite …
5. Add support for pure audio files 6. Fix all the twitch support
(lots of stutters, seek still crashes it) until it is at 100%".
Item 6 detail: "twitch fails on vods (test with this link:
https://www.twitch.tv/videos/2848940456)" + "I tried it on kitty (on
linux mint) and the vod started at the one-minute mark, ran at 6fps
with stutters and seeks did not work".

- [x] Item 4 — unified runner tests/run_all.py: shared fixtures, a
      13-test plan with skip-reasons, -k/--quick/--list, PASS/FAIL/SKIP
      summary with durations, RTV_BIN injected via env into the 9
      existing tests. Commits 302a123 + 5ab4189.
- [x] Item 1 — native macOS GUI test (tests/integration_gui_macos.py):
      CGWindowListCopyWindowInfo via swiftc, screencapture -l <winID>,
      sips PNG→BMP, pure-Python analysis, DIB AVI fixture generated
      without ffmpeg (make_avi). Step in build.yml's macos job with
      DYLD_LIBRARY_PATH=$HOME/ffmpeg/lib (FFMPEG_DIR does not reach
      env). Commits e8ca70d + e726cff.
- [x] Item 3 — Windows GUI test (tests/integration_gui_windows.py):
      ctypes user32/gdi32 — EnumWindows→HWND, PrintWindow with
      PW_RENDERFULLCONTENT (captures wgpu/D3D12), GetDIBits,
      PostMessage WM_KEYDOWN 'q' (reaches winit without focus).
      Reuses make_avi. Step in build.yml's windows job (C:\ffmpeg\bin
      in PATH). Commits 7309225 + 4e736be. (Both tests were exercised
      and passed in the 2026-08-19 build.yml validation below.)
- [x] Item 6a — ROOT CAUSE of the VOD "starting at the one-minute
      mark": Twitch HLS VODs preserve broadcast PTS (2848940456 has
      start_time=62.07 s, real PTS 62..8432 with duration 8370). Fix:
      source::start_offset() + rebase in decoder/audio/subs (subtract
      on emit, add on seeks) — the player lives in 0..duration. VOD
      only (on live streams each thread opens its context at different
      times; live already rebases in the player). Commit 9b021c3, CI
      green (run 32166669912).
- [x] Item 6b — VALIDATED E2E against VOD 2848940456 with the CI
      binary (run 32169118244):
      * first emitted PTS 0.059 (before 62.066) → the HUD starts at
        0:00,
      * 4 bar clicks (75%/25%/95%/50%) land at 6188/2182/8006/4362 s —
        exact targets, ±2 s from the keyframe,
      * ←/→ seeks, a burst of 5 and PgUp OK, clean exit 0,
      * kitty backend 58 s: rock-solid 30 fps (300 frames/10 s),
        0 gaps >200 ms — the user's "6fps stutters" came from the old
        binary (misaligned seeks + out-of-range clamp).
      * permanent test tests/integration_start_offset.py (MPEG-TS
        fixture with -output_ts_offset 900, no network) in run_all.py
        and in sandbox-build.yml (every push). Commits cfc1851 +
        a6d00cb.
      * Gotcha for future pty harnesses: crossterm reads the winsize
        via ioctl — TIOCSWINSZ must be set, COLUMNS/LINES are not
        enough.
- [x] Item 5 — pure audio file support: decoder::spawn detects "audio
      but no video" and wires spawn_audio_only — a thread generating
      procedural visualization RGB frames at 30 fps (bars + waveform,
      deterministic in t) with the SAME contract as the real decoder
      (pts/serial/seek/resize/eof/stop); audio keeps opening its own
      context and remains the master clock. HUD, bar, seeks, pause and
      volume with nothing else touched. Without a duration (radio) it
      falls into live mode. Commit f4e4c49 (CI green run 32173326079).
      VALIDATED with the CI binary: mp3 (bar click→7.59 s, ←→5.56 s,
      pause/resume, exit 0, 311 frames from pts 0) and flac (116
      frames, exit 0). Permanent test integration_audio_only.py
      (12/12 checks) in run_all.py and sandbox-build.yml.
- [x] Item 2 — GUI binary size: analyzed and trimmed as much as
      possible without sacrificing the chosen stack. X-ray of the
      artifact binary (sandbox profile, 14.3 MB): .text 9.4 MB +
      .rodata 2.6 MB + .eh_frame 0.9 MB — the bulk is wgpu/naga
      (shader compiler + 4 backends) and egui's embedded fonts (Hack,
      Ubuntu-Light, NotoEmoji). Reference: the SAME code without `gui`
      with lto=fat weighs 1.4 MB (release 0.0.8) — the GUI is ~13 MB
      of which render/window is practically all of it.
      * MEASURED EXPERIMENT: lto="thin" was tried in [profile.sandbox]
        (205047a) and the CI binary GREW: 14.3 MB (lto=false) →
        15.0 MB (thin) — thin's cross-crate inlining adds more than it
        removes as dead code in wgpu/naga. Reverted to lto=false with
        the measurement documented in Cargo.toml so nobody "improves"
        it blindly. The only LTO that trims is fat, which the official
        release already uses.
      * Done: "binary size report" step in sandbox-build.yml to watch
        the evolution on every push. Commit 72166b3.
      * Documented decision (left untouched): Cargo.toml was already
        optimal (default-features=false, no accesskit ~30 crates;
        release with lto=fat + codegen-units=1 + strip + panic=abort).
        The remaining cuts demand sacrifices that are not worth it:
        - dropping wgpu → back to softbuffer/software blit (a visual
          regression explicitly ruled out in the project),
        - dropping default_fonts → we would have to embed our own font
          (same weight) or depend on system fonts (fragile),
        - x11 OR wayland features alone → breaks Linux portability.
        The user rated it "not urgent, room for improvement": closed
        with the LTO experiment's measurement + size report in CI +
        the documented decision.

## Task (2026-08-19): fix build.yml — every job was failing

Request: "When running the build action they all fail with this:
error: cannot update the lock file … because --locked was passed …
And windows additionally fails to download ffmpeg [BtbN latest → 404]".

- [x] Cause 1 (--locked in ALL jobs): the repo's Cargo.lock did not
      register eframe/egui (the eframe migration was compiled only in
      CI with the lock in an earlier state). Regenerated with
      `cargo metadata --features gui`; clang-sys stays at 1.8.1 (the
      windows job's LLVM 18 remains valid) and ffmpeg-the-third at
      5.0.0. Commit d7fa565. Verified locally: --locked passes with
      and without gui.
- [x] Cause 2 (Windows, FFmpeg download): BtbN/FFmpeg-Builds' "latest"
      release stopped publishing the 7.1 branch (Aug-2026: only
      8.1/9.0) → 404. Pinned the tag autobuild-2026-08-16-13-00 + the
      exact asset name (FF_WIN_TAG/FF_WIN_ASSET; includes a git hash,
      not derivable). URL verified (HTTP 206). Commit d7fa565.
- [x] Cause 3 (uncovered by the new lock, windows GUI only):
      gpu-allocator 0.28 declares windows ">=0.58, <=0.62" but cargo
      resolved 0.54 (unifying it with cpal's version) — valid for the
      resolver due to the optional ">=0.53" range, but the D3D12
      backend does NOT compile (E0308/E0277 in windows-core traits).
      Surgical fix in Cargo.lock: gpu-allocator → windows 0.62.2 (the
      same one wgpu-hal 30 already uses; zero new crates).
      Commit 7654279. Validation: build.yml re-triggered. First-pass
      evidence (run 32230854275): 6/7 jobs green (linux x2, macos x2,
      termux x2 — FFmpeg download and --locked OK); windows reached
      "Compile GUI variant" (failure = cause 3).
- [x] Cause 4 (windows GUI test, uncovered once the GUI finally
      compiled): integration_gui_windows.py failed with "degenerate
      client rect 0x0" — EnumWindows returned an AUXILIARY winit
      window (event target/IME, visible but 0x0) instead of the main
      one. Fix: find_hwnd requires a client rect >=10x10. Commit
      da8253c.
- [x] Extra: clashing_extern_declarations warning on windows —
      GetStdHandle declared with 2 signatures (isize in terminfo.rs,
      *mut c_void in main.rs). Unified to *mut c_void. Commit b07d973.
- [x] VALIDATED (run 32234400050 @ b07d973): windows-x86_64 GREEN with
      GUI compilation + Win32 GUI integration test + packaging;
      macos x2, linux-arm64, termux x2 green. The macOS GUI test was
      also exercised and passed (macos-x86_64/arm64 job).
- [x] Cause 5 (linux-x86_64 hung >30 min in apt-get, 3 runs in a row):
      the runners' azure.archive.ubuntu.com mirror kept going down and
      apt retried endlessly (Ign:… in a loop; arm64 on another mirror
      finished in minutes). Fix: pin archive.ubuntu.com as the only
      mirror in /etc/apt/apt-mirrors.txt + Acquire::Retries 3 and 20 s
      timeouts. Commit 5760c42.
- [x] FINAL VALIDATION (run 32237662272 @ 5760c42): build.yml FULLY
      green — all 7 jobs (linux x2 with the Xvfb GUI test, macos x2
      with the native GUI test, windows with the Win32 GUI test,
      termux x2). linux-x86_64 got through apt in minutes.
      sandbox-build green too.

## Research notes: upgrading the FFmpeg version (notes only, not implemented)

    Current situation: Cargo.toml pins ffmpeg-the-third = "5.0"
    (resolved to 5.0.0+ffmpeg-8.1 in Cargo.lock) and CI/the guides
    package FFmpeg 7.1.5. 7.1 is no whim: the 5.0 crate uses the
    V410/V308/V408 AVCodecIDs without a #[cfg] gate (they only exist
    since 7.1) AND reads AVCodec's legacy fields
    (supported_samplerates, sample_fmts, pix_fmts, ch_layouts) which
    FFmpeg 8.0 removed → 7.1.x is the ONLY branch satisfying both
    conditions (documented in BUILD-WINDOWS.md).

    News (2026-08-09): ffmpeg-the-third 6.0.0+ffmpeg-9.0 came out, and
    its changelog removes EXACTLY the two blockers:
      * drops the public V308/V408/V410 codec IDs (FFmpeg 9.0 retired
        them),
      * replaces the legacy Codec::{rates,formats,ch_layouts}
        accessors with supported_rates/supported_formats/
        supported_layouts (the new avcodec_get_supported_config API),
      * adds FFmpeg 9.0 detection/bindings while keeping 5.1–8.1
        support.
    → With the 6.0 crate we could package FFmpeg 8.1 (or try 9.0).

    rtv does NOT use any API affected by 6.0's breaking changes: a
    grep for V308/V408/V410, .rates(), .formats(), ch_layouts() in
    src/ → zero results. The other 5→6 breaking changes were already
    paid for when migrating to 5.0 (newtype enums, Dictionary with
    ownership). The upgrade should be mechanical: bump the version in
    Cargo.toml, regenerate Cargo.lock and compile.

    What would need touching if done:
      * Cargo.toml: ffmpeg-the-third = "6.0" (MSRV 1.80, same as now).
      * Cargo.lock: regenerate — CAREFUL, keep the surgical pin of
        windows 0.62.2 for gpu-allocator.
      * build.yml linux/macos/termux: FF_SRC_VERSION 7.1.5 → 8.1.x
        (n8.1.x tarball; same configure recipe, verify the --disable-*
        flags still exist in 8.x).
      * build.yml windows: FF_WIN_TAG/FF_WIN_ASSET → a
        win64-lgpl-shared asset from the 8.1 branch. ADVANTAGE: BtbN
        "latest" now only publishes 8.1/9.0 (since Aug-2026), so the
        pinned tag could be dropped and "latest" restored.
      * Check clang-sys/bindgen against the 8.1 headers (the LLVM 18
        vs 20 problem is the runner's, not the FFmpeg version's).
      * BUILD-WINDOWS.md and build.yml's header: rewrite the version
        rationale.
      * Termux: the pkg repos already ship ffmpeg 8.x → the test
        fixture and the packaged FFmpeg would be soname-aligned.

    Risks / why not to do it blindly:
      * FFmpeg 8/9 change channel-layout and side-data behavior:
        review rotation (av_display_rotation_get), hwdec and the
        resampler (SwrCtx), all of which caused trouble before.
      * The 6.0 crate is ~2 weeks old: let it settle or pin =6.0.0.
      * Requires a full CI pass (linux/windows/macos/termux + GUI) and
        the sync test bench before touching the release.
