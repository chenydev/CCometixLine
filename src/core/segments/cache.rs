use super::{Segment, SegmentData};
use crate::config::{AnsiColor, CurrentUsage, InputData, SegmentId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Prompt cache TTL (1 hour)
const CACHE_TTL_SECS: u64 = 3600;
/// Session state files older than this are pruned
const STATE_MAX_AGE_SECS: u64 = 2 * 24 * 3600;

/// Minimum WCAG contrast for status text on a segment background
const MIN_CONTRAST: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Status {
    Good,
    Warn,
    Bad,
    /// Bad + blinking
    Critical,
    Muted,
}

impl Status {
    /// ANSI 16 code, used when the segment has no background
    fn ansi16(self) -> u8 {
        match self {
            Status::Good => 32,
            Status::Warn => 33,
            Status::Bad | Status::Critical => 31,
            Status::Muted => 90,
        }
    }

    /// (variant for dark backgrounds, variant for light backgrounds)
    fn rgb_variants(self) -> ([u8; 3], [u8; 3]) {
        match self {
            Status::Good => ([166, 227, 161], [26, 127, 55]),
            Status::Warn => ([249, 226, 175], [154, 103, 0]),
            Status::Bad | Status::Critical => ([243, 139, 168], [207, 34, 46]),
            Status::Muted => ([147, 153, 178], [87, 96, 106]),
        }
    }
}

/// Colors status text so it stays readable on the segment background
struct Painter {
    background: Option<[u8; 3]>,
}

impl Painter {
    fn paint(&self, status: Status, text: &str) -> String {
        let blink = if status == Status::Critical { "5;" } else { "" };
        // Only reset fg/blink/bg individually so the segment background survives
        let Some(bg) = self.background else {
            return format!("\x1b[{}{}m{}\x1b[25;39m", blink, status.ansi16(), text);
        };

        let (on_dark, on_light) = status.rgb_variants();
        let color = if contrast(on_dark, bg) >= contrast(on_light, bg) {
            on_dark
        } else {
            on_light
        };

        if contrast(color, bg) >= MIN_CONTRAST {
            format!("\x1b[{}{}m{}\x1b[25;39m", blink, fg_rgb(color), text)
        } else {
            // Background too close to the status color: draw a badge instead
            let text_color = if contrast([0, 0, 0], color) >= contrast([255, 255, 255], color) {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            };
            format!(
                "\x1b[{}{};{}m {} \x1b[25;39;{}m",
                blink,
                bg_rgb(color),
                fg_rgb(text_color),
                text,
                bg_rgb(bg)
            )
        }
    }
}

fn fg_rgb(c: [u8; 3]) -> String {
    format!("38;2;{};{};{}", c[0], c[1], c[2])
}

fn bg_rgb(c: [u8; 3]) -> String {
    format!("48;2;{};{};{}", c[0], c[1], c[2])
}

fn relative_luminance(c: [u8; 3]) -> f64 {
    let channel = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
}

/// WCAG 2 contrast ratio
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Approximate RGB of any configured color (xterm palette)
fn to_rgb(color: &AnsiColor) -> [u8; 3] {
    const ANSI16: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match color {
        AnsiColor::Rgb { r, g, b } => [*r, *g, *b],
        AnsiColor::Color16 { c16 } => ANSI16[(*c16 as usize) % 16],
        AnsiColor::Color256 { c256 } => match *c256 {
            0..=15 => ANSI16[*c256 as usize],
            16..=231 => {
                let i = c256 - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [level(i / 36), level((i / 6) % 6), level(i % 6)]
            }
            _ => {
                let v = 8 + (c256 - 232) * 10;
                [v, v, v]
            }
        },
    }
}

/// Per-session state: when the last new response arrived
#[derive(Debug, Serialize, Deserialize)]
struct SessionState {
    session_id: String,
    signature: String,
    last_response_ts: u64,
    hit_rate: f64,
}

/// Global state: hit rate of the most recent response across sessions
#[derive(Debug, Serialize, Deserialize)]
struct LastState {
    hit_rate: f64,
}

#[derive(Default)]
pub struct CacheSegment {
    background: Option<AnsiColor>,
}

impl CacheSegment {
    pub fn new() -> Self {
        Self::default()
    }

    /// Segment background, used to keep status colors readable
    pub fn with_background(mut self, background: Option<AnsiColor>) -> Self {
        self.background = background;
        self
    }
}

impl Segment for CacheSegment {
    fn collect(&self, input: &InputData) -> Option<SegmentData> {
        let session_key = input
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| input.transcript_path.clone());
        let now = now_secs();

        let usage = input
            .context_window
            .as_ref()
            .and_then(|cw| cw.current_usage.as_ref());

        let mut metadata = HashMap::new();
        let painter = Painter {
            background: self.background.as_ref().map(to_rgb),
        };

        let (hit_rate, last_response_ts, is_fallback) = match usage
            .and_then(|u| hit_rate(u).map(|rate| (rate, signature(u))))
        {
            Some((rate, sig)) => {
                let state_path = state_dir().map(|d| d.join(state_file_name(&session_key)));
                let prev = state_path
                    .as_ref()
                    .and_then(|p| read_session_state(p, &session_key, now));

                let ts = match prev {
                    Some(state) if state.signature == sig => state.last_response_ts,
                    _ => {
                        // New response (or first time seeing this session): reset TTL
                        if let Some(path) = &state_path {
                            if !path.exists() {
                                prune_old_states(now);
                            }
                            let state = SessionState {
                                session_id: session_key.clone(),
                                signature: sig,
                                last_response_ts: now,
                                hit_rate: rate,
                            };
                            write_json(path, &state);
                        }
                        if let Some(dir) = state_dir() {
                            write_json(&dir.join("last.json"), &LastState { hit_rate: rate });
                        }
                        now
                    }
                };
                (Some(rate), Some(ts), false)
            }
            None => {
                // Before the first response of a session: show the previous hit rate
                let session_state = state_dir().and_then(|d| {
                    read_session_state(&d.join(state_file_name(&session_key)), &session_key, now)
                });
                match session_state {
                    Some(state) => (Some(state.hit_rate), Some(state.last_response_ts), false),
                    None => (read_last_hit_rate(), None, true),
                }
            }
        };

        let primary = match hit_rate {
            Some(rate) => {
                metadata.insert("hit_rate".to_string(), format!("{:.1}", rate));
                let status = if rate >= 50.0 && !is_fallback {
                    Status::Good
                } else {
                    Status::Muted
                };
                painter.paint(status, &format!("{:.0}%", rate))
            }
            None => painter.paint(Status::Muted, "-"),
        };

        let secondary = match last_response_ts {
            Some(ts) => {
                let elapsed = now.saturating_sub(ts);
                metadata.insert("elapsed_secs".to_string(), elapsed.to_string());
                let (status, text) = ttl_status(elapsed);
                format!("· {}", painter.paint(status, &text))
            }
            None => String::new(),
        };

        Some(SegmentData {
            primary,
            secondary,
            metadata,
        })
    }

    fn id(&self) -> SegmentId {
        SegmentId::Cache
    }
}

/// cache_read / (input + cache_creation + cache_read), in percent
fn hit_rate(usage: &CurrentUsage) -> Option<f64> {
    let total =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
    if total == 0 {
        return None;
    }
    Some(usage.cache_read_input_tokens as f64 / total as f64 * 100.0)
}

/// Changes only when a new response arrives
fn signature(usage: &CurrentUsage) -> String {
    format!(
        "{}-{}-{}",
        usage.input_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens
    )
}

/// Remaining TTL as MM:SS, with a status derived from elapsed time
fn ttl_status(elapsed: u64) -> (Status, String) {
    if elapsed >= CACHE_TTL_SECS {
        return (Status::Muted, "exp".to_string());
    }
    let remaining = CACHE_TTL_SECS - elapsed;
    let status = match elapsed {
        0..1200 => Status::Good,
        1200..2400 => Status::Warn,
        2400..3300 => Status::Bad,
        _ => Status::Critical,
    };
    (
        status,
        format!("{:02}:{:02}", remaining / 60, remaining % 60),
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_dir() -> Option<PathBuf> {
    let dir = dirs::home_dir()?
        .join(".claude")
        .join("ccline")
        .join("cache");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Stable FNV-1a hash so file names survive toolchain upgrades
fn state_file_name(session_key: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in session_key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("session-{:016x}.json", hash)
}

/// Read and validate a session state; reject anything inconsistent
fn read_session_state(path: &PathBuf, session_key: &str, now: u64) -> Option<SessionState> {
    let content = fs::read_to_string(path).ok()?;
    let state: SessionState = serde_json::from_str(&content).ok()?;
    let valid = state.session_id == session_key
        && !state.signature.is_empty()
        && state.last_response_ts > 0
        && state.last_response_ts <= now + 60
        && state.hit_rate.is_finite()
        && (0.0..=100.0).contains(&state.hit_rate);
    valid.then_some(state)
}

fn read_last_hit_rate() -> Option<f64> {
    let content = fs::read_to_string(state_dir()?.join("last.json")).ok()?;
    let state: LastState = serde_json::from_str(&content).ok()?;
    (state.hit_rate.is_finite() && (0.0..=100.0).contains(&state.hit_rate))
        .then_some(state.hit_rate)
}

/// Atomic write: concurrent statusline processes never see a partial file
fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    let Ok(content) = serde_json::to_string(value) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, content).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn prune_old_states(now: u64) {
    let Some(dir) = state_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("session-") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if let Some(mtime) = modified {
            if now.saturating_sub(mtime) > STATE_MAX_AGE_SECS {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, creation: u64, read: u64) -> CurrentUsage {
        CurrentUsage {
            input_tokens: input,
            output_tokens: 0,
            cache_creation_input_tokens: creation,
            cache_read_input_tokens: read,
        }
    }

    #[test]
    fn computes_hit_rate() {
        assert_eq!(hit_rate(&usage(0, 0, 0)), None);
        assert_eq!(hit_rate(&usage(10, 10, 80)), Some(80.0));
    }

    #[test]
    fn formats_ttl_bands() {
        assert_eq!(ttl_status(0), (Status::Good, "60:00".to_string()));
        assert_eq!(ttl_status(1200), (Status::Warn, "40:00".to_string()));
        assert_eq!(ttl_status(2400), (Status::Bad, "20:00".to_string()));
        assert_eq!(ttl_status(3300), (Status::Critical, "05:00".to_string()));
        assert_eq!(ttl_status(3600), (Status::Muted, "exp".to_string()));
    }

    #[test]
    fn plain_ansi_without_background() {
        let painter = Painter { background: None };
        assert_eq!(painter.paint(Status::Good, "90%"), "\x1b[32m90%\x1b[25;39m");
        assert_eq!(
            painter.paint(Status::Critical, "01:00"),
            "\x1b[5;31m01:00\x1b[25;39m"
        );
    }

    #[test]
    fn readable_on_any_background() {
        let statuses = [
            Status::Good,
            Status::Warn,
            Status::Bad,
            Status::Critical,
            Status::Muted,
        ];
        for bg in [
            [163, 190, 140],
            [40, 167, 69],
            [45, 50, 59],
            [255, 255, 255],
        ] {
            let painter = Painter {
                background: Some(bg),
            };
            for status in statuses {
                let out = painter.paint(status, "x");
                let (on_dark, on_light) = status.rgb_variants();
                let best = contrast(on_dark, bg).max(contrast(on_light, bg));
                // Either readable text, or a badge restoring the segment background
                assert!(best >= MIN_CONTRAST || out.ends_with(&format!("{}m", bg_rgb(bg))));
            }
        }
    }

    #[test]
    fn green_on_green_becomes_badge() {
        let painter = Painter {
            background: Some([163, 190, 140]),
        };
        assert!(painter.paint(Status::Good, "90%").contains("48;2;"));
    }

    #[test]
    fn converts_256_colors() {
        assert_eq!(to_rgb(&AnsiColor::Color256 { c256: 196 }), [255, 0, 0]);
        assert_eq!(to_rgb(&AnsiColor::Color256 { c256: 232 }), [8, 8, 8]);
    }

    #[test]
    fn state_file_name_is_stable() {
        assert_eq!(state_file_name("abc"), state_file_name("abc"));
        assert_ne!(state_file_name("abc"), state_file_name("abd"));
    }
}
