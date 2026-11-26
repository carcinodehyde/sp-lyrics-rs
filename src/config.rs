//! Portable settings stored in `sp-lyrics-rs.ini` next to the exe.

use std::path::PathBuf;

pub struct Config {
    pub lyrics_dir: String,
    /// Substring of the media session's app id to follow ("spotify"; empty = any player).
    pub player: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub font: String,
    pub font_size: f32,
    pub bold: bool,
    pub color_active: u32,
    pub color_fill: u32,
    pub color_inactive: u32,
    pub karaoke: bool,
    pub locked: bool,
    /// Extra global timing offset in ms (positive = lyrics earlier).
    pub offset_ms: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lyrics_dir: "lrc".into(),
            player: "spotify".into(),
            x: i32::MIN,
            y: i32::MIN,
            w: 900,
            h: 170,
            font: "Segoe UI".into(),
            font_size: 30.0,
            bold: true,
            color_active: 0xFFFFFF,
            color_fill: 0x4FC3F7,
            color_inactive: 0xD8D8D8,
            karaoke: true,
            locked: false,
            offset_ms: 0,
        }
    }
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Into::into))
        .unwrap_or_else(|| ".".into())
}

fn path() -> PathBuf {
    exe_dir().join("sp-lyrics-rs.ini")
}

/// Settings file used before the rename to sp-lyrics-rs; read once if the new one is missing.
fn legacy_path() -> PathBuf {
    exe_dir().join("sp-lyrics.ini")
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('#'), 16).ok()
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(path()).or_else(|_| std::fs::read_to_string(legacy_path())) else { return c };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "lyrics_dir" => c.lyrics_dir = v.into(),
                "player" => c.player = v.into(),
                "x" => c.x = v.parse().unwrap_or(c.x),
                "y" => c.y = v.parse().unwrap_or(c.y),
                "width" => c.w = v.parse().unwrap_or(c.w),
                "height" => c.h = v.parse().unwrap_or(c.h),
                "font" => c.font = v.into(),
                "font_size" => c.font_size = v.parse().unwrap_or(c.font_size),
                "bold" => c.bold = v == "1" || v == "true",
                "color_active" => c.color_active = hex(v).unwrap_or(c.color_active),
                "color_fill" => c.color_fill = hex(v).unwrap_or(c.color_fill),
                "color_inactive" => c.color_inactive = hex(v).unwrap_or(c.color_inactive),
                "karaoke" => c.karaoke = v == "1" || v == "true",
                "locked" => c.locked = v == "1" || v == "true",
                "offset_ms" => c.offset_ms = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        c.font_size = c.font_size.clamp(8.0, 200.0);
        c
    }

    pub fn save(&self) {
        let b = |v: bool| if v { 1 } else { 0 };
        let text = format!(
            "; sp-lyrics-rs settings\r\n\
             ; lyrics_dir: folder with \"artist - title.lrc\" files (relative to the exe or absolute)\r\n\
             ; player: part of the media app id to follow (spotify); leave empty to follow any player\r\n\
             lyrics_dir={}\r\nplayer={}\r\nx={}\r\ny={}\r\nwidth={}\r\nheight={}\r\n\
             font={}\r\nfont_size={}\r\nbold={}\r\n\
             color_active={:06X}\r\ncolor_fill={:06X}\r\ncolor_inactive={:06X}\r\n\
             karaoke={}\r\nlocked={}\r\noffset_ms={}\r\n",
            self.lyrics_dir, self.player, self.x, self.y, self.w, self.h,
            self.font, self.font_size, b(self.bold),
            self.color_active, self.color_fill, self.color_inactive,
            b(self.karaoke), b(self.locked), self.offset_ms,
        );
        let _ = std::fs::write(path(), text);
    }

    pub fn lyrics_path(&self) -> PathBuf {
        let p = PathBuf::from(&self.lyrics_dir);
        if p.is_absolute() { p } else { exe_dir().join(p) }
    }
}
