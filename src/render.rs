//! Direct2D/DirectWrite rendering into a per-pixel-alpha layered window.

use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use windows_numerics::Vector2;

use crate::App;
use crate::config::Config;
use crate::lrc;
use crate::player;

// Glyphs from Windows' built-in Segoe Fluent Icons / Segoe MDL2 Assets font (same code points).
const ICON_SYNC: char = '\u{E895}';
const ICON_OFFSET: char = '\u{E823}';
const ICON_LOCK: char = '\u{E72E}';
const ICON_UNLOCK: char = '\u{E785}';
const ICON_UNDO: char = '\u{E7A7}';
const ICON_CANCEL: char = '\u{E711}';
const ICON_SAVE: char = '\u{E74E}';
const ICON_MINUS: char = '\u{E738}';
const ICON_PLUS: char = '\u{E710}';
const ICON_RESET: char = '\u{E72C}';
const ICON_DONE: char = '\u{E8FB}';
const ICON_MINIMIZE: char = '\u{E921}';
const ICON_GAP: char = '\u{EC4F}'; // MusicNote

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Btn {
    Label,
    Lock,
    Minimize,
    Sync,
    SyncUndo,
    SyncGap,
    SyncSave,
    SyncCancel,
    Offset,
    OffMinus,
    OffPlus,
    OffReset,
}

/// Rectangle in window pixels.
#[derive(Clone, Copy)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
}

struct LineLayout {
    layout: IDWriteTextLayout,
    h: f32,
    left: f32,
    w: f32,
    rows: u32,
}

pub struct Renderer {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    rt: Option<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)>,
    fmt: IDWriteTextFormat,
    fmt_small: IDWriteTextFormat,
    fmt_header: IDWriteTextFormat,
    fmt_icon: IDWriteTextFormat,
    scale: f32,
    memdc: HDC,
    bmp: HBITMAP,
    size: (i32, i32),
    lines: Vec<LineLayout>,
    layout_width: f32,
    scroll: f32,
    snap: bool,
    last: Instant,
}

fn col(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a,
    }
}

fn pt(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl Renderer {
    pub fn new(cfg: &Config, scale: f32) -> Result<Renderer> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let (fmt, fmt_small, fmt_header, fmt_icon) = make_formats(&dw, cfg, scale)?;
            Ok(Renderer {
                d2d,
                dw,
                rt: None,
                fmt,
                fmt_small,
                fmt_header,
                fmt_icon,
                scale,
                memdc: CreateCompatibleDC(None),
                bmp: HBITMAP::default(),
                size: (0, 0),
                lines: Vec::new(),
                layout_width: 0.0,
                scroll: 0.0,
                snap: true,
                last: Instant::now(),
            })
        }
    }

    pub fn reset_lines(&mut self) {
        self.lines.clear();
        self.layout_width = 0.0;
        self.snap = true;
    }

    pub fn rebuild_formats(&mut self, cfg: &Config) {
        if let Ok((a, b, c, d)) = unsafe { make_formats(&self.dw, cfg, self.scale) } {
            self.fmt = a;
            self.fmt_small = b;
            self.fmt_header = c;
            self.fmt_icon = d;
        }
        self.reset_lines();
    }

    pub fn set_scale(&mut self, scale: f32, cfg: &Config) {
        self.scale = scale;
        self.rebuild_formats(cfg);
    }

    unsafe fn ensure_surface(&mut self, w: i32, h: i32) -> bool {
        if self.size == (w, h) && !self.bmp.is_invalid() {
            return true;
        }
        unsafe {
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bmp) = CreateDIBSection(Some(self.memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else { return false };
            SelectObject(self.memdc, bmp.into());
            if !self.bmp.is_invalid() {
                let _ = DeleteObject(self.bmp.into());
            }
            self.bmp = bmp;
            self.size = (w, h);
        }
        true
    }

    fn target(&mut self) -> Option<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)> {
        if self.rt.is_none() {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE, // small surface: CPU raster avoids GPU readback and driver memory
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            unsafe {
                let rt = self.d2d.CreateDCRenderTarget(&props).ok()?;
                let brush = rt.CreateSolidColorBrush(&col(0xFFFFFF, 1.0), None).ok()?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                self.rt = Some((rt, brush));
            }
        }
        self.rt.clone()
    }

    fn layout(&self, text: &str, fmt: &IDWriteTextFormat, maxw: f32) -> Option<LineLayout> {
        unsafe {
            let layout = self.dw.CreateTextLayout(&wstr(text), fmt, maxw.max(1.0), 10_000.0).ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).ok()?;
            Some(LineLayout { layout, h: m.height, left: m.left, w: m.width, rows: m.lineCount })
        }
    }
}

unsafe fn make_formats(dw: &IDWriteFactory, cfg: &Config, scale: f32) -> Result<(IDWriteTextFormat, IDWriteTextFormat, IDWriteTextFormat, IDWriteTextFormat)> {
    let face: Vec<u16> = cfg.font.encode_utf16().chain(Some(0)).collect();
    let mk_face = |face: PCWSTR, size: f32, weight: DWRITE_FONT_WEIGHT| -> Result<IDWriteTextFormat> {
        unsafe {
            let f = dw.CreateTextFormat(
                face,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
            Ok(f)
        }
    };
    let mk = |size: f32, weight: DWRITE_FONT_WEIGHT| mk_face(PCWSTR(face.as_ptr()), size, weight);
    let weight = if cfg.bold { DWRITE_FONT_WEIGHT_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
    let small = (cfg.font_size * 0.45).clamp(11.0, 18.0);
    let header = mk(small * scale, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
    unsafe {
        header.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        header.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    }
    let icon = mk_face(unsafe { icon_font(dw) }, small * scale, DWRITE_FONT_WEIGHT_NORMAL)?;
    Ok((mk(cfg.font_size * scale, weight)?, mk(small * scale, DWRITE_FONT_WEIGHT_SEMI_BOLD)?, header, icon))
}

/// "Segoe Fluent Icons" (Windows 11), else "Segoe MDL2 Assets" (Windows 10).
unsafe fn icon_font(dw: &IDWriteFactory) -> PCWSTR {
    let fluent = w!("Segoe Fluent Icons");
    unsafe {
        let mut coll = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_ok() {
            if let Some(coll) = coll {
                let (mut idx, mut exists) = (0u32, BOOL(0));
                if coll.FindFamilyName(fluent, &mut idx, &mut exists).is_ok() && exists.as_bool() {
                    return fluent;
                }
            }
        }
    }
    w!("Segoe MDL2 Assets")
}

/// Text with a dark outline so it stays readable on any background.
unsafe fn draw_text(rt: &ID2D1DCRenderTarget, brush: &ID2D1SolidColorBrush, l: &IDWriteTextLayout, x: f32, y: f32, color: D2D1_COLOR_F, o: f32) {
    unsafe {
        brush.SetColor(&col(0, 0.35 * color.a));
        rt.DrawTextLayout(pt(x + o * 1.5, y + o * 2.0), l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        brush.SetColor(&col(0, 0.6 * color.a));
        for (dx, dy) in [(-o, -o), (o, -o), (-o, o), (o, o)] {
            rt.DrawTextLayout(pt(x + dx, y + dy), l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
        brush.SetColor(&color);
        rt.DrawTextLayout(pt(x, y), l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
    }
}

/// Draws one frame and pushes it to the window. Returns true while an animation is running.
pub unsafe fn render(app: &mut App, st: &player::State) -> bool {
    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(app.hwnd, &mut wr);
    }
    let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
    if w <= 0 || h <= 0 {
        return false;
    }
    let hovered = app.hovered();
    let pos = app.lyric_pos(st);
    let sync_next = app.sync.as_ref().map(|s| s.times.len());
    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let (mx, my) = ((cursor.x - wr.left) as f32, (cursor.y - wr.top) as f32);
    let r = &mut app.r;
    if !unsafe { r.ensure_surface(w, h) } {
        return false;
    }
    let Some((rt, brush)) = r.target() else { return false };
    let (wf, hf) = (w as f32, h as f32);
    let s = r.scale;
    let pad = 14.0 * s;
    let cfg = &app.cfg;
    let font_px = cfg.font_size * s;
    let o = (font_px / 24.0).max(1.0);
    let mut animating = false;

    let dt = r.last.elapsed().as_secs_f32().min(0.5);
    r.last = Instant::now();

    unsafe {
        if rt.BindDC(r.memdc, &RECT { left: 0, top: 0, right: w, bottom: h }).is_err() {
            r.rt = None;
            return false;
        }
        rt.BeginDraw();
        // Nearly-invisible background keeps the window grabbable while unlocked.
        // Locked or minimized: fully transparent (main.rs also makes it click-through).
        let passive = cfg.locked || cfg.minimized;
        rt.Clear(Some(&col(0, if passive { 0.0 } else { 1.0 / 255.0 })));
        if hovered && !passive {
            brush.SetColor(&col(0x101418, 0.55));
            let rr = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 0.5, top: 0.5, right: wf - 0.5, bottom: hf - 0.5 },
                radiusX: 10.0 * s,
                radiusY: 10.0 * s,
            };
            rt.FillRoundedRectangle(&rr, &brush);
            brush.SetColor(&col(0xFFFFFF, 0.18));
            rt.DrawRoundedRectangle(&rr, &brush, 1.0, None);
        }

        let maxw = wf - 2.0 * pad;
        let status = |r: &Renderer, big: &str, small: Option<&str>| {
            let a = r.layout(big, &r.fmt, maxw);
            let b = small.and_then(|t| r.layout(t, &r.fmt_small, maxw));
            let total = a.as_ref().map_or(0.0, |l| l.h) + b.as_ref().map_or(0.0, |l| l.h + 4.0 * s);
            let mut y = (hf - total) / 2.0;
            if let Some(a) = a {
                draw_text(&rt, &brush, &a.layout, pad, y, col(cfg.color_active, 0.85), o);
                y += a.h + 4.0 * s;
            }
            if let Some(b) = b {
                draw_text(&rt, &brush, &b.layout, pad, y, col(cfg.color_inactive, 0.75), 1.0);
            }
        };

        match (&app.track, &app.loaded) {
            _ if cfg.minimized => {} // lyrics hidden
            (None, _) => status(r, "\u{266A}", Some("Waiting for Spotify to play something\u{2026}")),
            (Some(t), None) => {
                let big = format!("{} \u{2014} {}", t.artist, t.title);
                let small = format!("No lyrics found in folder ({} .lrc files)", app.index_len);
                status(r, &big, Some(&small));
            }
            (Some(_), Some(l)) if l.lyrics.lines.is_empty() => status(r, "\u{266A}", Some("This .lrc file has no timed lines")),
            (Some(_), Some(l)) => {
                let lines = &l.lyrics.lines;
                if r.layout_width != maxw || r.lines.len() != lines.len() {
                    r.layout_width = maxw;
                    r.lines = lines
                        .iter()
                        .filter_map(|ln| r.layout(if ln.text.is_empty() { "\u{266A}" } else { &ln.text }, &r.fmt, maxw))
                        .collect();
                    if r.lines.len() != lines.len() {
                        r.lines.clear(); // layout failure; try again next frame
                    }
                }
                if !r.lines.is_empty() {
                    let gap = font_px * 0.3;
                    let mut ys = Vec::with_capacity(lines.len());
                    let mut y = 0.0;
                    for ll in &r.lines {
                        ys.push(y);
                        y += ll.h + gap;
                    }
                    // In sync mode the highlighted line is the one the next Space press stamps.
                    let cur = match sync_next {
                        Some(n) => Some(n.min(lines.len() - 1)),
                        None => l.lyrics.line_at(pos),
                    };
                    let target = match cur {
                        Some(i) => ys[i] + r.lines[i].h / 2.0,
                        None => -gap - font_px * 0.5,
                    };
                    if r.snap || (target - r.scroll).abs() > hf * 3.0 {
                        r.scroll = target;
                        r.snap = false;
                    } else {
                        r.scroll += (target - r.scroll) * (1.0 - (-dt * 12.0).exp());
                    }
                    animating = (target - r.scroll).abs() > 0.5;

                    let cy = hf / 2.0;
                    for (i, ll) in r.lines.iter().enumerate() {
                        let top = cy - r.scroll + ys[i];
                        if top > hf || top + ll.h < 0.0 {
                            continue;
                        }
                        let dist = ((top + ll.h / 2.0 - cy).abs() / cy).min(1.0);
                        let is_cur = cur == Some(i);
                        if is_cur {
                            let t0 = lines[i].time_ms;
                            let t1 = lines[i + 1..].iter().map(|l| l.time_ms).find(|&t| t > t0 && t != lrc::UNTIMED).unwrap_or(t0 + 4000);
                            let p = ((pos - t0) as f32 / (t1 - t0).max(1) as f32).clamp(0.0, 1.0);
                            // No karaoke fill on a gap's note glyph.
                            if cfg.karaoke && l.lyrics.synced && sync_next.is_none() && ll.rows == 1 && !lines[i].text.is_empty() {
                                draw_text(&rt, &brush, &ll.layout, pad, top, col(cfg.color_active, 1.0), o);
                                let x0 = pad + ll.left;
                                rt.PushAxisAlignedClip(
                                    &D2D_RECT_F { left: x0, top, right: x0 + ll.w * p, bottom: top + ll.h },
                                    D2D1_ANTIALIAS_MODE_ALIASED,
                                );
                                brush.SetColor(&col(cfg.color_fill, 1.0));
                                rt.DrawTextLayout(pt(pad, top), &ll.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                                rt.PopAxisAlignedClip();
                            } else {
                                draw_text(&rt, &brush, &ll.layout, pad, top, col(cfg.color_fill, 1.0), o);
                            }
                        } else {
                            let fade = (1.0 - dist.powf(1.6)).clamp(0.0, 1.0);
                            draw_text(&rt, &brush, &ll.layout, pad, top, col(cfg.color_inactive, 0.7 * fade), o);
                        }
                    }
                }
            }
        }

        // Toolbar while hovered: buttons right-aligned on the top row. When locked, only the
        // (faint) lock button shows and main.rs makes just that spot clickable.
        // Minimized shows nothing at all; it's restored from the tray.
        let mut buttons = Vec::new();
        let mut header_right = wf - pad;
        if hovered && !cfg.minimized {
            let has_lrc = app.loaded.as_ref().is_some_and(|l| !l.lyrics.lines.is_empty());
            let off = app.loaded.as_ref().map_or(0, |l| l.lyrics.offset_ms);
            // (button, icon glyph from the Segoe icon font, label)
            let mut items: Vec<(Btn, Option<char>, String)> = if cfg.locked {
                Vec::new()
            } else if let Some(n) = sync_next {
                let mut v = vec![
                    (Btn::SyncGap, Some(ICON_GAP), "Gap".into()),
                    (Btn::SyncUndo, Some(ICON_UNDO), "Undo".into()),
                    (Btn::SyncCancel, Some(ICON_CANCEL), "Cancel".into()),
                ];
                if n > 0 {
                    v.push((Btn::SyncSave, Some(ICON_SAVE), "Save now".into()));
                }
                v
            } else if app.offset_mode {
                vec![
                    (Btn::Label, None, "Wheel or".into()),
                    (Btn::OffMinus, Some(ICON_MINUS), String::new()),
                    (Btn::Label, None, format!("{:+.1}s", off as f32 / 1000.0)),
                    (Btn::OffPlus, Some(ICON_PLUS), String::new()),
                    (Btn::OffReset, Some(ICON_RESET), "Reset".into()),
                    (Btn::Offset, Some(ICON_DONE), "Done".into()),
                ]
            } else if has_lrc {
                let partial = app.loaded.as_ref().is_some_and(|l| (1..l.lyrics.lines.len()).contains(&l.lyrics.timed_count()));
                vec![
                    (Btn::Sync, Some(ICON_SYNC), if partial { "Resume sync" } else { "Sync" }.into()),
                    (Btn::Offset, Some(ICON_OFFSET), "Offset".into()),
                ]
            } else {
                Vec::new()
            };
            if !cfg.locked && sync_next.is_none() {
                items.push((Btn::Minimize, Some(ICON_MINIMIZE), String::new()));
            }
            items.push((Btn::Lock, Some(if cfg.locked { ICON_LOCK } else { ICON_UNLOCK }), String::new()));

            let y = 5.0 * s;
            let mut x = wf - 8.0 * s;
            for (b, icon, label) in items.iter().rev() {
                let il = icon.and_then(|c| r.layout(&c.to_string(), &r.fmt_icon, 1000.0));
                let ll = if label.is_empty() { None } else { r.layout(label, &r.fmt_small, 1000.0) };
                let text_h = ll.as_ref().map_or(0.0, |l| l.h).max(il.as_ref().map_or(0.0, |l| l.h));
                let gap = if il.is_some() && ll.is_some() { 6.0 * s } else { 0.0 };
                let content_w = il.as_ref().map_or(0.0, |l| l.w) + gap + ll.as_ref().map_or(0.0, |l| l.w);
                let side = if label.is_empty() { 7.0 * s } else { 9.0 * s };
                let (bw, bh) = (content_w + 2.0 * side, text_h + 6.0 * s);
                x -= bw;
                let rect = Rect { l: x, t: y, r: x + bw, b: y + bh };
                let hot = *b != Btn::Label && rect.contains(mx, my);
                // The locked-mode lock button stays faint until pointed at.
                let alpha = if passive && !hot { 0.45 } else { 1.0 };
                if *b != Btn::Label {
                    let bg = if hot {
                        col(0xFFFFFF, 0.3)
                    } else if *b == Btn::Offset && app.offset_mode {
                        col(cfg.color_fill, 0.45)
                    } else if passive {
                        col(0x101418, 0.55 * alpha) // dark: must stay visible over any background
                    } else {
                        col(0xFFFFFF, 0.13)
                    };
                    brush.SetColor(&bg);
                    let rr = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F { left: rect.l, top: rect.t, right: rect.r, bottom: rect.b },
                        radiusX: 6.0 * s,
                        radiusY: 6.0 * s,
                    };
                    rt.FillRoundedRectangle(&rr, &brush);
                    buttons.push((*b, rect));
                }
                brush.SetColor(&col(0xFFFFFF, if *b == Btn::Label { 0.75 } else { 0.95 * alpha }));
                let mut cx = x + side;
                if let Some(il) = &il {
                    let iy = y + 3.0 * s + (text_h - il.h) / 2.0;
                    rt.DrawTextLayout(pt(cx - il.left, iy), &il.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                    cx += il.w + gap;
                }
                if let Some(ll) = &ll {
                    let ly = y + 3.0 * s + (text_h - ll.h) / 2.0;
                    rt.DrawTextLayout(pt(cx - ll.left, ly), &ll.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
                x -= 4.0 * s;
            }
            header_right = x - 6.0 * s;
        }

        // Header: recent action feedback, sync instructions, or track info on hover.
        let toast = app.toast.as_ref().filter(|(_, t)| t.elapsed().as_secs_f32() < 2.0).map(|(m, _)| m.clone());
        let sync_text = sync_next.zip(app.loaded.as_ref()).map(|(n, l)| {
            format!(
                "SYNC {n}/{}  \u{00B7}  Space: line starts  \u{00B7}  Backspace: undo  \u{00B7}  Esc: cancel",
                l.lyrics.lines.len()
            )
        });
        let header = toast.clone().or(sync_text).or_else(|| {
            (hovered && !passive).then(|| match &app.track {
                Some(t) => {
                    let note = match app.loaded.as_ref().map(|l| (l.lyrics.timed_count(), l.lyrics.lines.len())) {
                        Some((0, _)) => "  (unsynced)".to_owned(),
                        Some((t, n)) if t < n => format!("  (synced {t}/{n})"),
                        _ => String::new(),
                    };
                    format!("{} \u{2014} {}{note}", t.artist, t.title)
                }
                None => "right-click for menu".to_owned(),
            })
        });
        if let Some(text) = header {
            if let Some(ll) = r.layout(&text, &r.fmt_header, 10_000.0) {
                let y = 8.0 * s;
                let right = header_right.max(pad + 40.0 * s);
                if toast.is_some() || sync_next.is_some() {
                    brush.SetColor(&col(0x101418, 0.8));
                    let rr = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: pad - 8.0 * s,
                            top: y - 3.0 * s,
                            right: (pad + ll.w + 8.0 * s).min(right + 8.0 * s),
                            bottom: y + ll.h + 3.0 * s,
                        },
                        radiusX: 6.0 * s,
                        radiusY: 6.0 * s,
                    };
                    rt.FillRoundedRectangle(&rr, &brush);
                    animating |= toast.is_some();
                }
                rt.PushAxisAlignedClip(&D2D_RECT_F { left: 0.0, top: 0.0, right, bottom: hf }, D2D1_ANTIALIAS_MODE_ALIASED);
                brush.SetColor(&col(0xFFFFFF, 0.9));
                rt.DrawTextLayout(pt(pad, y), &ll.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                rt.PopAxisAlignedClip();
            }
        }
        app.buttons = buttons;

        if rt.EndDraw(None, None).is_err() {
            r.rt = None; // device lost; recreate next frame
            return true;
        }

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            app.hwnd,
            None,
            None,
            Some(&SIZE { cx: w, cy: h }),
            Some(r.memdc),
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
    animating
}
