#![windows_subsystem = "windows"]
//! sp-lyrics-rs: a tiny always-on-top, transparent synced-lyrics overlay for Spotify.

mod autostart;
mod config;
mod lrc;
mod matcher;
mod player;
mod render;

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use config::Config;
use player::Track;

const WM_TRAY: u32 = WM_APP + 1;
const WM_TRACK: u32 = WM_APP + 2;
const TIMER_FRAME: usize = 1;
const FAST_MS: u32 = 30;
const SLOW_MS: u32 = 250;
/// Karaoke fill sweep: 15 fps looked steppy, so it runs at the full 30 fps.
const KARAOKE_MS: u32 = FAST_MS;

const ID_LOCK: u32 = 100;
const ID_KARAOKE: u32 = 101;
const ID_FONT: u32 = 102;
const ID_BIGGER: u32 = 103;
const ID_SMALLER: u32 = 104;
const ID_EARLIER: u32 = 105;
const ID_LATER: u32 = 106;
const ID_RESET_OFFSET: u32 = 107;
const ID_OPEN_DIR: u32 = 108;
const ID_CHANGE_DIR: u32 = 109;
const ID_OPEN_LRC: u32 = 110;
const ID_RELOAD: u32 = 111;
const ID_EXIT: u32 = 112;
const ID_SYNC: u32 = 113;
const ID_SYNC_SAVE: u32 = 114;
const ID_AUTOSTART: u32 = 115;
const ID_MINIMIZE: u32 = 116;

const HK_SPACE: i32 = 1;
const HK_BACK: i32 = 2;
const HK_ESC: i32 = 3;
const HK_ENTER: i32 = 4;
const OFFSET_STEP: i64 = 100;
const SCRUB_SETTLE_MS: u128 = 400;

pub struct Loaded {
    pub lyrics: lrc::Lyrics,
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
}

/// Tap-to-sync session: `times[i]` is the tapped start of line i. `gaps` are empty lines stamped
/// with Enter (instrumental breaks), saved as bare `[mm:ss.xx]` lines.
#[derive(Default)]
pub struct SyncState {
    pub times: Vec<i64>,
    gaps: Vec<i64>,
    /// Stamp order for undo: true = gap.
    history: Vec<bool>,
}

impl SyncState {
    fn last(&self) -> Option<i64> {
        self.times.last().copied().max(self.gaps.last().copied())
    }

    /// All lines for saving: tapped lines and gaps in time order, then the untapped rest as plain text.
    fn merged<'a>(&self, lines: &'a [lrc::Line]) -> Vec<(Option<i64>, &'a str)> {
        let tapped = self.times.len().min(lines.len());
        let mut out: Vec<(Option<i64>, &str)> = self.gaps.iter().map(|&g| (Some(g), "")).collect();
        out.extend(self.times[..tapped].iter().zip(lines).map(|(&t, ln)| (Some(t), ln.text.as_str())));
        out.sort_by_key(|&(t, _)| t); // stable: a gap stamped at the same time as a line goes first
        out.extend(lines[tapped..].iter().map(|ln| (None, ln.text.as_str())));
        out
    }
}

pub struct App {
    hwnd: HWND,
    pub cfg: Config,
    player: player::Shared,
    cmd: std::sync::mpsc::Sender<player::Cmd>,
    pub sync: Option<SyncState>,
    pub offset_mode: bool,
    /// Mouse-wheel seek preview: the highlighted line and when the wheel last moved.
    /// The seek is sent once the wheel has been still for `SCRUB_SETTLE_MS`.
    pub scrub: Option<(usize, Instant)>,
    pub buttons: Vec<(render::Btn, render::Rect)>,
    track: Option<Track>,
    pub loaded: Option<Loaded>,
    pub index_len: usize,
    last_check: Instant,
    r: render::Renderer,
    timer_ms: u32,
    pub toast: Option<(String, Instant)>,
    icon: HICON,
    taskbar_created: u32,
    /// Whether WS_EX_TRANSPARENT is currently set (locked, and not pointing at the lock button).
    click_through: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state unless it is already borrowed (re-entrant modal loops).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

/// The overlay never has focus, so Windows 11 would treat it as a background process and put it in
/// efficiency mode (EcoQoS): slow cores and late timers, which made the lyrics stall until hovered.
/// The redraw rate is already throttled by `next_frame_ms`, so opting out costs little.
fn opt_out_of_power_throttling() {
    use windows::Win32::System::Threading::*;
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0, // 0 = turn both throttles off
    };
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        opt_out_of_power_throttling();
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        // Single instance.
        let _mutex = CreateMutexW(None, true, w!("sp-lyrics-rs-single-instance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let mut cfg = Config::load();
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let icon = make_icon();
        let class = w!("SpLyricsRsOverlay");
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_SIZEALL)?,
            hIcon: icon,
            lpszClassName: class,
            ..Default::default()
        });

        // Default/validated placement: bottom-center of the primary screen.
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        cfg.w = cfg.w.clamp(200, sw.max(200));
        cfg.h = cfg.h.clamp(60, sh.max(60));
        let rect = RECT { left: cfg.x, top: cfg.y, right: cfg.x.saturating_add(cfg.w), bottom: cfg.y.saturating_add(cfg.h) };
        if cfg.x == i32::MIN || MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid() {
            cfg.x = (sw - cfg.w) / 2;
            cfg.y = sh - cfg.h - 90;
        }

        let mut ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if cfg.locked || cfg.minimized {
            ex |= WS_EX_TRANSPARENT;
        }
        let hwnd = CreateWindowExW(
            ex, class, w!("sp-lyrics-rs"), WS_POPUP | WS_THICKFRAME,
            cfg.x, cfg.y, cfg.w, cfg.h, None, None, Some(hinst), None,
        )?;

        let r = render::Renderer::new(&cfg, GetDpiForWindow(hwnd) as f32 / 96.0)?;
        let hwnd_raw = hwnd.0 as isize;
        let (player, cmd) = player::spawn(cfg.player.clone(), move || {
            let _ = PostMessageW(Some(HWND(hwnd_raw as _)), WM_TRACK, WPARAM(0), LPARAM(0));
        });
        let app = App {
            hwnd,
            cfg,
            player,
            cmd,
            sync: None,
            offset_mode: false,
            scrub: None,
            buttons: Vec::new(),
            track: None,
            loaded: None,
            index_len: 0,
            last_check: Instant::now(),
            r,
            timer_ms: 0,
            toast: None,
            icon,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            click_through: false,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        tray(hwnd, NIM_ADD);
        with_app(|a| {
            a.frame();
            a.set_timer(SLOW_MS);
        });
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        tray(hwnd, NIM_DELETE);
        with_app(|a| a.cfg.save());
    }
    Ok(())
}

impl App {
    fn set_timer(&mut self, ms: u32) {
        if self.timer_ms != ms {
            self.timer_ms = ms;
            unsafe { SetTimer(Some(self.hwnd), TIMER_FRAME, ms, None) };
        }
    }

    pub fn state(&self) -> player::State {
        self.player.lock().unwrap().clone()
    }

    /// Display position in ms, including file and global offsets.
    pub fn lyric_pos(&self, st: &player::State) -> i64 {
        let Some(l) = &self.loaded else { return st.position_ms() + self.cfg.offset_ms };
        let pos = st.position_ms() + l.lyrics.offset_ms + self.cfg.offset_ms;
        if l.lyrics.synced {
            return pos;
        }
        // Unsynced text: spread lines evenly over the song, skipping a 7% intro and 5% outro.
        let d = st.duration_ms as f64;
        if d <= 0.0 {
            return 0;
        }
        let n = l.lyrics.lines.len() as f64;
        ((pos as f64 - 0.07 * d) / (0.88 * d) * n * 1000.0) as i64
    }

    fn frame(&mut self) {
        let st = self.state();
        let seek_ok = self.player.lock().unwrap().seek_ok.take();
        if let Some(ok) = seek_ok {
            if !ok && self.sync.is_some() {
                self.toast("Spotify refused to seek: restart the song yourself, then tap Space".into());
            }
        }
        if self.scrub.is_some_and(|(_, at)| at.elapsed().as_millis() >= SCRUB_SETTLE_MS) {
            self.commit_scrub();
        }
        if st.track != self.track {
            self.scrub = None;
            if self.sync.is_some() {
                self.end_sync();
                self.toast("Sync cancelled: the song changed".into());
            }
            self.track = st.track.clone();
            self.load_lyrics();
        }
        // Periodically pick up new/edited .lrc files.
        if self.last_check.elapsed().as_secs_f32() > 2.0 {
            self.last_check = Instant::now();
            match &self.loaded {
                _ if self.sync.is_some() => {}
                None if self.track.is_some() => self.load_lyrics(),
                Some(l) if mtime(&l.path) != l.mtime => self.load_lyrics(),
                _ => {}
            }
        }
        let animating = unsafe { render::render(self, &st) };
        let hovered = self.hovered();
        if !hovered {
            self.offset_mode = false;
        }
        self.update_click_through();
        self.set_timer(self.next_frame_ms(&st, animating || hovered));
    }

    /// Mouse wheel over the lyrics: step the highlight one line back (wheel up) or forward
    /// (wheel down). The seek itself waits until the wheel settles, see `commit_scrub`.
    fn scrub_wheel(&mut self, step: i32) {
        if self.sync.is_some() || self.cfg.locked || self.cfg.minimized {
            return;
        }
        let st = self.state();
        let pos = self.lyric_pos(&st);
        let Some(l) = self.loaded.as_ref().filter(|l| l.lyrics.synced && st.track.is_some()) else { return };
        let timed = l.lyrics.timed_count();
        if timed == 0 {
            return;
        }
        let i = match (self.scrub, l.lyrics.line_at(pos)) {
            (Some((i, _)), _) => i as i64 - step as i64,
            (None, Some(cur)) => cur as i64 - step as i64,
            (None, None) if step < 0 => 0, // before the first line: wheel down goes to it
            (None, None) => return,
        };
        self.scrub = Some((i.clamp(0, timed as i64 - 1) as usize, Instant::now()));
    }

    /// Seeks the player to the start of the previewed line (keeps paused songs paused).
    fn commit_scrub(&mut self) {
        let Some((i, _)) = self.scrub.take() else { return };
        let Some(l) = &self.loaded else { return };
        let Some(line) = l.lyrics.lines.get(i).filter(|ln| ln.time_ms != lrc::UNTIMED) else { return };
        // Display position = player position + file offset + global offset, so undo both.
        let ms = (line.time_ms - l.lyrics.offset_ms - self.cfg.offset_ms).max(0);
        let _ = self.cmd.send(player::Cmd::Seek(ms, false));
        // Show the new position right away instead of waiting for the next poll.
        let mut st = self.player.lock().unwrap();
        st.pos_ms = ms;
        st.anchor = Some(Instant::now());
    }

    /// Picks the redraw interval: full rate only while something moves, otherwise as slow as
    /// the content allows (software rendering cost scales directly with frame rate).
    fn next_frame_ms(&self, st: &player::State, moving: bool) -> u32 {
        if moving || self.sync.is_some() || self.scrub.is_some() {
            return FAST_MS;
        }
        if self.cfg.minimized {
            return SLOW_MS; // nothing to animate
        }
        let Some(l) = self.loaded.as_ref().filter(|_| st.playing) else { return SLOW_MS };
        if !l.lyrics.synced {
            return SLOW_MS; // evenly spread plain text: position is approximate anyway
        }
        if self.cfg.karaoke {
            return KARAOKE_MS;
        }
        // Static lines: sleep until the next line is due.
        let pos = self.lyric_pos(st);
        let next = l.lyrics.lines.iter().map(|ln| ln.time_ms).find(|&t| t > pos && t != lrc::UNTIMED);
        next.map_or(SLOW_MS, |t| (t - pos).clamp(FAST_MS as i64, SLOW_MS as i64) as u32)
    }

    pub fn hovered(&self) -> bool {
        let mut p = POINT::default();
        let mut r = RECT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    }

    fn load_lyrics(&mut self) {
        self.loaded = None;
        self.last_check = Instant::now();
        let dir = self.cfg.lyrics_path();
        let index = matcher::Index::build(&dir);
        self.index_len = index.len();
        if let Some(t) = &self.track {
            if let Some(path) = index.find(&t.artist, &t.title) {
                if let Some(lyrics) = lrc::load(path) {
                    self.loaded = Some(Loaded { lyrics, mtime: mtime(path), path: path.to_owned() });
                }
            }
        }
        log(&format!(
            "track={:?} dir={} files={} match={:?} lines={}",
            self.track.as_ref().map(|t| (&t.artist, &t.title)),
            dir.display(),
            self.index_len,
            self.loaded.as_ref().map(|l| l.path.display().to_string()),
            self.loaded.as_ref().map_or(0, |l| l.lyrics.lines.len()),
        ));
        self.r.reset_lines();
    }

    fn toast(&mut self, s: String) {
        self.toast = Some((s, Instant::now()));
    }

    fn adjust_offset(&mut self, delta: i64) {
        let Some(l) = &mut self.loaded else {
            self.toast("No lyrics loaded".into());
            return;
        };
        l.lyrics.offset_ms = if delta == 0 { 0 } else { l.lyrics.offset_ms + delta };
        let off = l.lyrics.offset_ms;
        let saved = lrc::save_offset(&l.path, off).is_ok();
        l.mtime = mtime(&l.path);
        self.toast(format!(
            "Timing offset {}{:.1}s{}",
            if off > 0 { "+" } else { "" },
            off as f32 / 1000.0,
            if saved { "" } else { " (could not save)" }
        ));
    }

    fn set_font_size(&mut self, size: f32) {
        self.cfg.font_size = size.clamp(8.0, 200.0);
        self.r.rebuild_formats(&self.cfg);
        self.cfg.save();
        self.toast(format!("Font size {}", self.cfg.font_size));
    }

    fn set_minimized(&mut self, minimized: bool) {
        self.cfg.minimized = minimized;
        self.offset_mode = false;
        self.r.reset_lines(); // snap straight to the current line on restore
        self.update_click_through();
        self.cfg.save();
        self.toast(if minimized { "Lyrics hidden. Click the tray icon to show them again" } else { "Lyrics shown" }.into());
    }

    fn set_locked(&mut self, locked: bool) {
        self.cfg.locked = locked;
        self.update_click_through();
        self.cfg.save();
        self.toast(if locked { "Locked: clicks pass through. Point at the lock icon to unlock" } else { "Unlocked: drag to move, edges to resize" }.into());
    }

    fn save_placement(&mut self) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        self.cfg.x = r.left;
        self.cfg.y = r.top;
        self.cfg.w = r.right - r.left;
        self.cfg.h = r.bottom - r.top;
        self.cfg.save();
    }

    fn start_sync(&mut self) {
        if !self.loaded.as_ref().is_some_and(|l| !l.lyrics.lines.is_empty()) {
            self.toast("No lyrics loaded to sync".into());
            return;
        }
        // Global hotkeys so tapping works while Spotify (or anything else) has focus.
        let ok = unsafe { RegisterHotKey(Some(self.hwnd), HK_SPACE, MOD_NOREPEAT, VK_SPACE.0 as u32) }.is_ok();
        if !ok {
            self.toast("Can't use Space: another app has it as a global hotkey".into());
            return;
        }
        unsafe {
            let _ = RegisterHotKey(Some(self.hwnd), HK_BACK, MOD_NOREPEAT, VK_BACK.0 as u32);
            let _ = RegisterHotKey(Some(self.hwnd), HK_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32);
            let _ = RegisterHotKey(Some(self.hwnd), HK_ENTER, MOD_NOREPEAT, VK_RETURN.0 as u32);
        }
        if self.cfg.minimized {
            self.cfg.minimized = false; // syncing needs the lyrics visible
            self.update_click_through();
        }
        self.offset_mode = false;
        // A partly synced file resumes at its first untimed line; otherwise start over.
        // Saved times are converted to player time (file offset applied), since saving drops [offset:].
        let l = &self.loaded.as_ref().unwrap().lyrics;
        let (timed, total) = (l.timed_count(), l.lines.len());
        let times: Vec<i64> = if timed > 0 && timed < total {
            l.lines[..timed].iter().map(|ln| ln.time_ms - l.offset_ms).collect()
        } else {
            Vec::new()
        };
        let msg = match times.last() {
            Some(&last) => {
                let _ = self.cmd.send(player::Cmd::Seek((last - 3000).max(0), true));
                format!("Resuming at line {}/{total}. Space when a line starts, Enter for a gap (solo)", timed + 1)
            }
            None => {
                let _ = self.cmd.send(player::Cmd::Seek(0, true));
                "Song restarted. Space when a line starts, Enter for a gap (solo)".to_owned()
            }
        };
        let history = vec![false; times.len()];
        self.sync = Some(SyncState { times, history, ..Default::default() });
        self.toast(msg);
    }

    fn end_sync(&mut self) {
        self.sync = None;
        unsafe {
            for id in [HK_SPACE, HK_BACK, HK_ESC, HK_ENTER] {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
        }
    }

    fn sync_tap(&mut self) {
        let pos = self.state().position_ms();
        let (Some(sync), Some(l)) = (&mut self.sync, &self.loaded) else { return };
        let total = l.lyrics.lines.len();
        if sync.times.len() >= total {
            self.save_sync(); // every line already stamped: Space finishes
            return;
        }
        sync.times.push(sync.last().map_or(pos, |last| pos.max(last)));
        sync.history.push(false);
        if sync.times.len() == total {
            self.toast("All lines stamped. Enter adds an ending gap, Space saves".into());
        }
    }

    /// Stamps an empty line now, so an instrumental break clears the previous line.
    fn sync_gap(&mut self) {
        let pos = self.state().position_ms();
        let (Some(sync), Some(l)) = (&mut self.sync, &self.loaded) else { return };
        // The next line is itself a gap from an earlier sync: stamp that instead of adding another.
        if l.lyrics.lines.get(sync.times.len()).is_some_and(|ln| ln.text.is_empty()) {
            return self.sync_tap();
        }
        let t = sync.last().map_or(pos, |last| pos.max(last));
        if sync.last() == Some(t) && sync.history.last() == Some(&true) {
            return; // double press
        }
        sync.gaps.push(t);
        sync.history.push(true);
        self.toast("Gap added".into());
    }

    /// Writes the tapped timings into the .lrc in place. When saving early, untapped lines stay
    /// as plain text after the timed ones, and the next Sync resumes from there.
    fn save_sync(&mut self) {
        let (Some(sync), Some(l)) = (&self.sync, &self.loaded) else { return };
        if sync.times.is_empty() {
            self.toast("Tap Space at least once before saving".into());
            return;
        }
        let total = l.lyrics.lines.len();
        let tapped = sync.times.len().min(total);
        let res = lrc::save_synced(&l.path, &sync.merged(&l.lyrics.lines));
        self.end_sync();
        self.load_lyrics();
        self.toast(match res {
            Ok(()) if tapped < total => format!("Saved {tapped}/{total} lines. Click Sync later to continue"),
            Ok(()) => format!("Saved timing for all {total} lines"),
            Err(e) => format!("Could not save: {e}"),
        });
    }

    fn sync_undo(&mut self) {
        let Some(sync) = &mut self.sync else { return };
        match sync.history.pop() {
            Some(true) => sync.gaps.pop(),
            Some(false) => sync.times.pop(),
            None => return,
        };
        // Rewind to just before the previous stamp so the undone one can be tapped again.
        let back = sync.last().map_or(0, |t| (t - 1500).max(0));
        let _ = self.cmd.send(player::Cmd::Seek(back, true));
    }

    fn click(&mut self, b: render::Btn) {
        use render::Btn;
        match b {
            Btn::Sync => self.start_sync(),
            Btn::SyncUndo => self.sync_undo(),
            Btn::SyncGap => self.sync_gap(),
            Btn::SyncSave => self.save_sync(),
            Btn::SyncCancel => {
                self.end_sync();
                self.toast("Sync cancelled, nothing saved".into());
            }
            Btn::Offset => self.offset_mode = !self.offset_mode,
            Btn::OffMinus => self.adjust_offset(-OFFSET_STEP),
            Btn::OffPlus => self.adjust_offset(OFFSET_STEP),
            Btn::OffReset => self.adjust_offset(0),
            Btn::Lock => self.set_locked(!self.cfg.locked),
            Btn::Minimize => self.set_minimized(true),
            Btn::Label => {}
        }
    }

    /// Minimized = fully click-through (restored from the tray). Locked = click-through except
    /// while the cursor is over the lock button, so it can be clicked.
    fn update_click_through(&mut self) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        let want = self.cfg.minimized || (self.cfg.locked && self.button_at(p.x, p.y).is_none());
        if want != self.click_through {
            self.click_through = want;
            unsafe {
                let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
                let t = WS_EX_TRANSPARENT.0 as isize;
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, if want { ex | t } else { ex & !t });
            }
        }
    }

    fn button_at(&self, sx: i32, sy: i32) -> Option<render::Btn> {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        let (x, y) = ((sx - r.left) as f32, (sy - r.top) as f32);
        // Minimized: nothing is interactive. Locked: only the lock button.
        let usable = |b: &render::Btn| match () {
            _ if self.cfg.minimized => false,
            _ if self.cfg.locked => *b == render::Btn::Lock,
            _ => true,
        };
        self.buttons.iter().find(|(b, rc)| usable(b) && rc.contains(x, y)).map(|(b, _)| *b)
    }
}

/// Appends to sp-lyrics-rs.log next to the exe, only if that file already exists (create it to enable logging).
fn log(msg: &str) {
    use std::io::Write;
    let p = config::exe_dir().join("sp-lyrics-rs.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(p) {
        let _ = writeln!(f, "{msg}");
    }
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

unsafe fn tray(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) {
    let icon = with_app(|a| a.icon).unwrap_or_default();
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    let tip: Vec<u16> = "sp-lyrics-rs (click: lock/unlock, right-click: menu)".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    let _ = unsafe { Shell_NotifyIconW(op, &nid) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn show_menu(hwnd: HWND) {
    let Some((locked, karaoke, has_lrc, offset, syncing, minimized)) = with_app(|a| {
        (a.cfg.locked, a.cfg.karaoke, a.loaded.is_some(), a.loaded.as_ref().map_or(0, |l| l.lyrics.offset_ms), a.sync.is_some(), a.cfg.minimized)
    }) else {
        return;
    };
    unsafe {
        let m = CreatePopupMenu().unwrap();
        let add = |id: u32, text: &str, checked: bool, enabled: bool| {
            let mut f = MF_STRING;
            if checked {
                f |= MF_CHECKED;
            }
            if !enabled {
                f |= MF_GRAYED;
            }
            let t = wide(text);
            let _ = AppendMenuW(m, f, id as usize, PCWSTR(t.as_ptr()));
        };
        let sep = || {
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        };
        add(ID_LOCK, "Lock (click-through)", locked, true);
        add(ID_MINIMIZE, "Hide lyrics (minimize)", minimized, !syncing);
        add(ID_KARAOKE, "Karaoke fill", karaoke, true);
        add(ID_AUTOSTART, "Start with Windows", autostart::is_enabled(), true);
        sep();
        add(ID_FONT, "Font...", false, true);
        add(ID_BIGGER, "Bigger text\tCtrl+Wheel", false, true);
        add(ID_SMALLER, "Smaller text\tCtrl+Wheel", false, true);
        sep();
        add(ID_SYNC, if syncing { "Cancel sync" } else { "Sync timing by tapping Space..." }, false, has_lrc);
        if syncing {
            add(ID_SYNC_SAVE, "Save sync now", false, true);
        }
        add(ID_EARLIER, "Lyrics earlier (+0.1s)", false, has_lrc && !syncing);
        add(ID_LATER, "Lyrics later (-0.1s)", false, has_lrc && !syncing);
        add(ID_RESET_OFFSET, &format!("Reset timing ({:+.1}s)", offset as f32 / 1000.0), false, has_lrc && offset != 0);
        sep();
        add(ID_OPEN_LRC, "Edit current .lrc", false, has_lrc);
        add(ID_OPEN_DIR, "Open lyrics folder", false, true);
        add(ID_CHANGE_DIR, "Change lyrics folder...", false, true);
        add(ID_RELOAD, "Reload", false, true);
        sep();
        add(ID_EXIT, "Exit", false, true);

        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, p.x, p.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        command(hwnd, cmd.0 as u32);
    }
}

unsafe fn command(hwnd: HWND, id: u32) {
    match id {
        ID_LOCK => {
            with_app(|a| a.set_locked(!a.cfg.locked));
        }
        ID_KARAOKE => {
            with_app(|a| {
                a.cfg.karaoke = !a.cfg.karaoke;
                a.cfg.save();
            });
        }
        ID_BIGGER => {
            with_app(|a| a.set_font_size(a.cfg.font_size + 2.0));
        }
        ID_SMALLER => {
            with_app(|a| a.set_font_size(a.cfg.font_size - 2.0));
        }
        ID_EARLIER => {
            with_app(|a| a.adjust_offset(OFFSET_STEP));
        }
        ID_LATER => {
            with_app(|a| a.adjust_offset(-OFFSET_STEP));
        }
        ID_RESET_OFFSET => {
            with_app(|a| a.adjust_offset(0));
        }
        ID_RELOAD => {
            with_app(|a| {
                a.load_lyrics();
                let msg = match (&a.loaded, a.index_len) {
                    (Some(_), n) => format!("Reloaded ({n} .lrc files in folder)"),
                    (None, n) => format!("No match ({n} .lrc files in folder)"),
                };
                a.toast(msg);
            });
        }
        ID_OPEN_DIR => {
            if let Some(dir) = with_app(|a| a.cfg.lyrics_path()) {
                let _ = std::fs::create_dir_all(&dir);
                open(hwnd, &dir.to_string_lossy(), None);
            }
        }
        ID_OPEN_LRC => {
            if let Some(Some(p)) = with_app(|a| a.loaded.as_ref().map(|l| l.path.clone())) {
                open(hwnd, "notepad.exe", Some(&format!("\"{}\"", p.display())));
            }
        }
        ID_CHANGE_DIR => {
            if let Some(dir) = unsafe { pick_folder(hwnd) } {
                with_app(|a| {
                    a.cfg.lyrics_dir = dir;
                    a.cfg.save();
                    a.load_lyrics();
                    a.toast(format!("{} .lrc files found", a.index_len));
                });
            }
        }
        ID_FONT => unsafe { choose_font(hwnd) },
        ID_SYNC => {
            with_app(|a| if a.sync.is_some() { a.click(render::Btn::SyncCancel) } else { a.start_sync() });
        }
        ID_MINIMIZE => {
            with_app(|a| a.set_minimized(!a.cfg.minimized));
        }
        ID_AUTOSTART => {
            let on = !autostart::is_enabled();
            let ok = autostart::set(on);
            with_app(|a| {
                a.toast(match (ok, on) {
                    (true, true) => "sp-lyrics-rs will start when you sign in to Windows".into(),
                    (true, false) => "Removed from Windows startup".into(),
                    (false, _) => "Could not change the Windows startup setting".into(),
                })
            });
        }
        ID_SYNC_SAVE => {
            with_app(|a| a.save_sync());
        }
        ID_EXIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
    with_app(|a| a.frame());
}

fn open(hwnd: HWND, file: &str, args: Option<&str>) {
    let f = wide(file);
    let a = args.map(wide);
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            PCWSTR(f.as_ptr()),
            a.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr())),
            None,
            SW_SHOWNORMAL,
        );
    }
}

unsafe fn pick_folder(hwnd: HWND) -> Option<String> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dlg.SetOptions(dlg.GetOptions().ok()? | FOS_PICKFOLDERS).ok()?;
        dlg.SetTitle(w!("Choose the folder containing your .lrc files")).ok()?;
        dlg.Show(Some(hwnd)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

unsafe fn choose_font(hwnd: HWND) {
    let Some((face, size, bold)) = with_app(|a| (a.cfg.font.clone(), a.cfg.font_size, a.cfg.bold)) else { return };
    let mut lf = LOGFONTW {
        lfHeight: -(size.round() as i32),
        lfWeight: if bold { 700 } else { 400 },
        ..Default::default()
    };
    let f: Vec<u16> = face.encode_utf16().take(31).collect();
    lf.lfFaceName[..f.len()].copy_from_slice(&f);
    let mut cf = CHOOSEFONTW {
        lStructSize: size_of::<CHOOSEFONTW>() as u32,
        hwndOwner: hwnd,
        lpLogFont: &mut lf,
        Flags: CF_SCREENFONTS | CF_INITTOLOGFONTSTRUCT | CF_NOVERTFONTS | CF_FORCEFONTEXIST,
        ..Default::default()
    };
    if unsafe { ChooseFontW(&mut cf) }.as_bool() {
        let len = lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(32);
        let face = String::from_utf16_lossy(&lf.lfFaceName[..len]);
        let px = cf.iPointSize as f32 / 10.0 * 96.0 / 72.0;
        with_app(|a| {
            a.cfg.font = face;
            a.cfg.bold = lf.lfWeight >= 600;
            a.set_font_size(px.round());
        });
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
            WM_NCHITTEST => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                match with_app(|a| (a.button_at(x, y), a.cfg.locked || a.cfg.minimized)) {
                    Some((Some(_), _)) => return LRESULT(HTCLIENT as isize),
                    Some((None, true)) => return LRESULT(HTNOWHERE as isize), // locked: nothing else is interactive
                    _ => {}
                }
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                let b = 8;
                let (l, rt, t, bt) = (x < r.left + b, x >= r.right - b, y < r.top + b, y >= r.bottom - b);
                let ht = match (l, rt, t, bt) {
                    (true, _, true, _) => HTTOPLEFT,
                    (_, true, true, _) => HTTOPRIGHT,
                    (true, _, _, true) => HTBOTTOMLEFT,
                    (_, true, _, true) => HTBOTTOMRIGHT,
                    (true, ..) => HTLEFT,
                    (_, true, ..) => HTRIGHT,
                    (_, _, true, _) => HTTOP,
                    (.., true) => HTBOTTOM,
                    _ => HTCAPTION,
                };
                return LRESULT(ht as isize);
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize = POINT { x: 200, y: 60 };
                return LRESULT(0);
            }
            WM_NCLBUTTONDBLCLK => return LRESULT(0),
            WM_SIZE | WM_MOVE => {
                with_app(|a| a.frame());
                return LRESULT(0);
            }
            WM_EXITSIZEMOVE => {
                with_app(|a| a.save_placement());
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                with_app(|a| {
                    a.r.set_scale((wp.0 & 0xFFFF) as f32 / 96.0, &a.cfg);
                    a.save_placement();
                });
                return LRESULT(0);
            }
            WM_TIMER => {
                with_app(|a| a.frame());
                return LRESULT(0);
            }
            WM_TRACK => {
                with_app(|a| a.frame());
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
                let keys = (wp.0 & 0xFFFF) as u32;
                let step = if delta > 0 { 1 } else { -1 };
                // Ctrl: font size. Offset mode: adjust offset. Otherwise: seek line by line.
                with_app(|a| {
                    if keys & 0x0008 != 0 || GetKeyState(VK_CONTROL.0 as i32) < 0 {
                        a.set_font_size(a.cfg.font_size + 2.0 * step as f32);
                    } else if a.offset_mode {
                        a.adjust_offset(OFFSET_STEP * step as i64);
                    } else {
                        a.scrub_wheel(step);
                    }
                    a.frame();
                });
                return LRESULT(0);
            }
            WM_HOTKEY => {
                with_app(|a| {
                    match wp.0 as i32 {
                        HK_SPACE => a.sync_tap(),
                        HK_BACK => a.sync_undo(),
                        HK_ESC => a.click(render::Btn::SyncCancel),
                        HK_ENTER => a.sync_gap(),
                        _ => {}
                    }
                    a.frame();
                });
                return LRESULT(0);
            }
            // Toolbar buttons are client area; everything else is caption (drag) / borders.
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
                if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(c));
                }
                return LRESULT(1);
            }
            WM_LBUTTONUP => {
                let mut p = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                let _ = ClientToScreen(hwnd, &mut p);
                with_app(|a| {
                    if let Some(b) = a.button_at(p.x, p.y) {
                        a.click(b);
                    }
                    a.frame();
                });
                return LRESULT(0);
            }
            WM_CONTEXTMENU => {
                show_menu(hwnd);
                return LRESULT(0);
            }
            WM_TRAY => {
                match (lp.0 & 0xFFFF) as u32 {
                    WM_LBUTTONUP => {
                        with_app(|a| {
                            // Show/hide the lyrics; hiding is off while syncing (same as the menu item).
                            if a.cfg.minimized || a.sync.is_none() {
                                a.set_minimized(!a.cfg.minimized);
                                a.frame();
                            }
                        });
                    }
                    WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {
                if with_app(|a| a.taskbar_created) == Some(msg) && msg != 0 {
                    tray(hwnd, NIM_ADD);
                }
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// Builds a 32x32 tray/window icon (white music note on a cyan disc) at runtime.
fn make_icon() -> HICON {
    const N: i32 = 32;
    let mut px = vec![0u32; (N * N) as usize];
    let inside = |x: f32, y: f32| -> (bool, bool) {
        let disc = (x - 16.0).powi(2) + (y - 16.0).powi(2) <= 15.5f32.powi(2);
        let head = ((x - 13.0) / 4.8).powi(2) + ((y - 21.5) / 3.6).powi(2) <= 1.0;
        let stem = (16.3..18.6).contains(&x) && (7.0..21.5).contains(&y);
        let flag = (16.3..24.0).contains(&x) && (7.0..10.5).contains(&y);
        (disc, head || stem || flag)
    };
    for y in 0..N {
        for x in 0..N {
            let (mut d, mut n) = (0.0f32, 0.0f32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (a, b) = inside(x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                    d += a as u8 as f32 / 16.0;
                    n += (a && b) as u8 as f32 / 16.0;
                }
            }
            // Blend white note over cyan (0x4FC3F7), premultiplied by disc coverage.
            let lerp = |c: f32| (c + (255.0 - c) * (n / d.max(1e-6))) * d;
            let (r, g, b) = (lerp(79.0), lerp(195.0), lerp(247.0));
            px[(y * N + x) as usize] = ((d * 255.0) as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
        }
    }
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: N,
                biHeight: -N,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else { return HICON::default() };
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(N, N, 1, 1, None);
        let ii = ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&ii).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_gaps_merge_in_time_order() {
        let lines: Vec<lrc::Line> = ["a", "b", "c"].iter().map(|t| lrc::Line { time_ms: 0, text: (*t).into() }).collect();
        let s = SyncState { times: vec![1000, 9000], gaps: vec![5000, 9000], history: vec![false, true, false, true] };
        let m = s.merged(&lines);
        assert_eq!(m, [(Some(1000), "a"), (Some(5000), ""), (Some(9000), ""), (Some(9000), "b"), (None, "c")]);
        let text = lrc::synced_text("", &m, "\n");
        assert_eq!(text, "[00:01.00]a\n[00:05.00]\n[00:09.00]\n[00:09.00]b\nc\n");
        // A saved gap parses back as a timed empty line, so resuming keeps it.
        let l = lrc::parse(&text);
        assert_eq!((l.timed_count(), l.lines[1].text.as_str()), (4, ""));
        assert_eq!(l.line_at(6000), Some(1));
    }
}
