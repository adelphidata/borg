use crossterm::terminal;
use ratatui::style::Color;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

/// Cached terminal background RGB, queried once via OSC 11.
static TERMINAL_BG: OnceLock<Option<(u8, u8, u8)>> = OnceLock::new();

/// RAII guard that restores cooked mode on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Query the terminal's actual background color via OSC 11.
/// Must be called **before** entering the alternate screen, as some terminals
/// don't respond to OSC queries inside the alt buffer.
pub(super) fn query_terminal_bg() {
    TERMINAL_BG.get_or_init(|| {
        query_osc11_bg().or_else(|| {
            // Fallback: try COLORFGBG env var (rxvt-family terminals)
            if let Ok(val) = std::env::var("COLORFGBG") {
                if let Some(bg_str) = val.rsplit(';').next() {
                    if let Ok(bg_idx) = bg_str.trim().parse::<u8>() {
                        let is_light = bg_idx == 7 || bg_idx >= 9;
                        return Some(if is_light {
                            (240, 240, 240)
                        } else {
                            (30, 30, 30)
                        });
                    }
                }
            }
            None
        })
    });
}

/// Send OSC 11 query and parse the terminal's background color response.
/// Briefly enters raw mode, sends the query, reads raw bytes from stdin
/// using `poll(2)` with a 100ms timeout, then restores terminal state.
#[cfg(unix)]
fn query_osc11_bg() -> Option<(u8, u8, u8)> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let was_raw = terminal::is_raw_mode_enabled().unwrap_or(false);
    let _guard = if !was_raw {
        terminal::enable_raw_mode().ok()?;
        Some(RawModeGuard)
    } else {
        None
    };

    // Send OSC 11 query with BEL terminator (widest compatibility)
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b]11;?\x07");
    let _ = stdout.flush();

    let mut buf = [0u8; 128];
    let mut total = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_millis(100);

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let mut stdin_lock = stdin.lock();

    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as i32;
        if remaining <= 0 || total >= buf.len() {
            break;
        }
        let ret = unsafe { libc::poll(&mut poll_fd, 1, remaining) };
        if ret <= 0 {
            break;
        }
        let n = stdin_lock.read(&mut buf[total..]).unwrap_or(0);
        if n == 0 {
            break;
        }
        total += n;
        // Check for terminator: BEL (\x07) or ST (\x1b\\)
        if buf[..total].contains(&0x07) || buf[..total].windows(2).any(|w| w == b"\x1b\\") {
            break;
        }
    }

    drop(stdin_lock);
    // _guard drops here, restoring raw mode if we changed it

    if total == 0 {
        return None;
    }

    parse_osc11_response(&buf[..total])
}

#[cfg(not(unix))]
fn query_osc11_bg() -> Option<(u8, u8, u8)> {
    None
}

/// Parse an OSC 11 response like `\x1b]11;rgb:1c1c/1c1c/1c1c\x07`
fn parse_osc11_response(data: &[u8]) -> Option<(u8, u8, u8)> {
    let s = std::str::from_utf8(data).ok()?;
    let rgb_start = s.find("rgb:")?;
    let rgb_part = &s[rgb_start + 4..];
    // Isolate the color value before any terminator
    let rgb_end = rgb_part.find(['\x07', '\x1b'])?;
    let rgb_value = &rgb_part[..rgb_end];
    let parts: Vec<&str> = rgb_value.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let r = parse_color_component(parts[0])?;
    let g = parse_color_component(parts[1])?;
    let b = parse_color_component(parts[2])?;
    Some((r, g, b))
}

/// Parse a hex color component of 1-4 digits, scaling to 8-bit.
fn parse_color_component(s: &str) -> Option<u8> {
    let val = u16::from_str_radix(s, 16).ok()?;
    match s.len() {
        1 => Some((val * 17) as u8), // 0xF -> 0xFF
        2 => Some(val as u8),        // 0xFF -> 0xFF
        3 => Some((val >> 4) as u8), // 0xFFF -> 0xFF
        4 => Some((val >> 8) as u8), // 0xFFFF -> 0xFF
        _ => None,
    }
}

/// Relative luminance (ITU-R BT.601).
fn luminance(r: u8, g: u8, b: u8) -> f64 {
    0.299 * (r as f64) + 0.587 * (g as f64) + 0.114 * (b as f64)
}

/// Alpha-blend a foreground value onto a background value.
pub(super) fn blend_channel(fg: u8, bg: u8, alpha: f64) -> u8 {
    let v = fg as f64 * alpha + bg as f64 * (1.0 - alpha);
    v.round().clamp(0.0, 255.0) as u8
}

/// Map an RGB color to the best representation for the current terminal.
fn best_color(r: u8, g: u8, b: u8) -> Option<Color> {
    if supports_truecolor() {
        return Some(Color::Rgb(r, g, b));
    }
    if supports_256color() {
        return Some(Color::Indexed(nearest_xterm256(r, g, b)));
    }
    None
}

pub(super) fn supports_truecolor() -> bool {
    if let Ok(ct) = std::env::var("COLORTERM") {
        matches!(ct.as_str(), "truecolor" | "24bit")
    } else {
        false
    }
}

fn supports_256color() -> bool {
    if supports_truecolor() {
        return true;
    }
    if let Ok(term) = std::env::var("TERM") {
        term.contains("256color")
    } else {
        false
    }
}

/// Find the nearest color in the xterm-256 color cube (16-231)
/// plus the grayscale ramp (232-255).
fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let cube_vals: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_cube_idx = |v: u8| -> usize {
        cube_vals
            .iter()
            .enumerate()
            .min_by_key(|(_, &cv)| (v as i16 - cv as i16).unsigned_abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let ri = nearest_cube_idx(r);
    let gi = nearest_cube_idx(g);
    let bi = nearest_cube_idx(b);
    let cube_index = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_dist = color_dist(r, g, b, cube_vals[ri], cube_vals[gi], cube_vals[bi]);

    let gray_avg = ((r as u16 + g as u16 + b as u16) / 3) as u8;
    let gray_idx = if gray_avg < 4 {
        0u8
    } else {
        (((gray_avg as u16 - 8) / 10).min(23)) as u8
    };
    let gray_val = 8 + 10 * gray_idx;
    let gray_index = 232 + gray_idx;
    let gray_dist = color_dist(r, g, b, gray_val, gray_val, gray_val);

    if gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

fn color_dist(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = (r1 as i32 - r2 as i32).unsigned_abs();
    let dg = (g1 as i32 - g2 as i32).unsigned_abs();
    let db = (b1 as i32 - b2 as i32).unsigned_abs();
    dr * dr + dg * dg + db * db
}

/// Compute the user message background color.
/// Blends a subtle tint onto the detected terminal background.
pub(super) fn user_message_bg() -> Option<Color> {
    let (bg_r, bg_g, bg_b) = *TERMINAL_BG.get()?.as_ref()?;
    let is_light = luminance(bg_r, bg_g, bg_b) > 128.0;

    let (fg_r, fg_g, fg_b, alpha) = if is_light {
        (0u8, 0u8, 0u8, 0.04)
    } else {
        (255u8, 255u8, 255u8, 0.12)
    };

    let r = blend_channel(fg_r, bg_r, alpha);
    let g = blend_channel(fg_g, bg_g, alpha);
    let b = blend_channel(fg_b, bg_b, alpha);

    best_color(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc11_4digit() {
        let data = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07";
        assert_eq!(parse_osc11_response(data), Some((0x1c, 0x1c, 0x1c)));
    }

    #[test]
    fn parse_osc11_2digit() {
        let data = b"\x1b]11;rgb:ff/00/80\x07";
        assert_eq!(parse_osc11_response(data), Some((0xff, 0x00, 0x80)));
    }

    #[test]
    fn parse_osc11_st_terminated() {
        let data = b"\x1b]11;rgb:ffff/0000/0000\x1b\\";
        assert_eq!(parse_osc11_response(data), Some((0xff, 0x00, 0x00)));
    }

    #[test]
    fn parse_osc11_3digit() {
        let data = b"\x1b]11;rgb:fff/000/888\x07";
        assert_eq!(parse_osc11_response(data), Some((0xff, 0x00, 0x88)));
    }

    #[test]
    fn parse_osc11_1digit() {
        let data = b"\x1b]11;rgb:f/0/8\x07";
        assert_eq!(parse_osc11_response(data), Some((0xff, 0x00, 0x88)));
    }

    #[test]
    fn parse_osc11_malformed() {
        assert_eq!(parse_osc11_response(b"garbage"), None);
        assert_eq!(parse_osc11_response(b"\x1b]11;rgb:ff/00\x07"), None);
        assert_eq!(parse_osc11_response(b""), None);
    }

    #[test]
    fn nearest_xterm256_black() {
        assert_eq!(nearest_xterm256(0, 0, 0), 16); // cube index for (0,0,0)
    }

    #[test]
    fn nearest_xterm256_white() {
        assert_eq!(nearest_xterm256(255, 255, 255), 231); // cube index for (255,255,255)
    }

    #[test]
    fn nearest_xterm256_gray() {
        // Mid-gray should map to grayscale ramp
        let idx = nearest_xterm256(128, 128, 128);
        assert!(idx >= 232 && idx <= 255);
    }

    #[test]
    fn blend_channel_zero_alpha() {
        assert_eq!(blend_channel(255, 30, 0.0), 30);
    }

    #[test]
    fn blend_channel_full_alpha() {
        assert_eq!(blend_channel(255, 30, 1.0), 255);
    }

    #[test]
    fn luminance_black() {
        assert_eq!(luminance(0, 0, 0), 0.0);
    }

    #[test]
    fn luminance_white() {
        assert!((luminance(255, 255, 255) - 255.0).abs() < 0.01);
    }

    #[test]
    fn parse_color_component_all_lengths() {
        assert_eq!(parse_color_component("f"), Some(0xff));
        assert_eq!(parse_color_component("1c"), Some(0x1c));
        assert_eq!(parse_color_component("fff"), Some(0xff));
        assert_eq!(parse_color_component("1c1c"), Some(0x1c));
        assert_eq!(parse_color_component(""), None);
        assert_eq!(parse_color_component("zzz"), None);
    }
}
