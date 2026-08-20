//! Hardware decoding (VAAPI / CUDA / QSV / D3D11VA / DXVA2 /
//! VideoToolbox / Vulkan / DRM) with transparent software fallback.
//!
//! Design notes:
//!
//!   * The ffmpeg-the-third bindings expose no safe wrapper around
//!     the hwaccel API — everything goes through `ffmpeg::sys` (raw
//!     bindgen), isolated in this module so the rest of the code
//!     never touches unsafe.
//!
//!   * Standard FFmpeg flow (doc/examples/hw_decode.c):
//!       1. `avcodec_get_hw_config(codec, i)` — enumerate the
//!          hwaccels the decoder supports with the HW_DEVICE_CTX
//!          method.
//!       2. `av_hwdevice_ctx_create` — open the device (e.g.
//!          /dev/dri/renderD128 for VAAPI). If it fails (no GPU, no
//!          permissions, headless) → try the next one → software.
//!       3. `get_format` callback — when the decoder negotiates the
//!          pix_fmt, we pick the HW format if it's on the list.
//!       4. Per frame: if `frame.format()` is the HW format, copy it
//!          to RAM with `av_hwframe_transfer_data` (→ typically NV12)
//!          and continue the normal pipeline (sws NV12→RGB24).
//!
//!   * Why copy-back to RAM instead of zero-copy? The sink is a
//!     terminal: cells are generated on the CPU no matter what. The
//!     win is in the decode (the expensive part of 4K AV1/HEVC), not
//!     the scaling.
//!
//!   * `get_format` is a C callback with no direct userdata. rtv
//!     opens a single video decoder per process, so the expected HW
//!     format is published in an atomic static (`EXPECTED_HW_FMT`).
//!     If we ever run N decoders, this moves to `(*ctx).opaque`.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::sys as ff;
use std::ffi::CStr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

/// Hwaccel diagnostics buffer. Messages go to stderr immediately
/// (handy with `2>file`), but the decoder opens inside the alternate
/// screen — in the terminal the video covers them and leaving the alt
/// screen discards them (which is why --verbose "only showed the line
/// listing available hwaccels"). They're collected here and main dumps
/// them on exit with --verbose, once the terminal is restored.
static DIAG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record one diagnostic line (immediate stderr + buffer).
pub fn diag(msg: String) {
    eprintln!("{msg}");
    if let Ok(mut d) = DIAG.lock() {
        if d.len() < 100 {
            d.push(msg);
        }
    }
}

/// Drain and return the collected diagnostics (main, on exit).
pub fn take_diagnostics() -> Vec<String> {
    DIAG.lock()
        .map(|mut d| std::mem::take(&mut *d))
        .unwrap_or_default()
}

/// Human-readable text for an FFmpeg error code (av_strerror).
fn av_err_str(code: i32) -> String {
    // Portability gotcha: `c_char` is i8 on x86 but u8 on ARM/AArch64
    // (linux-arm, Termux, Apple Silicon macOS). A bare [0i8; N] buffer
    // does not compile on ARM — always use the c_char alias.
    let mut buf = [0 as std::os::raw::c_char; 128];
    unsafe {
        if ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) == 0 {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            format!("error {code}")
        }
    }
}

/// User preference (CLI `--hwdec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPref {
    /// Try hwaccels in the platform's preference order; if none
    /// works → software. (default)
    Auto,
    /// Software only.
    None,
    /// Force one specific type (if it fails → software, with a
    /// warning).
    Only(ff::AVHWDeviceType),
}

impl HwPref {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "auto" => HwPref::Auto,
            "none" | "no" | "off" => HwPref::None,
            "vaapi" => HwPref::Only(ff::AVHWDeviceType::VAAPI),
            "cuda" | "nvdec" => HwPref::Only(ff::AVHWDeviceType::CUDA),
            "qsv" => HwPref::Only(ff::AVHWDeviceType::QSV),
            "d3d11va" => HwPref::Only(ff::AVHWDeviceType::D3D11VA),
            "dxva2" => HwPref::Only(ff::AVHWDeviceType::DXVA2),
            "videotoolbox" | "vt" => HwPref::Only(ff::AVHWDeviceType::VIDEOTOOLBOX),
            "vulkan" => HwPref::Only(ff::AVHWDeviceType::VULKAN),
            "drm" => HwPref::Only(ff::AVHWDeviceType::DRM),
            "vdpau" => HwPref::Only(ff::AVHWDeviceType::VDPAU),
            other => {
                return Err(format!(
                    "--hwdec '{other}' not recognized (auto|none|vaapi|cuda|qsv|d3d11va|dxva2|videotoolbox|vulkan|drm|vdpau)"
                ))
            }
        })
    }
}

/// Is there an NVIDIA GPU with the proprietary driver loaded? (Linux)
///
/// This matters for the `--hwdec auto` order: on NVIDIA, VAAPI only
/// exists through `nvidia-vaapi-driver` (a translation layer over
/// NVDEC built for Firefox) which is slower and more fragile than
/// native CUDA/NVDEC — yet `av_hwdevice_ctx_create(VAAPI)` opens with
/// it without error, so if VAAPI goes first rtv gets stuck on the
/// slow layer and the user "doesn't feel" the hwdec. With NVIDIA
/// detected, CUDA moves to the front.
#[cfg(target_os = "linux")]
fn has_nvidia() -> bool {
    std::path::Path::new("/proc/driver/nvidia").exists()
        || std::path::Path::new("/dev/nvidiactl").exists()
        || std::path::Path::new("/dev/nvidia0").exists()
}

/// Per-platform preference order for `--hwdec auto`.
/// Only the ones the decoder advertises via `avcodec_get_hw_config`
/// get tried — the list is the tiebreaker.
fn platform_preference() -> &'static [ff::AVHWDeviceType] {
    #[cfg(target_os = "linux")]
    {
        if has_nvidia() {
            // NVIDIA: native NVDEC first. VDPAU (also NVIDIA-native)
            // ahead of VAAPI (which here would be the
            // nvidia-vaapi-driver translation layer).
            &[
                ff::AVHWDeviceType::CUDA,
                ff::AVHWDeviceType::VDPAU,
                ff::AVHWDeviceType::VAAPI,
                ff::AVHWDeviceType::QSV,
                ff::AVHWDeviceType::VULKAN,
                ff::AVHWDeviceType::DRM,
            ]
        } else {
            // Intel/AMD: VAAPI is the native path.
            &[
                ff::AVHWDeviceType::VAAPI,
                ff::AVHWDeviceType::CUDA,
                ff::AVHWDeviceType::QSV,
                ff::AVHWDeviceType::VDPAU,
                ff::AVHWDeviceType::VULKAN,
                ff::AVHWDeviceType::DRM,
            ]
        }
    }
    #[cfg(target_os = "windows")]
    {
        &[
            ff::AVHWDeviceType::D3D11VA,
            ff::AVHWDeviceType::DXVA2,
            ff::AVHWDeviceType::CUDA,
            ff::AVHWDeviceType::QSV,
            ff::AVHWDeviceType::VULKAN,
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[ff::AVHWDeviceType::VIDEOTOOLBOX]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        &[]
    }
}

pub fn type_name(t: ff::AVHWDeviceType) -> &'static str {
    unsafe {
        let p = ff::av_hwdevice_get_type_name(t);
        if p.is_null() {
            "?"
        } else {
            CStr::from_ptr(p).to_str().unwrap_or("?")
        }
    }
}

/// HW pixel format the `get_format` callback should pick.
/// AV_PIX_FMT_NONE (-1) = hwaccel inactive (picks software).
static EXPECTED_HW_FMT: AtomicI32 = AtomicI32::new(-1);

/// hw_decode.c-style `get_format` callback: picks the HW format
/// published in EXPECTED_HW_FMT if the decoder offers it; otherwise
/// defers to FFmpeg's default (software) choice. Never aborts: a
/// hwaccel that stops being offered mid-stream (unsupported
/// resolution/profile change) degrades to software on its own.
unsafe extern "C" fn get_format_cb(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let want = EXPECTED_HW_FMT.load(Ordering::Acquire);
    if want >= 0 && !fmts.is_null() {
        let mut p = fmts;
        while (*p).0 != -1 {
            if (*p).0 == want {
                return *p;
            }
            p = p.add(1);
        }
    }
    // Fallback: first non-HW format on the list (software choice).
    if !fmts.is_null() {
        let mut p = fmts;
        while (*p).0 != -1 {
            let desc = ff::av_pix_fmt_desc_get(*p);
            if !desc.is_null() && ((*desc).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64) == 0 {
                return *p;
            }
            p = p.add(1);
        }
    }
    ff::AVPixelFormat(-1)
}

/// A hwaccel that is active on an open decoder.
pub struct ActiveHw {
    /// Device type (for the HUD/logs).
    pub device_type: ff::AVHWDeviceType,
    /// HW pixel format the decoder will emit (e.g. AV_PIX_FMT_VAAPI).
    pub hw_pix_fmt: ff::AVPixelFormat,
    /// Reference to the device ctx (owning; released in Drop).
    device_ref: *mut ff::AVBufferRef,
}

// The device's AVBufferRef is refcounted and thread-safe
// (av_buffer_*); ActiveHw moves to the decoder thread.
unsafe impl Send for ActiveHw {}

impl Drop for ActiveHw {
    fn drop(&mut self) {
        unsafe {
            if !self.device_ref.is_null() {
                ff::av_buffer_unref(&mut self.device_ref);
            }
        }
    }
}

impl ActiveHw {
    /// Readable hwaccel name (--verbose logs and diagnostics).
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        type_name(self.device_type)
    }
}

/// Try to enable HW decoding on an `AVCodecContext` that is already
/// configured (stream parameters copied, threading set) but not yet
/// opened with avcodec_open2. `codec` is the decoder that will be
/// used (the `(*ctx).codec` field is still null before open). Returns
/// `Some(ActiveHw)` when a hwaccel got hooked up (hw_device_ctx +
/// get_format set) or `None` when we stay on software (context left
/// untouched).
///
/// # Safety
/// `ctx` must be a valid, unopened AVCodecContext and `codec` the
/// AVCodec it will be opened with.
pub unsafe fn try_enable(
    ctx: *mut ff::AVCodecContext,
    codec: *const ff::AVCodec,
    pref: HwPref,
) -> Option<ActiveHw> {
    if matches!(pref, HwPref::None) {
        EXPECTED_HW_FMT.store(-1, Ordering::Release);
        return None;
    }
    if codec.is_null() {
        return None;
    }
    // Diagnostics: with --verbose stderr is visible; without it it
    // goes to /dev/null (silence_stderr) and these eprintln cost
    // nothing.
    let codec_name = CStr::from_ptr((*codec).name).to_str().unwrap_or("?");

    // Candidates = hwaccels this decoder supports with HW_DEVICE_CTX.
    let mut candidates: Vec<(ff::AVHWDeviceType, ff::AVPixelFormat)> = Vec::new();
    let mut i = 0;
    loop {
        let cfg = ff::avcodec_get_hw_config(codec, i);
        if cfg.is_null() {
            break;
        }
        let method_ok =
            ((*cfg).methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX.0 as i32) != 0;
        if method_ok {
            candidates.push(((*cfg).device_type, (*cfg).pix_fmt));
        }
        i += 1;
    }
    if candidates.is_empty() {
        diag(format!(
            "hwdec: decoder '{codec_name}' advertises no hwaccels \
             (FFmpeg built without HW support for this codec?) → software"
        ));
        return None;
    }
    diag(format!(
        "hwdec: '{codec_name}' advertises: {:?}",
        candidates.iter().map(|(t, _)| type_name(*t)).collect::<Vec<_>>()
    ));

    // Trial order according to preference.
    let try_order: Vec<(ff::AVHWDeviceType, ff::AVPixelFormat)> = match pref {
        HwPref::Only(t) => candidates.iter().copied().filter(|(dt, _)| *dt == t).collect(),
        HwPref::Auto => {
            let prefs = platform_preference();
            let mut v: Vec<_> = Vec::new();
            for want in prefs {
                if let Some(c) = candidates.iter().find(|(dt, _)| dt == want) {
                    v.push(*c);
                }
            }
            // Anything else the codec advertises that isn't on the
            // platform list goes last (better to try it than fall to
            // software).
            for c in &candidates {
                if !v.contains(c) {
                    v.push(*c);
                }
            }
            v
        }
        HwPref::None => unreachable!(),
    };

    for (dev_type, hw_fmt) in try_order {
        let mut device_ref: *mut ff::AVBufferRef = std::ptr::null_mut();
        let ret = ff::av_hwdevice_ctx_create(
            &mut device_ref,
            dev_type,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        if ret < 0 || device_ref.is_null() {
            // No device (headless, no permissions, missing driver…)
            // → next candidate.
            diag(format!(
                "hwdec: {} unavailable (av_hwdevice_ctx_create: {})",
                type_name(dev_type),
                av_err_str(ret)
            ));
            continue;
        }
        // Hook into the context: the codec takes its own ref.
        (*ctx).hw_device_ctx = ff::av_buffer_ref(device_ref);
        if (*ctx).hw_device_ctx.is_null() {
            ff::av_buffer_unref(&mut device_ref);
            continue;
        }
        (*ctx).get_format = Some(get_format_cb);
        EXPECTED_HW_FMT.store(hw_fmt.0, Ordering::Release);
        diag(format!(
            "hwdec: {} hooked up to '{codec_name}'",
            type_name(dev_type)
        ));
        return Some(ActiveHw {
            device_type: dev_type,
            hw_pix_fmt: hw_fmt,
            device_ref,
        });
    }
    EXPECTED_HW_FMT.store(-1, Ordering::Release);
    diag(format!(
        "hwdec: no usable hwaccel for '{codec_name}' → software"
    ));
    None
}

/// Unhook the hwaccel from a context (used in the mid-stream
/// fallback: the static is cleared so get_format picks software).
pub fn disable_expected_fmt() {
    EXPECTED_HW_FMT.store(-1, Ordering::Release);
}

/// Copy a HW frame (GPU surface) to RAM. `dst` gets reset and FFmpeg
/// fills it with the native transfer format (almost always NV12).
/// Props (pts, etc.) are copied too. Returns false when the transfer
/// failed (crashed driver, unmappable format).
pub fn transfer_to_ram(
    src: &ffmpeg::util::frame::video::Video,
    dst: &mut ffmpeg::util::frame::video::Video,
) -> bool {
    unsafe {
        ff::av_frame_unref(dst.as_mut_ptr());
        if ff::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0) < 0 {
            return false;
        }
        ff::av_frame_copy_props(dst.as_mut_ptr(), src.as_ptr());
    }
    true
}

/// Is this frame in the active HW format? (raw-value comparison).
pub fn is_hw_frame(frame: &ffmpeg::util::frame::video::Video, hw: &ActiveHw) -> bool {
    unsafe { (*frame.as_ptr()).format == hw.hw_pix_fmt.0 }
}

/// Readable name for a device type stored as a raw i32 (for the HUD:
/// the player reads the atomic `DecoderHandle::hw_state`; -1 =
/// software).
pub fn name_of_raw(v: i32) -> Option<&'static str> {
    if v <= 0 {
        return None;
    }
    Some(type_name(ff::AVHWDeviceType(v as _)))
}

/// List the hwaccels compiled into the linked FFmpeg (for --verbose
/// and diagnostics).
pub fn available_types() -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut t = ff::AVHWDeviceType::NONE;
    unsafe {
        loop {
            t = ff::av_hwdevice_iterate_types(t);
            if t == ff::AVHWDeviceType::NONE {
                break;
            }
            v.push(type_name(t));
        }
    }
    v
}
