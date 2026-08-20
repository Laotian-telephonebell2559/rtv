// rotation.rs — auto-rotate videos according to container metadata.
//
// Videos shot with a phone held vertically are stored landscape (the
// sensor doesn't rotate) plus a "Display Matrix" in the stream saying
// "rotate me 90° on display". Serious players (mpv, ffplay, VLC)
// apply that rotation automatically; without it the video shows up
// sideways. This module:
//
//   1. Reads the stream's Display Matrix (coded_side_data on the
//      codecpar — the modern spot where the MP4/MOV demuxer leaves
//      it) and, as a fallback, the "rotate" metadata tag (old files /
//      remuxes made by older tools).
//   2. Normalizes the angle to one of the 4 cardinal rotations
//      (0/90/180/270) — same as ffplay: arbitrary angles don't exist
//      in practice (phones write these) and supporting them would
//      need interpolated resampling.
//   3. Rotates the already-scaled RGB24 buffer (post-sws, on the
//      decoder thread). Rotating after scaling is the cheap way: you
//      rotate the small destination frame (say 640×360), not the 4K
//      source.
//
// Sign convention (the ffplay/mpv one): av_display_rotation_get
// returns the counter-clockwise angle the matrix applies; to correct
// on screen you rotate the frame by `-θ` (that is, θ degrees
// clockwise). `Transform::Rot90` = rotate the frame 90° clockwise at
// presentation time.

use ffmpeg_the_third as ffmpeg;

use crate::decoder::RgbFrame;

/// Presentation rotation to apply to every decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    None,
    /// 90° clockwise (the typical "portrait" phone video).
    Rot90,
    /// 180° (upside down).
    Rot180,
    /// 270° clockwise (= 90° counter-clockwise).
    Rot270,
}

impl Transform {
    /// Does it swap width and height?
    pub fn swaps_dims(self) -> bool {
        matches!(self, Transform::Rot90 | Transform::Rot270)
    }

    /// Dimensions the source frame must be scaled to (before
    /// rotating) so that after rotation it lands exactly on
    /// `(dst_w, dst_h)`: with 90/270, sws scales to the transposed
    /// pair.
    pub fn pre_rotate_dims(self, dst_w: u32, dst_h: u32) -> (u32, u32) {
        if self.swaps_dims() {
            (dst_h, dst_w)
        } else {
            (dst_w, dst_h)
        }
    }

    /// Source size as presented (for the player's layout/aspect
    /// math).
    pub fn display_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_dims() {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// Human-readable label for `--info` / HUD (`None` when there is
    /// no rotation).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Transform::None => None,
            Transform::Rot90 => Some("rotated 90°"),
            Transform::Rot180 => Some("rotated 180°"),
            Transform::Rot270 => Some("rotated 270°"),
        }
    }
}

/// Rotate the RGB24 frame in place (rewrites buffer and dims).
///
/// Cost: one O(w·h) pass over the frame already scaled to terminal
/// size (a few hundred KB) — negligible next to decode+sws. Only
/// called when there's an actual rotation (`Transform::None` is a
/// copy-free no-op).
pub fn rotate_frame(frame: &mut RgbFrame, t: Transform) {
    if t == Transform::None {
        return;
    }
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || frame.data.len() < w * h * 3 {
        return;
    }
    let src = &frame.data;
    let mut dst = vec![0u8; w * h * 3];
    match t {
        Transform::None => unreachable!(),
        // 90° clockwise: dst has h columns × w rows.
        // dst(x, y) = src(col = y, row = h-1-x)
        Transform::Rot90 => {
            let (dw, dh) = (h, w);
            for y in 0..dh {
                let drow = y * dw * 3;
                for x in 0..dw {
                    let s = ((h - 1 - x) * w + y) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
            frame.width = dw as u32;
            frame.height = dh as u32;
        }
        // 180°: same dims; dst(x, y) = src(w-1-x, h-1-y).
        Transform::Rot180 => {
            for y in 0..h {
                let drow = y * w * 3;
                let srow = (h - 1 - y) * w * 3;
                for x in 0..w {
                    let s = srow + (w - 1 - x) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
        }
        // 270° clockwise (90° counter-clockwise): dst h×w.
        // dst(x, y) = src(col = w-1-y, row = x)
        Transform::Rot270 => {
            let (dw, dh) = (h, w);
            for y in 0..dh {
                let drow = y * dw * 3;
                for x in 0..dw {
                    let s = (x * w + (w - 1 - y)) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
            frame.width = dw as u32;
            frame.height = dh as u32;
        }
    }
    frame.data = dst;
}

/// Presentation rotation for the video stream: Display Matrix from
/// the codecpar (modern) or the `rotate` metadata tag (legacy).
pub fn from_stream(stream: &ffmpeg::format::stream::Stream) -> Transform {
    if let Some(theta) = display_matrix_theta(stream) {
        return transform_from_theta(theta);
    }
    // Legacy fallback: the "rotate" tag (old MOVs, old remuxes). Tag
    // convention: clockwise degrees to apply at presentation time
    // (the opposite of the matrix — which is why there's no negation
    // here).
    if let Some(r) = stream.metadata().get("rotate") {
        if let Ok(deg) = r.trim().parse::<f64>() {
            return transform_from_theta(deg);
        }
    }
    Transform::None
}

/// Presentation θ (clockwise degrees) from the stream's Display
/// Matrix, or `None` when the stream carries no matrix.
fn display_matrix_theta(stream: &ffmpeg::format::stream::Stream) -> Option<f64> {
    use ffmpeg::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return None;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).data.is_null() || (*sd).size < 9 * 4 {
            return None;
        }
        // The matrix is 9 int32 values in 16.16 fixed point —
        // av_display_rotation_get does the trigonometry. It returns
        // the counter-clockwise degrees the matrix applies; the
        // presentation correction is the negation (ffplay does
        // `theta = -av_display_rotation_get(m)`).
        let m = (*sd).data as *const i32;
        let ccw = ffi::av_display_rotation_get(m);
        if ccw.is_nan() {
            return None;
        }
        Some(-ccw)
    }
}

/// Normalize an arbitrary clockwise angle to the nearest cardinal
/// rotation (rounding like ffplay: only multiples of 90 are
/// supported; a phone sensor's 89.98° becomes 90°).
fn transform_from_theta(theta_cw: f64) -> Transform {
    // Into [0, 360): f64's rem_euclid.
    let t = theta_cw.rem_euclid(360.0);
    // Nearest cardinal (355° → 0).
    match ((t / 90.0).round() as i64).rem_euclid(4) {
        1 => Transform::Rot90,
        2 => Transform::Rot180,
        3 => Transform::Rot270,
        _ => Transform::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_2x3(pix: &[[u8; 3]]) -> RgbFrame {
        // 2 columns × 3 rows, row-major order.
        assert_eq!(pix.len(), 6);
        RgbFrame {
            width: 2,
            height: 3,
            pts: 0.0,
            serial: 0,
            data: pix.iter().flatten().copied().collect(),
        }
    }

    fn px(f: &RgbFrame, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * f.width + x) * 3) as usize;
        [f.data[i], f.data[i + 1], f.data[i + 2]]
    }

    // Named pixels: a b / c d / e f  (2 wide × 3 tall)
    const A: [u8; 3] = [1, 0, 0];
    const B: [u8; 3] = [2, 0, 0];
    const C: [u8; 3] = [3, 0, 0];
    const D: [u8; 3] = [4, 0, 0];
    const E: [u8; 3] = [5, 0, 0];
    const F: [u8; 3] = [6, 0, 0];

    #[test]
    fn rot90_clockwise() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot90);
        // 90° CW of (a b / c d / e f) = (e c a / f d b), 3×2.
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(px(&f, 0, 0), E);
        assert_eq!(px(&f, 1, 0), C);
        assert_eq!(px(&f, 2, 0), A);
        assert_eq!(px(&f, 0, 1), F);
        assert_eq!(px(&f, 1, 1), D);
        assert_eq!(px(&f, 2, 1), B);
    }

    #[test]
    fn rot180() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot180);
        // 180° = (f e / d c / b a), 2×3.
        assert_eq!((f.width, f.height), (2, 3));
        assert_eq!(px(&f, 0, 0), F);
        assert_eq!(px(&f, 1, 0), E);
        assert_eq!(px(&f, 0, 2), B);
        assert_eq!(px(&f, 1, 2), A);
    }

    #[test]
    fn rot270_clockwise() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot270);
        // 270° CW (= 90° CCW) of (a b / c d / e f) = (b d f / a c e).
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(px(&f, 0, 0), B);
        assert_eq!(px(&f, 1, 0), D);
        assert_eq!(px(&f, 2, 0), F);
        assert_eq!(px(&f, 0, 1), A);
        assert_eq!(px(&f, 1, 1), C);
        assert_eq!(px(&f, 2, 1), E);
    }

    #[test]
    fn rot90_and_rot270_cancel_out() {
        let orig = frame_2x3(&[A, B, C, D, E, F]);
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot90);
        rotate_frame(&mut f, Transform::Rot270);
        assert_eq!(f.data, orig.data);
        assert_eq!((f.width, f.height), (2, 3));
    }

    #[test]
    fn theta_normalization() {
        assert_eq!(transform_from_theta(0.0), Transform::None);
        assert_eq!(transform_from_theta(90.0), Transform::Rot90);
        assert_eq!(transform_from_theta(180.0), Transform::Rot180);
        assert_eq!(transform_from_theta(270.0), Transform::Rot270);
        // Negative and >360 (rem_euclid).
        assert_eq!(transform_from_theta(-90.0), Transform::Rot270);
        assert_eq!(transform_from_theta(-270.0), Transform::Rot90);
        assert_eq!(transform_from_theta(450.0), Transform::Rot90);
        // Rounding to the nearest cardinal (phone sensor: 89.98°).
        assert_eq!(transform_from_theta(89.98), Transform::Rot90);
        assert_eq!(transform_from_theta(180.02), Transform::Rot180);
        assert_eq!(transform_from_theta(-90.01), Transform::Rot270);
        assert_eq!(transform_from_theta(359.9), Transform::None);
        // Odd angles → nearest cardinal (same rounding as ffplay).
        assert_eq!(transform_from_theta(44.0), Transform::None);
        assert_eq!(transform_from_theta(46.0), Transform::Rot90);
    }

    #[test]
    fn pre_rotate_and_display_dims() {
        assert_eq!(Transform::Rot90.pre_rotate_dims(640, 360), (360, 640));
        assert_eq!(Transform::Rot180.pre_rotate_dims(640, 360), (640, 360));
        assert_eq!(Transform::None.pre_rotate_dims(640, 360), (640, 360));
        assert_eq!(Transform::Rot90.display_size(1920, 1080), (1080, 1920));
        assert_eq!(Transform::Rot270.display_size(1920, 1080), (1080, 1920));
        assert_eq!(Transform::Rot180.display_size(1920, 1080), (1920, 1080));
    }

    #[test]
    fn display_matrix_via_ffi() {
        // av_display_rotation_get(m) = -atan2(m[1], m[0]) in degrees
        // (16.16 fixed point). The typical "portrait" iPhone matrix
        // has m[1]=+1 ⇒ get returns -90 (CCW) ⇒ presentation
        // θ = -get = +90 clockwise ⇒ Rot90. We verify the whole
        // chain sign-by-sign against the actual linked FFmpeg.
        let f = |v: f64| (v * 65536.0) as i32;
        let m_iphone: [i32; 9] = [f(0.0), f(1.0), 0, f(-1.0), f(0.0), 0, 0, 0, 1 << 30];
        let ccw = unsafe { ffmpeg::ffi::av_display_rotation_get(m_iphone.as_ptr()) };
        assert!((ccw + 90.0).abs() < 0.01, "ccw={ccw}");
        assert_eq!(transform_from_theta(-ccw), Transform::Rot90);

        let m180: [i32; 9] = [f(-1.0), f(0.0), 0, f(0.0), f(-1.0), 0, 0, 0, 1 << 30];
        let ccw = unsafe { ffmpeg::ffi::av_display_rotation_get(m180.as_ptr()) };
        assert!((ccw.abs() - 180.0).abs() < 0.01, "ccw={ccw}");
        assert_eq!(transform_from_theta(-ccw), Transform::Rot180);
    }
}
