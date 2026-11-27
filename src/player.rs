//! Reads the currently playing track from Windows' media session API
//! (GlobalSystemMediaTransportControls), which Spotify desktop publishes to.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

#[derive(Clone, Default, PartialEq, Debug)]
pub struct Track {
    pub artist: String,
    pub title: String,
}

#[derive(Clone, Default)]
pub struct State {
    /// None = no matching player session.
    pub track: Option<Track>,
    pub playing: bool,
    /// Position at `anchor` in ms.
    pub pos_ms: i64,
    pub anchor: Option<Instant>,
    pub duration_ms: i64,
    /// Result of the last `Cmd::Seek`, taken by the UI.
    pub seek_ok: Option<bool>,
}

impl State {
    pub fn position_ms(&self) -> i64 {
        match self.anchor {
            Some(a) if self.playing => self.pos_ms + a.elapsed().as_millis() as i64,
            _ => self.pos_ms,
        }
    }
}

pub type Shared = Arc<Mutex<State>>;

pub enum Cmd {
    /// Seek to this position (ms). `true` also starts playback (sync mode needs the song running).
    Seek(i64, bool),
}

/// Spawns the polling thread. `on_track_change` is called (from that thread) whenever
/// the track changes or the player appears/disappears.
pub fn spawn(player_filter: String, on_track_change: impl Fn() + Send + 'static) -> (Shared, Sender<Cmd>) {
    let shared: Shared = Arc::default();
    let s = shared.clone();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok() };
        let filter = player_filter.to_lowercase();
        let mut mgr: Option<Manager> = None;
        loop {
            if mgr.is_none() {
                mgr = Manager::RequestAsync().and_then(|op| op.join()).ok();
            }
            let new = mgr.as_ref().and_then(|m| poll(m, &filter));
            let changed = {
                let mut st = s.lock().unwrap();
                let changed = new.as_ref().map(|n| &n.0) != st.track.as_ref();
                match new {
                    None => {
                        st.track = None;
                        st.playing = false;
                    }
                    Some((track, playing, pos, duration)) => {
                        st.duration_ms = duration;
                        // Only re-anchor when the reported position drifts noticeably;
                        // keeps the local clock smooth between coarse API updates.
                        let drift = (st.position_ms() - pos).abs();
                        if changed || playing != st.playing || drift > 400 || st.anchor.is_none() {
                            st.pos_ms = pos;
                            st.anchor = Some(Instant::now());
                        }
                        st.track = Some(track);
                        st.playing = playing;
                    }
                }
                changed
            };
            if changed {
                on_track_change();
            }
            if let Ok(Cmd::Seek(ms, play)) = rx.recv_timeout(Duration::from_millis(250)) {
                let ok = mgr.as_ref().and_then(|m| find_session(m, &filter)).is_some_and(|sess| seek(&sess, ms, play));
                s.lock().unwrap().seek_ok = Some(ok);
            }
        }
    });
    (shared, tx)
}

fn seek(sess: &Session, ms: i64, play: bool) -> bool {
    let ok = sess
        .TryChangePlaybackPositionAsync(ms * 10_000)
        .and_then(|op| op.join())
        .unwrap_or(false);
    if ok && play {
        let _ = sess.TryPlayAsync().and_then(|op| op.join());
    }
    ok
}

fn poll(mgr: &Manager, filter: &str) -> Option<(Track, bool, i64, i64)> {
    let session = find_session(mgr, filter)?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let track = Track {
        artist: props.Artist().ok()?.to_string_lossy(),
        title: props.Title().ok()?.to_string_lossy(),
    };
    if track.title.is_empty() {
        return None; // e.g. Spotify open but nothing loaded / ad break
    }
    let playing = session
        .GetPlaybackInfo()
        .and_then(|i| i.PlaybackStatus())
        .is_ok_and(|s| s == Status::Playing);
    let tl = session.GetTimelineProperties().ok()?;
    let mut pos = tl.Position().ok()?.Duration / 10_000; // 100ns -> ms
    let start = tl.StartTime().map_or(0, |t| t.Duration / 10_000);
    let duration = tl.EndTime().map_or(0, |t| t.Duration / 10_000) - start;
    if playing {
        // Position is a snapshot taken at LastUpdatedTime; extrapolate to now.
        if let Ok(updated) = tl.LastUpdatedTime() {
            let now = unsafe { GetSystemTimePreciseAsFileTime() };
            let now = ((now.dwHighDateTime as i64) << 32) | now.dwLowDateTime as i64;
            let since = (now - updated.UniversalTime) / 10_000;
            if updated.UniversalTime > 0 && (0..6 * 3600 * 1000).contains(&since) {
                pos += since;
            }
        }
        if duration > 0 {
            pos = pos.min(start + duration);
        }
    }
    Some((track, playing, pos, duration))
}

fn find_session(mgr: &Manager, filter: &str) -> Option<Session> {
    let matches = |s: &Session| {
        filter.is_empty()
            || s.SourceAppUserModelId().is_ok_and(|id| id.to_string_lossy().to_lowercase().contains(filter))
    };
    if let Ok(cur) = mgr.GetCurrentSession() {
        if matches(&cur) {
            return Some(cur);
        }
    }
    let sessions = mgr.GetSessions().ok()?;
    (&sessions).into_iter().find(|s| matches(s))
}
