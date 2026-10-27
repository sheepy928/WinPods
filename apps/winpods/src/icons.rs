//! Tray icons drawn at runtime: a filled circle in the connection-state colour with a
//! white speaker glyph, rendered with 4x4 supersampling at whatever size the shell
//! wants (16/20/24/32 px for 100/125/150/200 % scaling).
//!
//! Everything here is pure (no Windows calls) so it can be unit tested; `tray.rs`
//! turns the RGBA buffers into `tray_icon::Icon`s.

use winpods_engine::State;

use crate::view::StatusKind;

/// An sRGB colour without alpha.
pub type Rgb = [u8; 3];

/// Grey: not connected.
pub const COLOR_DISCONNECTED: Rgb = [0x8a, 0x8a, 0x8a];
/// Amber: connecting, waiting to reconnect, or streaming without captured audio.
pub const COLOR_CONNECTING: Rgb = [0xe0, 0xa0, 0x00];
/// Green: streaming.
pub const COLOR_STREAMING: Rgb = [0x2e, 0xaa, 0x4a];
/// Red: the last attempt failed.
pub const COLOR_ERROR: Rgb = [0xd0, 0x40, 0x40];

/// The four tray icon variants (Reconnecting shares the Connecting colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayIconKind {
    Disconnected,
    Connecting,
    Streaming,
    Error,
}

impl TrayIconKind {
    /// All variants, in cache order.
    pub const ALL: [TrayIconKind; 4] = [
        TrayIconKind::Disconnected,
        TrayIconKind::Connecting,
        TrayIconKind::Streaming,
        TrayIconKind::Error,
    ];

    /// Background colour of the icon.
    pub fn color(self) -> Rgb {
        match self {
            TrayIconKind::Disconnected => COLOR_DISCONNECTED,
            TrayIconKind::Connecting => COLOR_CONNECTING,
            TrayIconKind::Streaming => COLOR_STREAMING,
            TrayIconKind::Error => COLOR_ERROR,
        }
    }

    /// Position in [`TrayIconKind::ALL`] (used to index icon caches).
    pub fn index(self) -> usize {
        match self {
            TrayIconKind::Disconnected => 0,
            TrayIconKind::Connecting => 1,
            TrayIconKind::Streaming => 2,
            TrayIconKind::Error => 3,
        }
    }
}

impl From<State> for TrayIconKind {
    fn from(state: State) -> Self {
        match state {
            State::Disconnected => TrayIconKind::Disconnected,
            State::Connecting | State::Reconnecting => TrayIconKind::Connecting,
            State::Streaming => TrayIconKind::Streaming,
            State::Error => TrayIconKind::Error,
        }
    }
}

impl From<StatusKind> for TrayIconKind {
    fn from(kind: StatusKind) -> Self {
        match kind {
            StatusKind::Idle => TrayIconKind::Disconnected,
            StatusKind::Busy => TrayIconKind::Connecting,
            StatusKind::Ok => TrayIconKind::Streaming,
            StatusKind::Error => TrayIconKind::Error,
        }
    }
}

/// Supersampling grid per pixel axis.
const SUBSAMPLES: u32 = 4;
/// Radius of the backdrop circle in unit coordinates.
const CIRCLE_RADIUS: f32 = 0.48;

/// Speaker glyph geometry in unit coordinates (0..1 across the icon).
#[derive(Debug, Clone, Copy)]
struct Glyph {
    /// Horizontal shift applied to every part so the glyph looks centred.
    dx: f32,
    /// (radius, stroke width) of each sound wave.
    waves: &'static [(f32, f32)],
}

impl Glyph {
    /// Small icons get one bold wave; larger ones two thinner waves.
    fn for_size(size: u32) -> Glyph {
        if size <= 24 {
            Glyph {
                dx: 0.03,
                waves: &[(0.17, 0.11)],
            }
        } else {
            Glyph {
                dx: -0.01,
                waves: &[(0.14, 0.075), (0.26, 0.075)],
            }
        }
    }

    /// True when the unit-space point lies on the white glyph.
    fn contains(&self, x: f32, y: f32) -> bool {
        let x = x - self.dx;
        let dy = (y - 0.5).abs();
        // Magnet / body of the speaker.
        if (0.24..=0.36).contains(&x) && dy <= 0.10 {
            return true;
        }
        // Cone: widens from half-height 0.10 at x=0.35 to 0.25 at x=0.51.
        if (0.35..=0.51).contains(&x) {
            let t = (x - 0.35) / (0.51 - 0.35);
            if dy <= 0.10 + t * 0.15 {
                return true;
            }
        }
        // Sound waves: arc segments centred on the cone mouth, within +-50 degrees.
        let (cx, cy) = (0.49, 0.5);
        let (vx, vy) = (x - cx, y - cy);
        if vx > 0.0 && vy.abs() <= vx * 1.19 {
            let d = (vx * vx + vy * vy).sqrt();
            for &(radius, width) in self.waves {
                if (d - radius).abs() <= width / 2.0 {
                    return true;
                }
            }
        }
        false
    }
}

/// Render the tray icon for `kind` as straight (non-premultiplied) RGBA, row-major,
/// `size * size * 4` bytes. Sizes below 8 px are rendered at 8 px.
pub fn render_tray_icon(kind: TrayIconKind, size: u32) -> Vec<u8> {
    let size = size.max(8);
    let glyph = Glyph::for_size(size);
    let backdrop = kind.color().map(f32::from);
    let white = [255.0_f32; 3];
    let samples = (SUBSAMPLES * SUBSAMPLES) as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);

    for py in 0..size {
        for px in 0..size {
            // Premultiplied accumulation over the subsample grid.
            let mut acc = [0.0_f32; 4];
            for sy in 0..SUBSAMPLES {
                for sx in 0..SUBSAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SUBSAMPLES as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SUBSAMPLES as f32) / size as f32;
                    let (dx, dy) = (x - 0.5, y - 0.5);
                    if dx * dx + dy * dy > CIRCLE_RADIUS * CIRCLE_RADIUS {
                        continue;
                    }
                    let color = if glyph.contains(x, y) {
                        &white
                    } else {
                        &backdrop
                    };
                    acc[0] += color[0];
                    acc[1] += color[1];
                    acc[2] += color[2];
                    acc[3] += 1.0;
                }
            }
            if acc[3] == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let coverage = acc[3];
                out.push(to_u8(acc[0] / coverage));
                out.push(to_u8(acc[1] / coverage));
                out.push(to_u8(acc[2] / coverage));
                out.push(to_u8(coverage / samples * 255.0));
            }
        }
    }
    out
}

fn to_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// Pick the tray icon size for a system small-icon metric (`SM_CXSMICON`), keeping it
/// within sane bounds when the metric is unavailable (0) or absurd.
pub fn tray_icon_size(small_icon_metric: i32) -> u32 {
    match u32::try_from(small_icon_metric) {
        Ok(size) if (16..=64).contains(&size) => size,
        Ok(size) if size > 64 => 64,
        _ => 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    /// Pixel at unit coordinates (u, v).
    fn at(buf: &[u8], size: u32, u: f32, v: f32) -> [u8; 4] {
        let x = ((u * size as f32) as u32).min(size - 1);
        let y = ((v * size as f32) as u32).min(size - 1);
        pixel(buf, size, x, y)
    }

    #[test]
    fn buffer_has_expected_length_for_dpi_sizes() {
        for size in [16, 20, 24, 32, 48] {
            for kind in TrayIconKind::ALL {
                assert_eq!(
                    render_tray_icon(kind, size).len(),
                    (size * size * 4) as usize
                );
            }
        }
    }

    #[test]
    fn corners_are_transparent() {
        for size in [16, 24, 32] {
            let buf = render_tray_icon(TrayIconKind::Streaming, size);
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert_eq!(pixel(&buf, size, x, y)[3], 0, "corner {x},{y} at {size}px");
            }
        }
    }

    #[test]
    fn backdrop_uses_state_colour() {
        for kind in TrayIconKind::ALL {
            let buf = render_tray_icon(kind, 32);
            // Well inside the circle, above the glyph.
            let p = at(&buf, 32, 0.40, 0.12);
            assert_eq!(&p[..3], &kind.color(), "{kind:?}");
            assert_eq!(p[3], 255);
        }
    }

    #[test]
    fn glyph_is_white_and_opaque() {
        for size in [16, 24, 32] {
            let buf = render_tray_icon(TrayIconKind::Disconnected, size);
            // Speaker body, next to where the cone starts.
            let p = at(&buf, size, 0.33, 0.5);
            assert_eq!(p, [255, 255, 255, 255], "{size}px");
        }
    }

    #[test]
    fn icon_is_vertically_symmetric() {
        let size = 24;
        let buf = render_tray_icon(TrayIconKind::Error, size);
        for y in 0..size / 2 {
            for x in 0..size {
                assert_eq!(pixel(&buf, size, x, y), pixel(&buf, size, x, size - 1 - y));
            }
        }
    }

    #[test]
    fn edges_are_antialiased() {
        let buf = render_tray_icon(TrayIconKind::Streaming, 32);
        let (pixels, _) = buf.as_chunks::<4>();
        let partial = pixels.iter().filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(partial > 0, "expected anti-aliased edge pixels");
    }

    #[test]
    fn state_maps_to_icon_kind() {
        assert_eq!(
            TrayIconKind::from(State::Disconnected),
            TrayIconKind::Disconnected
        );
        assert_eq!(
            TrayIconKind::from(State::Connecting),
            TrayIconKind::Connecting
        );
        assert_eq!(
            TrayIconKind::from(State::Reconnecting),
            TrayIconKind::Connecting
        );
        assert_eq!(
            TrayIconKind::from(State::Streaming),
            TrayIconKind::Streaming
        );
        assert_eq!(TrayIconKind::from(State::Error), TrayIconKind::Error);
        for (i, kind) in TrayIconKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i);
        }
    }

    #[test]
    fn tiny_sizes_are_clamped() {
        assert_eq!(
            render_tray_icon(TrayIconKind::Streaming, 0).len(),
            8 * 8 * 4
        );
    }

    #[test]
    fn tray_size_follows_metric() {
        assert_eq!(tray_icon_size(16), 16);
        assert_eq!(tray_icon_size(20), 20);
        assert_eq!(tray_icon_size(24), 24);
        assert_eq!(tray_icon_size(32), 32);
        assert_eq!(tray_icon_size(0), 16);
        assert_eq!(tray_icon_size(-1), 16);
        assert_eq!(tray_icon_size(300), 64);
    }
}
