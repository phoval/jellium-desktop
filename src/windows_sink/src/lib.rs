//! Windows SystemMediaTransportControls (SMTC) sink. The shared
//! [`jfn_playback::sink_core`] harness owns the event queue and consumer
//! thread (MTA-initialised); this crate supplies a [`WindowsSink`] whose
//! `deliver` drives SMTC. ButtonPressed / PlaybackPositionChangeRequested
//! callbacks dispatch via [`sink_core::execute`] / [`sink_core::seek_to_ms`].
//!
//! Public entry points:
//!   * `jfn_windows_sink_start_for(hwnd)` — start the sink (SMTC binds to the
//!     mpv window via GetForWindow).
//!   * `jfn_windows_sink_stop()` — signal the thread to exit at next wake.

#![cfg(target_os = "windows")]

use std::time::Instant;

use jfn_playback::sink_core::{
    self, MediaCommand, Phase, PositionThrottle, QueuedSink, map_kind_to_phase,
};
use jfn_playback::{
    MediaMetadata, MediaType as PbMediaType, PlaybackEvent, PlaybackEventKind, PlaybackSnapshot,
};
use windows::Foundation::TimeSpan;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::{
    DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Security::Cryptography::{CRYPT_STRING_BASE64, CryptStringToBinaryA};
use windows::Win32::System::WinRT::{ISystemMediaTransportControlsInterop, RoGetActivationFactory};
use windows::core::HSTRING;

/// Start the sink. `hwnd_raw` is the HWND of the mpv window — required
/// to bind SMTC via ISystemMediaTransportControlsInterop::GetForWindow.
pub fn jfn_windows_sink_start_for(hwnd_raw: isize) {
    sink_core::run_sink("windows-sink", move || WindowsSink::new(hwnd_raw));
}

pub fn jfn_windows_sink_stop() {
    sink_core::stop();
}

#[derive(Default)]
struct WinState {
    metadata: MediaMetadata,
    phase: Option<Phase>,
    snapshot: PlaybackSnapshot,
    throttle: PositionThrottle,
}

struct WindowsSink {
    hwnd_raw: isize,
    smtc: Option<Smtc>,
    state: WinState,
}

impl WindowsSink {
    fn new(hwnd_raw: isize) -> Self {
        Self {
            hwnd_raw,
            smtc: None,
            state: WinState::default(),
        }
    }
}

impl QueuedSink for WindowsSink {
    fn init(&mut self) {
        self.smtc = init_smtc(self.hwnd_raw);
    }

    fn deliver(&mut self, ev: &PlaybackEvent) {
        deliver(&mut self.state, &mut self.smtc, ev);
    }

    fn teardown(&mut self) {
        if let Some(s) = self.smtc.take() {
            teardown_smtc(s);
        }
    }
}

struct Smtc {
    smtc: SystemMediaTransportControls,
    updater: windows::Media::SystemMediaTransportControlsDisplayUpdater,
    button_token: i64,
    seek_token: i64,
    cached_thumbnail: Option<RandomAccessStreamReference>,
}

fn init_smtc(hwnd_raw: isize) -> Option<Smtc> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    if hwnd_raw == 0 {
        tracing::error!(target: "Media", "[SMTC] NULL HWND provided");
        return None;
    }
    let smtc: SystemMediaTransportControls = match unsafe {
        let interop: ISystemMediaTransportControlsInterop =
            RoGetActivationFactory(&HSTRING::from("Windows.Media.SystemMediaTransportControls"))
                .ok()?;
        let hwnd = HWND(hwnd_raw as *mut _);
        interop
            .GetForWindow::<SystemMediaTransportControls>(hwnd)
            .ok()
    } {
        Some(s) => s,
        None => {
            tracing::error!(target: "Media", "[SMTC] GetForWindow returned null");
            return None;
        }
    };

    smtc.SetIsEnabled(true).ok()?;
    smtc.SetIsPlayEnabled(true).ok()?;
    smtc.SetIsPauseEnabled(true).ok()?;
    smtc.SetIsStopEnabled(true).ok()?;
    smtc.SetIsNextEnabled(false).ok()?;
    smtc.SetIsPreviousEnabled(false).ok()?;

    let updater = smtc.DisplayUpdater().ok()?;

    let button_token = {
        let handler = windows::Foundation::TypedEventHandler::new(
            |_sender: windows_core::Ref<SystemMediaTransportControls>,
             args: windows_core::Ref<
                SystemMediaTransportControlsButtonPressedEventArgs,
            >| {
                if let Some(args) = args.as_ref()
                    && let Ok(button) = args.Button()
                {
                    on_button_pressed(button);
                }
                Ok(())
            },
        );
        smtc.ButtonPressed(&handler).ok()?
    };

    let seek_token = {
        let handler =
            windows::Foundation::TypedEventHandler::new(
                |_sender: windows_core::Ref<SystemMediaTransportControls>,
                 args: windows_core::Ref<
                    windows::Media::PlaybackPositionChangeRequestedEventArgs,
                >| {
                    if let Some(args) = args.as_ref()
                        && let Ok(span) = args.RequestedPlaybackPosition()
                    {
                        let pos_us = span.Duration / 10;
                        sink_core::seek_to_ms(pos_us / 1000);
                    }
                    Ok(())
                },
            );
        smtc.PlaybackPositionChangeRequested(&handler).ok()?
    };

    tracing::info!(target: "Media", "[SMTC] Initialized");
    Some(Smtc {
        smtc,
        updater,
        button_token,
        seek_token,
        cached_thumbnail: None,
    })
}

fn teardown_smtc(s: Smtc) {
    let _ = s.smtc.RemoveButtonPressed(s.button_token);
    let _ = s.smtc.RemovePlaybackPositionChangeRequested(s.seek_token);
    let _ = s.updater.ClearAll();
    let _ = s.updater.Update();
    let _ = s.smtc.SetIsEnabled(false);
}

fn on_button_pressed(button: SystemMediaTransportControlsButton) {
    use SystemMediaTransportControlsButton as B;
    let cmd = match button {
        B::Play => MediaCommand::Play,
        B::Pause => MediaCommand::Pause,
        B::Stop => MediaCommand::Stop,
        B::Next => MediaCommand::Next,
        B::Previous => MediaCommand::Previous,
        _ => return,
    };
    sink_core::execute(cmd);
}

fn deliver(state: &mut WinState, smtc: &mut Option<Smtc>, ev: &PlaybackEvent) {
    // Every event carries the coordinator's observed mpv state. In particular,
    // pause/rate/duration changes must not reuse the last throttled position.
    state.snapshot.clone_from(&ev.snapshot);
    match ev.kind {
        PlaybackEventKind::MetadataChanged => {
            if !ev.metadata.id.is_empty() && ev.metadata.id == state.metadata.id {
                return;
            }
            state.metadata = ev.metadata.clone();
        }
        PlaybackEventKind::ArtworkChanged => {
            let Some(smtc) = smtc.as_mut() else { return };
            if ev.artwork_uri.is_empty() {
                return;
            }
            let Some(comma) = ev.artwork_uri.find(',') else {
                return;
            };
            let b64 = &ev.artwork_uri[comma + 1..];
            let bytes = match decode_base64(b64) {
                Some(b) => b,
                None => return,
            };
            if let Some(ref_stream) = make_thumbnail_stream(&bytes) {
                let _ = smtc.updater.SetThumbnail(&ref_stream);
                let _ = smtc.updater.Update();
                smtc.cached_thumbnail = Some(ref_stream);
            }
        }
        PlaybackEventKind::QueueCapsChanged => {
            if let Some(s) = smtc.as_ref() {
                let _ = s.smtc.SetIsNextEnabled(ev.can_go_next);
                let _ = s.smtc.SetIsPreviousEnabled(ev.can_go_prev);
            }
        }
        PlaybackEventKind::Started
        | PlaybackEventKind::Paused
        | PlaybackEventKind::TrackLoaded
        | PlaybackEventKind::Finished
        | PlaybackEventKind::Canceled
        | PlaybackEventKind::Error => {
            let p = map_kind_to_phase(ev.kind);
            state.phase = Some(p);
            let Some(s) = smtc.as_mut() else { return };
            match p {
                Phase::Playing => {
                    let _ = s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing);
                    update_display_properties(state, s);
                }
                Phase::Paused => {
                    let _ = s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Paused);
                    update_timeline(state, s);
                }
                Phase::Stopped => {
                    state.metadata = MediaMetadata::default();
                    state.snapshot = PlaybackSnapshot::default();
                    s.cached_thumbnail = None;
                    let _ = s.updater.ClearAll();
                    let _ = s.updater.Update();
                    let _ = s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Stopped);
                    return;
                }
            }
            state.throttle.force_next();
        }
        PlaybackEventKind::PositionChanged => {
            if state.throttle.due(Instant::now(), false)
                && let Some(s) = smtc.as_mut()
            {
                update_timeline(state, s);
            }
        }
        PlaybackEventKind::Seeked => {
            if state.throttle.due(Instant::now(), true)
                && let Some(s) = smtc.as_mut()
            {
                update_timeline(state, s);
            }
        }
        PlaybackEventKind::RateChanged | PlaybackEventKind::DurationChanged => {
            if let Some(s) = smtc.as_ref() {
                update_timeline(state, s);
            }
        }
        _ => {}
    }
}

fn update_display_properties(state: &mut WinState, s: &mut Smtc) {
    if state.phase == Some(Phase::Stopped) {
        return;
    }
    let _ = s.updater.ClearAll();
    if state.metadata.media_type == PbMediaType::Audio {
        let _ = s.updater.SetType(MediaPlaybackType::Music);
        if let Ok(music) = s.updater.MusicProperties() {
            let _ = music.SetTitle(&HSTRING::from(&state.metadata.title));
            let _ = music.SetArtist(&HSTRING::from(&state.metadata.artist));
            let _ = music.SetAlbumTitle(&HSTRING::from(&state.metadata.album));
            if state.metadata.track_number > 0 {
                let _ = music.SetTrackNumber(state.metadata.track_number as u32);
            }
        }
    } else {
        let _ = s.updater.SetType(MediaPlaybackType::Video);
        if let Ok(video) = s.updater.VideoProperties() {
            let _ = video.SetTitle(&HSTRING::from(&state.metadata.title));
            if !state.metadata.artist.is_empty() {
                let _ = video.SetSubtitle(&HSTRING::from(&state.metadata.artist));
            }
        }
    }
    if let Some(thumb) = &s.cached_thumbnail {
        let _ = s.updater.SetThumbnail(thumb);
    }
    let _ = s.updater.Update();
    update_timeline(state, s);
}

fn update_timeline(state: &WinState, s: &Smtc) {
    let _ = s.smtc.SetPlaybackRate(state.snapshot.rate);
    let Ok(tl) = timeline_properties(&state.snapshot) else {
        return;
    };
    let _ = s.smtc.UpdateTimelineProperties(&tl);
}

fn timeline_properties(
    snapshot: &PlaybackSnapshot,
) -> windows_core::Result<SystemMediaTransportControlsTimelineProperties> {
    // Windows uses 100 ns ticks. Unknown duration clears the previous range;
    // clamp position so SMTC never receives a seek range with position
    // outside its endpoints while a stream's duration changes.
    let duration = snapshot.duration_us.max(0);
    let to_ticks = |us: i64| TimeSpan {
        Duration: us.saturating_mul(10),
    };
    let tl = SystemMediaTransportControlsTimelineProperties::new()?;
    tl.SetStartTime(TimeSpan { Duration: 0 })?;
    tl.SetEndTime(to_ticks(duration))?;
    tl.SetPosition(to_ticks(snapshot.position_us.clamp(0, duration)))?;
    tl.SetMinSeekTime(TimeSpan { Duration: 0 })?;
    tl.SetMaxSeekTime(to_ticks(duration))?;
    Ok(tl)
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut needed: u32 = 0;
    unsafe {
        let ok = CryptStringToBinaryA(bytes, CRYPT_STRING_BASE64, None, &mut needed, None, None);
        if ok.is_err() || needed == 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let mut got = needed;
        let ok = CryptStringToBinaryA(
            bytes,
            CRYPT_STRING_BASE64,
            Some(buf.as_mut_ptr()),
            &mut got,
            None,
            None,
        );
        if ok.is_err() {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }
}

fn make_thumbnail_stream(bytes: &[u8]) -> Option<RandomAccessStreamReference> {
    let stream = InMemoryRandomAccessStream::new().ok()?;
    let writer = DataWriter::CreateDataWriter(&stream).ok()?;
    writer.WriteBytes(bytes).ok()?;
    let _ = writer.StoreAsync().ok()?.join().ok()?;
    let _ = writer.DetachStream();
    stream.Seek(0).ok()?;
    RandomAccessStreamReference::CreateFromStream(&stream).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow};
    use windows::core::w;

    struct Apartment;

    impl Apartment {
        fn new() -> windows_core::Result<Self> {
            unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
            Ok(Self)
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { RoUninitialize() };
        }
    }

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn event(
        kind: PlaybackEventKind,
        rate: f64,
        duration_us: i64,
        position_us: i64,
    ) -> PlaybackEvent {
        PlaybackEvent {
            kind,
            flag: false,
            error_message: String::new(),
            snapshot: PlaybackSnapshot {
                rate,
                duration_us,
                position_us,
                ..Default::default()
            },
            metadata: MediaMetadata::default(),
            artwork_uri: String::new(),
            can_go_next: false,
            can_go_prev: false,
        }
    }

    #[test]
    fn rate_and_duration_changes_refresh_smtc_without_waiting_for_position()
    -> windows_core::Result<()> {
        let _apartment = Apartment::new()?;
        let window = TestWindow(unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }?);
        let interop: ISystemMediaTransportControlsInterop = unsafe {
            RoGetActivationFactory(&HSTRING::from("Windows.Media.SystemMediaTransportControls"))
        }?;
        let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(window.0) }?;
        let mut smtc = Some(Smtc {
            updater: controls.DisplayUpdater()?,
            smtc: controls.clone(),
            button_token: 0,
            seek_token: 0,
            cached_thumbnail: None,
        });
        let mut state = WinState::default();
        assert!(state.throttle.due(Instant::now(), false));
        deliver(
            &mut state,
            &mut smtc,
            &event(PlaybackEventKind::RateChanged, 1.5, 60_000_000, 12_000_000),
        );
        assert_eq!(controls.PlaybackRate()?, 1.5);
        assert_eq!(state.snapshot.position_us, 12_000_000);
        // A duration update must publish immediately too, even with no new
        // PositionChanged and an already-used throttle allowance.
        deliver(
            &mut state,
            &mut smtc,
            &event(
                PlaybackEventKind::DurationChanged,
                2.0,
                90_000_000,
                15_000_000,
            ),
        );
        assert_eq!(controls.PlaybackRate()?, 2.0);
        let timeline = timeline_properties(&state.snapshot)?;
        assert_eq!(timeline.EndTime()?.Duration, 900_000_000);
        assert_eq!(timeline.MaxSeekTime()?.Duration, 900_000_000);
        assert_eq!(timeline.Position()?.Duration, 150_000_000);
        Ok(())
    }

    #[test]
    fn paused_timeline_uses_observed_duration_and_current_position() -> windows_core::Result<()> {
        let _apartment = Apartment::new()?;
        let mut state = WinState::default();
        let mut metadata = event(
            PlaybackEventKind::MetadataChanged,
            1.5,
            60_000_000,
            5_000_000,
        );
        metadata.metadata.duration_us = 120_000_000;
        deliver(&mut state, &mut None, &metadata);
        deliver(
            &mut state,
            &mut None,
            &event(PlaybackEventKind::Paused, 1.5, 60_000_000, 12_000_000),
        );
        let timeline = timeline_properties(&state.snapshot)?;
        assert_eq!(timeline.EndTime()?.Duration, 600_000_000);
        assert_eq!(timeline.Position()?.Duration, 120_000_000);
        Ok(())
    }

    #[test]
    fn unknown_duration_and_out_of_range_positions_produce_valid_timelines()
    -> windows_core::Result<()> {
        let _apartment = Apartment::new()?;
        for (duration_us, position_us, expected_end, expected_position) in [
            (0, 12_000_000, 0, 0),
            (-1, 12_000_000, 0, 0),
            (60_000_000, -1, 600_000_000, 0),
            (60_000_000, 90_000_000, 600_000_000, 600_000_000),
            (i64::MAX, i64::MAX, i64::MAX, i64::MAX),
        ] {
            let snapshot = PlaybackSnapshot {
                duration_us,
                position_us,
                ..Default::default()
            };
            let timeline = timeline_properties(&snapshot)?;
            assert_eq!(timeline.StartTime()?.Duration, 0);
            assert_eq!(timeline.MinSeekTime()?.Duration, 0);
            assert_eq!(timeline.EndTime()?.Duration, expected_end);
            assert_eq!(timeline.MaxSeekTime()?.Duration, expected_end);
            assert_eq!(timeline.Position()?.Duration, expected_position);
        }
        Ok(())
    }
}
