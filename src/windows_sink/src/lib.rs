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
use windows::Win32::Foundation::{E_HANDLE, HWND, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Security::Cryptography::{CRYPT_STRING_BASE64, CryptStringToBinaryA};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::WinRT::{ISystemMediaTransportControlsInterop, RoGetActivationFactory};
use windows::core::{HRESULT, HSTRING};

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
    apartment: Option<ComApartment>,
    state: WinState,
}

impl WindowsSink {
    fn new(hwnd_raw: isize) -> Self {
        Self {
            hwnd_raw,
            smtc: None,
            apartment: None,
            state: WinState::default(),
        }
    }
}

impl QueuedSink for WindowsSink {
    fn init(&mut self) {
        match try_init_smtc(self.hwnd_raw) {
            Ok((smtc, apartment)) => {
                self.smtc = Some(smtc);
                self.apartment = Some(apartment);
            }
            Err(err) => {
                tracing::error!(target: "Media", "{err}");
            }
        }
    }

    fn deliver(&mut self, ev: &PlaybackEvent) {
        deliver(&mut self.state, &mut self.smtc, ev);
    }

    fn teardown(&mut self) {
        if let Some(s) = self.smtc.take() {
            let status = teardown_smtc(s);
            if !status.all_succeeded() {
                tracing::warn!(
                    target: "Media",
                    "[SMTC] teardown completed with errors: {status:?}"
                );
            }
        }
        // Release all COM/WinRT objects above before balancing CoInitializeEx.
        drop(self.apartment.take());
    }
}

/// Structured error preserving the failing SMTC operation and its `HRESULT`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct SmtcError {
    operation: &'static str,
    hr: HRESULT,
    detail: String,
}

impl SmtcError {
    fn from_win32(operation: &'static str, err: windows_core::Error) -> Self {
        Self {
            operation,
            hr: err.code(),
            detail: err.message(),
        }
    }

    fn custom(operation: &'static str, hr: HRESULT, detail: impl Into<String>) -> Self {
        Self {
            operation,
            hr,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for SmtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[SMTC] {} failed (hr={:#010x}): {}",
            self.operation, self.hr.0, self.detail
        )
    }
}

/// Tracks COM apartment initialization on the sink thread and balances
/// `S_OK` / `S_FALSE` initializations with `CoUninitialize` on drop.
#[derive(Debug)]
struct ComApartment {
    hr: HRESULT,
    should_uninitialize: bool,
}

impl ComApartment {
    fn initialize() -> Result<Self, SmtcError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK {
            Ok(Self {
                hr,
                should_uninitialize: true,
            })
        } else if hr == S_FALSE {
            tracing::debug!(
                target: "Media",
                "[SMTC] CoInitializeEx(COINIT_MULTITHREADED) returned S_FALSE (apartment already initialized; balancing with CoUninitialize on teardown)"
            );
            Ok(Self {
                hr,
                should_uninitialize: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            tracing::warn!(
                target: "Media",
                "[SMTC] CoInitializeEx(COINIT_MULTITHREADED) returned RPC_E_CHANGED_MODE ({:#010x}); using existing thread apartment without extra CoUninitialize",
                hr.0,
            );
            Ok(Self {
                hr,
                should_uninitialize: false,
            })
        } else {
            Err(SmtcError::from_win32(
                "CoInitializeEx(COINIT_MULTITHREADED)",
                windows_core::Error::from_hresult(hr),
            ))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe { CoUninitialize() };
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

fn check_smtc<T>(operation: &'static str, result: windows_core::Result<T>) -> Result<T, SmtcError> {
    result.map_err(|err| SmtcError::from_win32(operation, err))
}

fn log_smtc_result<T>(operation: &'static str, result: windows_core::Result<T>) -> Option<T> {
    match result {
        Ok(val) => Some(val),
        Err(err) => {
            tracing::warn!(
                target: "Media",
                "[SMTC] {operation} failed (hr={:#010x}): {err}",
                err.code().0,
            );
            None
        }
    }
}

fn try_init_smtc(hwnd_raw: isize) -> Result<(Smtc, ComApartment), SmtcError> {
    let apartment = ComApartment::initialize()?;
    if hwnd_raw == 0 {
        return Err(SmtcError::custom(
            "validate mpv HWND",
            E_HANDLE,
            "NULL HWND provided",
        ));
    }
    let interop: ISystemMediaTransportControlsInterop = check_smtc(
        "RoGetActivationFactory(ISystemMediaTransportControlsInterop)",
        unsafe {
            RoGetActivationFactory(&HSTRING::from("Windows.Media.SystemMediaTransportControls"))
        },
    )?;
    let hwnd = HWND(hwnd_raw as *mut _);
    let smtc: SystemMediaTransportControls = check_smtc(
        "ISystemMediaTransportControlsInterop::GetForWindow",
        unsafe { interop.GetForWindow::<SystemMediaTransportControls>(hwnd) },
    )?;

    check_smtc(
        "SystemMediaTransportControls::SetIsEnabled(true)",
        smtc.SetIsEnabled(true),
    )?;
    check_smtc(
        "SystemMediaTransportControls::SetIsPlayEnabled(true)",
        smtc.SetIsPlayEnabled(true),
    )?;
    check_smtc(
        "SystemMediaTransportControls::SetIsPauseEnabled(true)",
        smtc.SetIsPauseEnabled(true),
    )?;
    check_smtc(
        "SystemMediaTransportControls::SetIsStopEnabled(true)",
        smtc.SetIsStopEnabled(true),
    )?;
    check_smtc(
        "SystemMediaTransportControls::SetIsNextEnabled(false)",
        smtc.SetIsNextEnabled(false),
    )?;
    check_smtc(
        "SystemMediaTransportControls::SetIsPreviousEnabled(false)",
        smtc.SetIsPreviousEnabled(false),
    )?;

    let updater = check_smtc(
        "SystemMediaTransportControls::DisplayUpdater",
        smtc.DisplayUpdater(),
    )?;

    let button_token = {
        let handler = windows::Foundation::TypedEventHandler::new(
            |_sender: windows_core::Ref<SystemMediaTransportControls>,
             args: windows_core::Ref<
                SystemMediaTransportControlsButtonPressedEventArgs,
            >| {
                if let Some(args) = args.as_ref()
                    && let Some(button) =
                        log_smtc_result("ButtonPressed::Button", args.Button())
                {
                    on_button_pressed(button);
                }
                Ok(())
            },
        );
        check_smtc(
            "SystemMediaTransportControls::ButtonPressed",
            smtc.ButtonPressed(&handler),
        )?
    };

    let seek_token = {
        let handler =
            windows::Foundation::TypedEventHandler::new(
                |_sender: windows_core::Ref<SystemMediaTransportControls>,
                 args: windows_core::Ref<
                    windows::Media::PlaybackPositionChangeRequestedEventArgs,
                >| {
                    if let Some(args) = args.as_ref()
                        && let Some(span) = log_smtc_result(
                            "PlaybackPositionChangeRequested::RequestedPlaybackPosition",
                            args.RequestedPlaybackPosition(),
                        )
                    {
                        let pos_us = span.Duration / 10;
                        sink_core::seek_to_ms(pos_us / 1000);
                    }
                    Ok(())
                },
            );
        match check_smtc(
            "SystemMediaTransportControls::PlaybackPositionChangeRequested",
            smtc.PlaybackPositionChangeRequested(&handler),
        ) {
            Ok(token) => token,
            Err(err) => {
                let _ = smtc.RemoveButtonPressed(button_token);
                return Err(err);
            }
        }
    };

    tracing::info!(
        target: "Media",
        "[SMTC] Initialized (apartment hr={:#010x})",
        apartment.hr.0,
    );
    Ok((
        Smtc {
            smtc,
            updater,
            button_token,
            seek_token,
            cached_thumbnail: None,
        },
        apartment,
    ))
}

#[derive(Debug)]
struct TeardownStatus {
    button_removed: bool,
    seek_removed: bool,
    cleared: bool,
    updated: bool,
    disabled: bool,
}

impl TeardownStatus {
    const fn all_succeeded(&self) -> bool {
        self.button_removed && self.seek_removed && self.cleared && self.updated && self.disabled
    }
}

fn teardown_smtc(s: Smtc) -> TeardownStatus {
    let button_removed = log_smtc_result(
        "SystemMediaTransportControls::RemoveButtonPressed",
        s.smtc.RemoveButtonPressed(s.button_token),
    )
    .is_some();
    let seek_removed = log_smtc_result(
        "SystemMediaTransportControls::RemovePlaybackPositionChangeRequested",
        s.smtc.RemovePlaybackPositionChangeRequested(s.seek_token),
    )
    .is_some();
    let cleared = log_smtc_result("DisplayUpdater::ClearAll", s.updater.ClearAll()).is_some();
    let updated = log_smtc_result("DisplayUpdater::Update", s.updater.Update()).is_some();
    let disabled = log_smtc_result(
        "SystemMediaTransportControls::SetIsEnabled(false)",
        s.smtc.SetIsEnabled(false),
    )
    .is_some();
    TeardownStatus {
        button_removed,
        seek_removed,
        cleared,
        updated,
        disabled,
    }
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
            if state.metadata == ev.metadata {
                return;
            }
            let item_changed = !state.metadata.id.is_empty() && ev.metadata.id != state.metadata.id;
            state.metadata = ev.metadata.clone();
            if let Some(s) = smtc.as_mut() {
                if item_changed {
                    s.cached_thumbnail = None;
                }
                update_display_properties(state, s);
            }
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
                log_smtc_result(
                    "DisplayUpdater::SetThumbnail",
                    smtc.updater.SetThumbnail(&ref_stream),
                );
                log_smtc_result("DisplayUpdater::Update", smtc.updater.Update());
                smtc.cached_thumbnail = Some(ref_stream);
            }
        }
        PlaybackEventKind::QueueCapsChanged => {
            if let Some(s) = smtc.as_ref() {
                log_smtc_result(
                    "SystemMediaTransportControls::SetIsNextEnabled",
                    s.smtc.SetIsNextEnabled(ev.can_go_next),
                );
                log_smtc_result(
                    "SystemMediaTransportControls::SetIsPreviousEnabled",
                    s.smtc.SetIsPreviousEnabled(ev.can_go_prev),
                );
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
                    log_smtc_result(
                        "SystemMediaTransportControls::SetPlaybackStatus(Playing)",
                        s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Playing),
                    );
                    update_display_properties(state, s);
                }
                Phase::Paused => {
                    log_smtc_result(
                        "SystemMediaTransportControls::SetPlaybackStatus(Paused)",
                        s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Paused),
                    );
                    update_display_properties(state, s);
                }
                Phase::Stopped => {
                    state.metadata = MediaMetadata::default();
                    state.snapshot = PlaybackSnapshot::default();
                    s.cached_thumbnail = None;
                    log_smtc_result("DisplayUpdater::ClearAll", s.updater.ClearAll());
                    log_smtc_result("DisplayUpdater::Update", s.updater.Update());
                    log_smtc_result(
                        "SystemMediaTransportControls::SetPlaybackStatus(Stopped)",
                        s.smtc.SetPlaybackStatus(MediaPlaybackStatus::Stopped),
                    );
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
    log_smtc_result("DisplayUpdater::ClearAll", s.updater.ClearAll());
    if state.metadata.media_type == PbMediaType::Audio {
        log_smtc_result(
            "DisplayUpdater::SetType(Music)",
            s.updater.SetType(MediaPlaybackType::Music),
        );
        if let Some(music) = log_smtc_result(
            "DisplayUpdater::MusicProperties",
            s.updater.MusicProperties(),
        ) {
            log_smtc_result(
                "MusicDisplayProperties::SetTitle",
                music.SetTitle(&HSTRING::from(&state.metadata.title)),
            );
            log_smtc_result(
                "MusicDisplayProperties::SetArtist",
                music.SetArtist(&HSTRING::from(&state.metadata.artist)),
            );
            log_smtc_result(
                "MusicDisplayProperties::SetAlbumTitle",
                music.SetAlbumTitle(&HSTRING::from(&state.metadata.album)),
            );
            if state.metadata.track_number > 0 {
                log_smtc_result(
                    "MusicDisplayProperties::SetTrackNumber",
                    music.SetTrackNumber(state.metadata.track_number as u32),
                );
            }
        }
    } else {
        log_smtc_result(
            "DisplayUpdater::SetType(Video)",
            s.updater.SetType(MediaPlaybackType::Video),
        );
        if let Some(video) = log_smtc_result(
            "DisplayUpdater::VideoProperties",
            s.updater.VideoProperties(),
        ) {
            log_smtc_result(
                "VideoDisplayProperties::SetTitle",
                video.SetTitle(&HSTRING::from(&state.metadata.title)),
            );
            if !state.metadata.artist.is_empty() {
                log_smtc_result(
                    "VideoDisplayProperties::SetSubtitle",
                    video.SetSubtitle(&HSTRING::from(&state.metadata.artist)),
                );
            }
        }
    }
    if let Some(thumb) = &s.cached_thumbnail {
        log_smtc_result(
            "DisplayUpdater::SetThumbnail",
            s.updater.SetThumbnail(thumb),
        );
    }
    log_smtc_result("DisplayUpdater::Update", s.updater.Update());
    update_timeline(state, s);
}

fn update_timeline(state: &WinState, s: &Smtc) {
    log_smtc_result(
        "SystemMediaTransportControls::SetPlaybackRate",
        s.smtc.SetPlaybackRate(state.snapshot.rate),
    );
    let Some(tl) = log_smtc_result(
        "SystemMediaTransportControlsTimelineProperties",
        timeline_properties(&state.snapshot),
    ) else {
        return;
    };
    log_smtc_result(
        "SystemMediaTransportControls::UpdateTimelineProperties",
        s.smtc.UpdateTimelineProperties(&tl),
    );
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
    let stream = log_smtc_result(
        "InMemoryRandomAccessStream::new",
        InMemoryRandomAccessStream::new(),
    )?;
    let writer = log_smtc_result(
        "DataWriter::CreateDataWriter",
        DataWriter::CreateDataWriter(&stream),
    )?;
    log_smtc_result("DataWriter::WriteBytes", writer.WriteBytes(bytes))?;
    let store_op = log_smtc_result("DataWriter::StoreAsync", writer.StoreAsync())?;
    let _ = log_smtc_result("DataWriter::StoreAsync::join", store_op.join())?;
    let _ = log_smtc_result("DataWriter::DetachStream", writer.DetachStream());
    log_smtc_result("InMemoryRandomAccessStream::Seek", stream.Seek(0))?;
    log_smtc_result(
        "RandomAccessStreamReference::CreateFromStream",
        RandomAccessStreamReference::CreateFromStream(&stream),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
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

    impl TestWindow {
        fn new() -> windows_core::Result<Self> {
            Ok(Self(unsafe {
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
            }?))
        }
    }

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
        let window = TestWindow::new()?;
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

    #[test]
    fn same_id_metadata_changes_refresh_smtc_display_properties() -> windows_core::Result<()> {
        let window = TestWindow::new()?;
        let (s, _apartment) = try_init_smtc(window.0.0 as isize)
            .map_err(|e| windows_core::Error::from_hresult(e.hr))?;
        let updater = s.updater.clone();
        let mut smtc = Some(s);
        let mut state = WinState::default();

        deliver(
            &mut state,
            &mut smtc,
            &event(PlaybackEventKind::Started, 1.0, 60_000_000, 0),
        );

        let mut initial_video = event(PlaybackEventKind::MetadataChanged, 1.0, 60_000_000, 0);
        initial_video.metadata = MediaMetadata {
            id: "item-100".to_owned(),
            title: "Original Video Title".to_owned(),
            artist: "Original Subtitle".to_owned(),
            media_type: PbMediaType::Video,
            ..Default::default()
        };
        deliver(&mut state, &mut smtc, &initial_video);
        assert_eq!(updater.Type()?, MediaPlaybackType::Video);
        assert_eq!(
            updater.VideoProperties()?.Title()?.to_string_lossy(),
            "Original Video Title"
        );
        assert_eq!(
            updater.VideoProperties()?.Subtitle()?.to_string_lossy(),
            "Original Subtitle"
        );

        // Update title and subtitle while keeping the exact same media ID.
        let mut updated_video = event(PlaybackEventKind::MetadataChanged, 1.0, 60_000_000, 0);
        updated_video.metadata = MediaMetadata {
            id: "item-100".to_owned(),
            title: "Updated Video Title".to_owned(),
            artist: "Updated Subtitle".to_owned(),
            media_type: PbMediaType::Video,
            ..Default::default()
        };
        deliver(&mut state, &mut smtc, &updated_video);
        assert_eq!(
            updater.VideoProperties()?.Title()?.to_string_lossy(),
            "Updated Video Title"
        );
        assert_eq!(
            updater.VideoProperties()?.Subtitle()?.to_string_lossy(),
            "Updated Subtitle"
        );

        // Same-ID audio metadata refresh (title, artist, album, track number).
        let mut updated_audio = event(PlaybackEventKind::MetadataChanged, 1.0, 60_000_000, 0);
        updated_audio.metadata = MediaMetadata {
            id: "item-100".to_owned(),
            title: "Track Title".to_owned(),
            artist: "Track Artist".to_owned(),
            album: "Track Album".to_owned(),
            track_number: 7,
            media_type: PbMediaType::Audio,
            ..Default::default()
        };
        deliver(&mut state, &mut smtc, &updated_audio);
        assert_eq!(updater.Type()?, MediaPlaybackType::Music);
        let music = updater.MusicProperties()?;
        assert_eq!(music.Title()?.to_string_lossy(), "Track Title");
        assert_eq!(music.Artist()?.to_string_lossy(), "Track Artist");
        assert_eq!(music.AlbumTitle()?.to_string_lossy(), "Track Album");
        assert_eq!(music.TrackNumber()?, 7);

        if let Some(s) = smtc.take() {
            let status = teardown_smtc(s);
            assert!(status.button_removed);
            assert!(status.seek_removed);
        }
        Ok(())
    }

    #[test]
    fn failed_smtc_startup_reports_failing_operation_and_hresult() -> Result<(), String> {
        let null_err = try_init_smtc(0)
            .err()
            .ok_or_else(|| "null HWND must fail SMTC initialization".to_owned())?;
        assert_eq!(null_err.operation, "validate mpv HWND");
        assert_eq!(null_err.hr, E_HANDLE);
        assert!(null_err.to_string().contains("validate mpv HWND"));
        assert!(null_err.to_string().contains("0x80070006"));

        let invalid_hwnd_err = try_init_smtc(-1)
            .err()
            .ok_or_else(|| "invalid HWND must fail GetForWindow".to_owned())?;
        assert_eq!(
            invalid_hwnd_err.operation,
            "ISystemMediaTransportControlsInterop::GetForWindow"
        );
        assert!(invalid_hwnd_err.hr.is_err());
        assert!(
            invalid_hwnd_err
                .to_string()
                .contains("ISystemMediaTransportControlsInterop::GetForWindow")
        );
        Ok(())
    }

    #[test]
    fn com_apartment_handles_fresh_nested_mta_and_existing_sta_initialization() {
        let join = std::thread::spawn(|| {
            // 1. Fresh thread: CoInitializeEx returns S_OK and must uninitialize on drop.
            let first = ComApartment::initialize().ok();
            assert_eq!(first.as_ref().map(|a| a.hr), Some(S_OK));
            assert_eq!(first.as_ref().map(|a| a.should_uninitialize), Some(true));

            // 2. Nested MTA on the same thread: returns S_FALSE and also balances on drop.
            let nested = ComApartment::initialize().ok();
            assert_eq!(nested.as_ref().map(|a| a.hr), Some(S_FALSE));
            assert_eq!(nested.as_ref().map(|a| a.should_uninitialize), Some(true));
            drop(nested);
            drop(first);

            // 3. After both dropped, the thread apartment is fully uninitialized, so STA succeeds with S_OK.
            let sta_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            assert_eq!(sta_hr, S_OK);

            // 4. Initializing ComApartment on an STA thread observes RPC_E_CHANGED_MODE
            // and does not call CoUninitialize on drop.
            let changed = ComApartment::initialize().ok();
            assert_eq!(changed.as_ref().map(|a| a.hr), Some(RPC_E_CHANGED_MODE));
            assert_eq!(changed.as_ref().map(|a| a.should_uninitialize), Some(false));
            drop(changed);

            unsafe { CoUninitialize() };
        });
        assert!(join.join().is_ok());
    }

    #[test]
    fn normal_shutdown_removes_handlers_disables_controls_and_tears_down_apartment()
    -> windows_core::Result<()> {
        let window = TestWindow::new()?;
        let mut sink = WindowsSink::new(window.0.0 as isize);
        sink.init();
        assert!(sink.smtc.is_some());
        assert!(sink.apartment.is_some());

        if let Some(s) = sink.smtc.take() {
            let controls = s.smtc.clone();
            assert!(controls.IsEnabled()?);
            let status = teardown_smtc(s);
            assert!(status.button_removed);
            assert!(status.seek_removed);
            assert!(status.cleared);
            assert!(status.updated);
            assert!(status.disabled);
            assert!(!controls.IsEnabled()?);
        }
        sink.teardown();
        assert!(sink.smtc.is_none());
        assert!(sink.apartment.is_none());
        Ok(())
    }
}
