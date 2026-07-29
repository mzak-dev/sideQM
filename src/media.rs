//! Now Playing state (CONTEXT.md), off the event loop.
//!
//! Polls `GlobalSystemMediaTransportControlsSessionManager` on a dedicated
//! thread rather than subscribing to its change events: a handful of
//! synchronous calls twice a second is far less code than wiring up WinRT
//! event tokens, and the Menu is never open long enough for the extra latency
//! to be visible. The Manager and its Sessions are WinRT-agile, so one
//! `COINIT_MULTITHREADED` thread can hold the Manager for the app's whole
//! lifetime and call straight through, blocking on `wait` below.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{Interface, RuntimeType};
use windows_future::{AsyncStatus, IAsyncInfo, IAsyncOperation};
use winit::event_loop::EventLoopProxy;

use crate::AppEvent;
use crate::icons::{self, RgbaIcon};
use crate::log;

/// How often to re-check the current session when nothing is pushing us an
/// update. Half a second is invisible next to how briefly the Menu is open.
const POLL: Duration = Duration::from_millis(600);
/// Refuse an absurd thumbnail before allocating for it, same spirit as
/// `icons::MAX_SRC_DIM`.
const MAX_ART_BYTES: u64 = 16 * 1024 * 1024;

/// One snapshot of "what's playing". Absence of a `NowPlaying` (the `Option`
/// this is always wrapped in) means no session is Playing or Paused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// Identifies the track for change detection and for matching an
    /// in-flight art fetch back to the track it was requested for — a hash of
    /// title+artist rather than anything the session exposes directly.
    pub track_key: u64,
}

#[derive(Debug)]
pub enum MediaEvent {
    /// The current session's state changed (including appearing/disappearing).
    State(Option<NowPlaying>),
    /// Album art finished decoding for `track_key`. `None` means there is
    /// none, or it could not be decoded — the Hub falls back to a plain
    /// background either way.
    Art {
        track_key: u64,
        icon: Option<Arc<RgbaIcon>>,
    },
}

/// A Transport button's action.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    Prev,
    PlayPause,
    Next,
}

pub struct MediaService {
    tx: Sender<Command>,
}

impl MediaService {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> MediaService {
        let (tx, rx) = mpsc::channel::<Command>();
        std::thread::Builder::new()
            .name("sideqm-media".into())
            .spawn(move || worker(rx, proxy))
            .expect("media worker thread");
        MediaService { tx }
    }

    /// Fire a Transport button. Silently dropped if the worker thread died —
    /// same "never breaks over it" spirit as a missing icon.
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

fn worker(rx: mpsc::Receiver<Command>, proxy: EventLoopProxy<AppEvent>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let manager = match wait(SessionManager::RequestAsync()) {
        Ok(m) => m,
        Err(e) => {
            log!("Now Playing unavailable: could not request the session manager: {e}");
            return;
        }
    };

    // (track_key, playing) of the last snapshot actually sent, so a poll that
    // finds nothing new stays quiet.
    let mut last: Option<(u64, bool)> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(cmd) => run_command(&manager, cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let snap = snapshot(&manager);
        let fingerprint = snap.as_ref().map(|s| (s.track_key, s.playing));
        if fingerprint == last {
            continue;
        }
        let is_new_track = snap.as_ref().map(|s| s.track_key) != last.map(|(k, _)| k);
        last = fingerprint;
        let track_key = snap.as_ref().map(|s| s.track_key);
        // State first: on_media_event's track_key guard for the Art event
        // below only matches once this has been applied, and decoding art is
        // the slower of the two, so sending it second costs nothing anyway.
        if proxy
            .send_event(AppEvent::Media(MediaEvent::State(snap)))
            .is_err()
        {
            return; // event loop is gone; so are we
        }
        if is_new_track && let Some(track_key) = track_key {
            fetch_art(&manager, track_key, &proxy);
        }
    }
}

/// `None` unless the current session is actually Playing or Paused — Opened,
/// Changing, Stopped and Closed all read as "nothing to show" here.
fn snapshot(manager: &SessionManager) -> Option<NowPlaying> {
    let session = manager.GetCurrentSession().ok()?;
    let status = session.GetPlaybackInfo().ok()?.PlaybackStatus().ok()?;
    let playing = match status {
        PlaybackStatus::Playing => true,
        PlaybackStatus::Paused => false,
        _ => return None,
    };
    let props = wait(session.TryGetMediaPropertiesAsync()).ok()?;
    let title = props.Title().map(|h| h.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
    let track_key = track_key(&title, &artist);
    Some(NowPlaying {
        title,
        artist,
        playing,
        track_key,
    })
}

/// `windows-rs` projects WinRT async operations as `IntoFuture`, with no
/// blocking accessor — there is no async runtime in this app to `.await` on,
/// and pulling one in for a handful of calls on an already-dedicated thread
/// would be a much bigger dependency than this spin-wait.
fn wait<T: RuntimeType + 'static>(op: windows::core::Result<IAsyncOperation<T>>) -> windows::core::Result<T> {
    let op = op?;
    let info: IAsyncInfo = op.cast()?;
    while info.Status()? == AsyncStatus::Started {
        std::thread::sleep(Duration::from_millis(5));
    }
    op.GetResults()
}

fn track_key(title: &str, artist: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    artist.hash(&mut h);
    h.finish()
}

fn run_command(manager: &SessionManager, cmd: Command) {
    let Ok(session) = manager.GetCurrentSession() else {
        return;
    };
    let op = match cmd {
        Command::Prev => session.TrySkipPreviousAsync(),
        Command::PlayPause => session.TryTogglePlayPauseAsync(),
        Command::Next => session.TrySkipNextAsync(),
    };
    // Fire-and-forget: a rejected command just leaves things where they were,
    // and the next poll reports whatever actually happened.
    let _ = wait(op);
}

fn fetch_art(manager: &SessionManager, track_key: u64, proxy: &EventLoopProxy<AppEvent>) {
    let icon = decode_art(manager).map(Arc::new);
    let _ = proxy.send_event(AppEvent::Media(MediaEvent::Art { track_key, icon }));
}

/// Every step logs its own failure — SMTC thumbnail support varies a lot
/// between players, and a silent `None` here left no way to tell "this app
/// doesn't publish art" from "the code is broken".
fn decode_art(manager: &SessionManager) -> Option<RgbaIcon> {
    let session = manager
        .GetCurrentSession()
        .map_err(|e| log!("Now Playing art: no current session: {e}"))
        .ok()?;
    let props = wait(session.TryGetMediaPropertiesAsync())
        .map_err(|e| log!("Now Playing art: could not read media properties: {e}"))
        .ok()?;
    let thumb = props
        .Thumbnail()
        .map_err(|e| log!("Now Playing art: session has no thumbnail: {e}"))
        .ok()?;
    let stream = wait(thumb.OpenReadAsync())
        .map_err(|e| log!("Now Playing art: could not open the thumbnail stream: {e}"))
        .ok()?;
    let size = stream
        .Size()
        .map_err(|e| log!("Now Playing art: could not read the thumbnail's size: {e}"))
        .ok()?;
    if size == 0 || size > MAX_ART_BYTES {
        log!("Now Playing art: thumbnail size {size} bytes out of range, skipping");
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream)
        .map_err(|e| log!("Now Playing art: could not create a reader for the thumbnail: {e}"))
        .ok()?;
    wait(reader.LoadAsync(size as u32))
        .map_err(|e| log!("Now Playing art: could not load {size} thumbnail bytes: {e}"))
        .ok()?;
    let mut buf = vec![0u8; size as usize];
    if let Err(e) = reader.ReadBytes(&mut buf) {
        log!("Now Playing art: could not read thumbnail bytes out of the reader: {e}");
        return None;
    }
    match image::load_from_memory(&buf) {
        Ok(img) => Some(icons::normalize(img.to_rgba8())),
        Err(e) => {
            log!("Now Playing art: could not decode the {size}-byte thumbnail: {e}");
            None
        }
    }
}
