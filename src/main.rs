//! rtv — a fast terminal video player.
//!
//! The broad strokes:
//!   * Decoding through FFmpeg via the `ffmpeg-the-third` bindings.
//!   * Real audio output with cpal + swresample, with audio acting as the
//!     master clock.
//!   * Adaptive scaling: we detect the real pixel size of each terminal cell
//!     and pick the target resolution accordingly (bigger cells = sharper).

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod audio;
mod audio_backend;
mod clock;
mod decoder;
#[cfg(feature = "gui")]
mod gui;
mod hls;
mod hlsdvr;
mod hwdec;
mod info;
mod input;
mod playback;
mod player;
mod renderer;
mod rotation;
mod source;
mod subs;
mod terminfo;
mod tracks;
mod twitch;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rtv", version, about = "Terminal video player (Rust)")]
struct Cli {
    /// Video file, direct http/https URL, or a video site page
    /// (YouTube, Twitch, Vimeo…; needs yt-dlp installed).
    path: String,

    /// Don't play: show information about the file (format, duration,
    /// quality, audio/subtitle tracks, chapters…).
    #[arg(long)]
    info: bool,

    /// Force a render backend: kitty | iterm2 | sixel | blocks | ascii
    #[arg(long)]
    backend: Option<String>,

    /// Maximum scale (fraction of the terminal, 0.1..=1.0). Defaults to 1.0
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Loop forever
    #[arg(long)]
    loop_video: bool,

    /// Show performance stats in the HUD
    #[arg(long)]
    stats: bool,

    /// Disable audio (falls back to a monotonic clock)
    #[arg(long)]
    no_audio: bool,

    /// Audio output backend: auto | cpal | pulse | none.
    /// `auto` picks per platform (on Termux it tries pulse first).
    #[arg(long, default_value = "auto")]
    audio_backend: String,

    /// Subtitles. WITHOUT this option no subtitles are shown.
    /// `--sub` (no value) uses the container's embedded text track;
    /// `--sub file.srt|.ass` uses the external file.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "FILE")]
    sub: Option<String>,

    /// Disable subtitles (redundant these days: it's already the
    /// default without --sub; kept for compatibility)
    #[arg(long)]
    no_subs: bool,

    /// Initial audio track by 1-based index among the audio tracks
    /// (`--aid 2` = second track), like mpv.
    #[arg(long, value_name = "N")]
    aid: Option<usize>,

    /// Initial audio track by language ("eng", "spa", "en"...).
    #[arg(long, value_name = "LANG")]
    alang: Option<String>,

    /// Initial embedded subtitle track by 1-based index among the
    /// container's text tracks. Implies subtitles ON.
    #[arg(long, value_name = "N")]
    sid: Option<usize>,

    /// Initial embedded subtitle track by language. Implies
    /// subtitles ON.
    #[arg(long, value_name = "LANG")]
    slang: Option<String>,

    /// Hardware decoding: auto | none | vaapi | cuda | qsv | d3d11va |
    /// dxva2 | videotoolbox | vulkan | drm | vdpau. `auto` tries the
    /// platform's hwaccels and falls back to software if none works.
    #[arg(long, default_value = "auto")]
    hwdec: String,

    /// Force yt-dlp resolution for ANY URL (sites that aren't on the
    /// automatic list but that yt-dlp supports).
    #[arg(long)]
    ytdl: bool,

    /// Format requested from yt-dlp (its -f option syntax). The
    /// default asks for the best video up to 1080p + best audio as
    /// separate streams (dual input), falling back to the best muxed
    /// format. "b" forces muxed (a single connection, up to ~720p on
    /// YouTube).
    #[arg(
        long,
        default_value = "bv*[height<=?1080]+ba/b",
        value_name = "FMT"
    )]
    ytdl_format: String,

    /// Play the AUDIO from another file/URL (dual input), like mpv's
    /// --audio-file. Takes priority over any separate audio stream
    /// returned by yt-dlp.
    #[arg(long, value_name = "FILE|URL")]
    audio_file: Option<String>,

    /// Let FFmpeg and its codecs write to stderr (handy for debugging).
    #[arg(long)]
    verbose: bool,

    /// Open the video in a WINDOW (mpv style) instead of the terminal.
    /// Only available when rtv was built with the `gui` feature
    /// (cargo build --release --features gui).
    #[cfg(feature = "gui")]
    #[arg(long)]
    gui: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // VISIBLE PANICS: the release profile uses panic=abort, and during
    // playback stderr is silenced and the terminal is in raw + alt
    // screen mode -- an internal panic used to kill rtv with NO message
    // and a broken terminal ("rtv crashes and says nothing"). The hook
    // runs before the abort: it restores the terminal and dumps the
    // panic to /dev/tty (the real stderr may be pointing at /dev/null).
    #[cfg(unix)]
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write as _;
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out: Box<dyn std::io::Write> = match std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
        {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stderr()),
        };
        // Release the mouse + show the cursor + leave the alt screen.
        let _ = out.write_all(b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l");
        let _ = writeln!(out, "\nrtv: internal panic (this is a bug, please report it):\n{info}");
    }));

    // Validate --hwdec BEFORE silencing stderr: an invalid value must
    // be visible (exit 2, the usual CLI usage-error convention).
    let hw_pref = match hwdec::HwPref::parse(&cli.hwdec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    // Same for --audio-backend.
    let audio_backend = match audio::BackendPref::parse(&cli.audio_backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // Resolve the input BEFORE silencing stderr: yt-dlp can take a
    // while and fail, and its errors must be visible. For local paths
    // and direct URLs this runs nothing (it just classifies the
    // argument).
    let mut src = match source::resolve(&cli.path, cli.ytdl, &cli.ytdl_format) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };
    // --audio-file wins over yt-dlp's separate audio stream.
    if let Some(af) = &cli.audio_file {
        src.audio = Some(PathBuf::from(af));
    }
    // HLS: if the URL (direct, or the one yt-dlp returned — Twitch
    // hands out the live master .m3u8) is a MASTER playlist, picking
    // ONE variant now keeps the demuxer from starting to download
    // every quality at once. Also done BEFORE silencing stderr: its
    // notices (live HLS, chosen variant) must be visible.
    if let Some(variant) = hls::pick_variant(&src.video) {
        src.video = variant;
    }
    // LIVE HLS → local DVR (hlsdvr.rs): fragments are downloaded ahead
    // of playback (zero network jitter while playing), a growing EVENT
    // playlist (seek across everything received so far) and continuous
    // prefetch if you fall behind the live edge. RTV_NO_DVR=1 turns it
    // off.
    if std::env::var_os("RTV_NO_DVR").is_none() {
        if let Some(local) = hlsdvr::start_if_live(&src.video.to_string_lossy()) {
            src.video = PathBuf::from(local);
        }
    }

    // Silence libav before touching anything else; there are three
    // layers of logging to shut down.
    // With --info stderr is NOT redirected: the process doesn't draw on
    // the terminal, and the user must SEE "couldn't open ..." if it
    // fails (only libav's internal logs are muted).
    // A copy of the ORIGINAL stderr, saved before silencing it: if
    // playback fails (e.g. the CDN URL yt-dlp returned gives 403 or has
    // expired by open time), the error must be VISIBLE. Without this,
    // player::run's Err was printed to a stderr already redirected to
    // /dev/null, so rtv "didn't work with some videos and didn't even
    // show an error".
    let mut saved_stderr: Option<stderr_gate::Saved> = None;
    if !cli.verbose {
        ffmpeg_the_third::util::log::set_level(ffmpeg_the_third::util::log::Level::Quiet);
        unsafe {
            ffmpeg_the_third::sys::av_log_set_level(ffmpeg_the_third::sys::AV_LOG_QUIET);
            ffmpeg_the_third::sys::av_log_set_callback(None);
        }
        if !cli.info {
            saved_stderr = stderr_gate::silence();
        }
    }

    ffmpeg_the_third::init()?;

    // --info: inspection without playback. Exits before touching the
    // terminal (no raw mode, no alt screen): the output is pipeable.
    if cli.info {
        // For URLs the useful "name" is yt-dlp's title or the original
        // URL the user typed (not the mile-long CDN URL).
        let display = if source::is_url(&cli.path) {
            Some(src.title.as_deref().unwrap_or(cli.path.as_str()))
        } else {
            None
        };
        if let Err(e) = info::print_info(&src.video, src.audio.as_deref(), display) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if cli.verbose {
        eprintln!("available hwaccels: {:?}", hwdec::available_types());
    }

    // --sub semantics:
    //   * absent           → NO subtitles (SubMode::Off)
    //   * `--sub` (empty)  → the container's embedded track
    //   * `--sub file`     → external .srt/.ass file
    // `--no-subs` forces Off in every case (compatibility).
    // `--sid/--slang` imply embedded subtitles ON even without `--sub`
    // (it would be absurd to ask for a track and never see it).
    let sub_mode = if cli.no_subs {
        player::SubMode::Off
    } else {
        match cli.sub.as_deref() {
            None => {
                if cli.sid.is_some() || cli.slang.is_some() {
                    player::SubMode::Embedded
                } else {
                    player::SubMode::Off
                }
            }
            Some("") => player::SubMode::Embedded,
            Some(p) => player::SubMode::File(PathBuf::from(p)),
        }
    };

    let player_cfg = player::Config {
        path: src.video,
        audio_path: src.audio,
        forced_backend: cli.backend,
        scale: cli.scale.clamp(0.1, 1.0),
        loop_video: cli.loop_video,
        show_stats: cli.stats,
        no_audio: cli.no_audio,
        audio_backend,
        hw_pref,
        sub_mode,
        aid: cli.aid,
        alang: cli.alang,
        sid: cli.sid,
        slang: cli.slang,
    };

    // --gui (the `gui` feature): an mpv-style window using the SAME
    // pipeline (decoder + audio + clocks). Terminal mode — the heart of
    // rtv — remains the default path and doesn't change at all.
    #[cfg(feature = "gui")]
    let result = if cli.gui {
        // GUI mode doesn't use the terminal: keep stderr usable so
        // window errors are visible, and do NOT touch raw mode.
        gui::run(player_cfg)
    } else {
        player::run(player_cfg)
    };
    #[cfg(not(feature = "gui"))]
    let result = player::run(player_cfg);

    // With --verbose: dump the hwaccel diagnostics ON EXIT, once the
    // terminal has been restored. Live eprintln calls happen INSIDE the
    // alternate screen (the decoder opens after we enter it): the video
    // covers them and leaving the alt screen throws them away — which
    // is why "--verbose only ever printed the available-hwaccels line".
    if cli.verbose {
        let diags = hwdec::take_diagnostics();
        if diags.is_empty() {
            eprintln!("hwdec: no activity (--hwdec none?)");
        } else {
            for d in diags {
                eprintln!("{d}");
            }
        }
    }

    // If playback failed, RESTORE the original stderr before printing
    // the error: with stderr pointed at /dev/null, anyhow's `Err` died
    // silently and "rtv didn't work with some videos and didn't even
    // show an error" (typically: an expired/403 yt-dlp CDN URL).
    if let Err(e) = result {
        if let Some(s) = saved_stderr.take() {
            s.restore();
        }
        eprintln!("error: {e:#}");
        if source::is_url(&cli.path) {
            eprintln!(
                "(network input: the stream URL may have expired or be \
                 geo-restricted; try again or use --verbose to see \
                 FFmpeg's details)"
            );
        }
        std::process::exit(1);
    }
    Ok(())
}

// --- stderr silencing (safety net for logs that slip through) ---
//
// `silence()` redirects stderr to /dev/null (or NUL) and returns a
// `Saved` holding the ORIGINAL stderr so it can be RESTORED if playback
// fails. The silencing used to be irreversible → playback-time errors
// (expired CDN URL, network drop…) died in /dev/null and "rtv didn't
// work with some videos and didn't even show an error".
mod stderr_gate {
    #[cfg(unix)]
    pub struct Saved {
        old_fd: i32,
    }

    #[cfg(unix)]
    pub fn silence() -> Option<Saved> {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn dup(oldfd: i32) -> i32;
            fn dup2(oldfd: i32, newfd: i32) -> i32;
        }
        let f = std::fs::OpenOptions::new().write(true).open("/dev/null").ok()?;
        unsafe {
            let saved = dup(2);
            if saved < 0 {
                return None;
            }
            dup2(f.as_raw_fd(), 2);
            Some(Saved { old_fd: saved })
        }
    }

    #[cfg(unix)]
    impl Saved {
        pub fn restore(self) {
            extern "C" {
                fn dup2(oldfd: i32, newfd: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            unsafe {
                dup2(self.old_fd, 2);
                close(self.old_fd);
            }
        }
    }

    #[cfg(windows)]
    pub struct Saved {
        old_handle: *mut core::ffi::c_void,
    }

    // The handle only travels from main back to itself (saved and
    // restored on the same thread); Send is safe here.
    #[cfg(windows)]
    unsafe impl Send for Saved {}

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> *mut core::ffi::c_void;
        fn SetStdHandle(nStdHandle: u32, handle: *mut core::ffi::c_void) -> i32;
        fn CreateFileA(
            lpFileName: *const u8,
            dwDesiredAccess: u32,
            dwShareMode: u32,
            lpSecurityAttributes: *mut core::ffi::c_void,
            dwCreationDisposition: u32,
            dwFlagsAndAttributes: u32,
            hTemplateFile: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
    }

    #[cfg(windows)]
    const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4;

    #[cfg(windows)]
    pub fn silence() -> Option<Saved> {
        use std::ptr;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_READ: u32 = 0x1;
        const OPEN_EXISTING: u32 = 3;
        const INVALID_HANDLE_VALUE: isize = -1;
        unsafe {
            let old = GetStdHandle(STD_ERROR_HANDLE);
            let h = CreateFileA(
                b"NUL\0".as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if !h.is_null() && h as isize != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_ERROR_HANDLE, h);
                Some(Saved { old_handle: old })
            } else {
                None
            }
        }
    }

    #[cfg(windows)]
    impl Saved {
        pub fn restore(self) {
            unsafe {
                SetStdHandle(STD_ERROR_HANDLE, self.old_handle);
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub struct Saved;

    #[cfg(not(any(unix, windows)))]
    pub fn silence() -> Option<Saved> {
        None
    }

    #[cfg(not(any(unix, windows)))]
    impl Saved {
        pub fn restore(self) {}
    }
}
