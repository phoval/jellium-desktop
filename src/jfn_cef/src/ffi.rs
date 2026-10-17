//! CEF process bootstrap.

use cef::*;
#[cfg(not(windows))]
use std::ffi::CString;
#[cfg(not(windows))]
use std::os::raw::c_char;
use std::os::raw::c_int;
#[cfg(not(windows))]
use std::sync::OnceLock;

use jfn_platform_abi::DisplayBackend;

use crate::app::{JfnApp, JfnAppBuilder};
use crate::state;

// jfn constructs Chromium's `MainArgs` itself. Two entry points:
//
// * Browser process (`jfn_cef_initialize`): `MainArgs` is `[argv[0]]` —
//   see `browser_main_args`. Chromium's `base::CommandLine` parses only
//   the program name; no jfn CLI flag is ever in there.
// * Subprocess (`jfn_cef_start`): Chromium spawned this binary with an
//   argv it authored itself (`--type=renderer …` etc.). That argv is
//   forwarded to `execute_process` so CEF can dispatch on `--type=`.
//
// Initialization builds one immutable switch snapshot for App callbacks.
// Chromium switch names are mapped explicitly, never passed through from CLI.
//
// CEF refcounts the App; constructing a fresh one in each FFI call is
// safe because the underlying object outlives the local once CEF has
// captured its reference.

/// Subprocess dispatch + browser-process App construction.
/// Returns -1 in the browser process (continue startup); returns the
/// subprocess exit code otherwise.
pub(crate) fn jfn_cef_start(_runtime: &crate::LoadedCef) -> c_int {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let mut app = JfnAppBuilder::new(JfnApp::new());
    execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    )
}

fn platform_switches(backend: DisplayBackend) -> Vec<state::PendingSwitch> {
    let mut c = state::Config::default();
    match backend {
        DisplayBackend::Wayland => {
            c.pending_switches.push(state::PendingSwitch::with_value(
                "ozone-platform",
                "wayland",
            ));
            // OSR honors GetScreenInfo device_scale_factor only without the
            // fractional-scale protocol.
            c.pending_switches.push(state::PendingSwitch::with_value(
                "disable-features",
                "WaylandFractionalScaleV1",
            ));
        }
        DisplayBackend::X11 => {
            c.pending_switches
                .push(state::PendingSwitch::with_value("ozone-platform", "x11"));
        }
        DisplayBackend::MacOS => {
            c.pending_switches
                .push(state::PendingSwitch::flag("single-process"));
            c.pending_switches
                .push(state::PendingSwitch::flag("use-mock-keychain"));
            c.pending_switches
                .push(state::PendingSwitch::with_value("password-store", "basic"));
        }
        DisplayBackend::Windows => {
            #[cfg(windows)]
            {
                let luid =
                    jfn_gpu_paint::surfaces().and_then(jfn_gpu_paint::Surfaces::adapter_luid);
                c.pending_switches.extend(windows_switches_for_luid(luid));
            }
            #[cfg(not(windows))]
            {
                c.pending_switches
                    .push(state::PendingSwitch::with_value("use-angle", "d3d11"));
            }
        }
    };
    c.pending_switches
}

#[cfg(windows)]
fn windows_switches_for_luid(luid: Option<jfn_gpu_paint::ProducerId>) -> Vec<state::PendingSwitch> {
    let mut switches = vec![state::PendingSwitch::with_value("use-angle", "d3d11")];
    if let Some(luid) = luid {
        switches.push(state::PendingSwitch::with_value(
            "use-adapter-luid",
            &jfn_gpu_paint::format_luid_switch(luid),
        ));
    }
    switches
}

#[cfg(windows)]
pub(crate) fn format_windows_cef_graphics_baseline(
    adapter: Option<&jfn_gpu_paint::AdapterDiagnostics>,
    disable_gpu_compositing: bool,
) -> String {
    let gpu_compositing = if disable_gpu_compositing {
        "disabled (disable-gpu-compositing)"
    } else {
        "enabled"
    };
    let (overlay_summary, alignment_summary, shared_import_summary) = match adapter {
        Some(diag) => {
            let luid_switch = diag.adapter_luid.map(jfn_gpu_paint::format_luid_switch);
            let alignment = match (&luid_switch, diag.producer_match.is_compatible()) {
                (Some(luid), true) => format!("matched (use-adapter-luid={luid})"),
                (Some(luid), false) => format!(
                    "mismatch ({}; use-adapter-luid={luid})",
                    diag.producer_match
                ),
                (None, _) => {
                    "unpinned (overlay adapter LUID unavailable; adapter mismatch risk)".to_owned()
                }
            };
            let shared_enabled =
                !disable_gpu_compositing && diag.can_import_shared && diag.adapter_luid.is_some();
            let shared = if shared_enabled {
                "enabled".to_owned()
            } else if disable_gpu_compositing {
                "disabled (disable-gpu-compositing)".to_owned()
            } else if !diag.can_import_shared {
                format!(
                    "disabled (overlay can_import_shared=false, producer_match={})",
                    diag.producer_match
                )
            } else {
                "disabled (unpinned producer adapter LUID)".to_owned()
            };
            let overlay = format!(
                "\"{}\" (backend={:?}, type={:?}, vendor={:#06x}, device={:#06x}, luid={}, producer_match={}, can_import_shared={})",
                diag.name,
                diag.backend,
                diag.device_type,
                diag.vendor,
                diag.device,
                diag.luid_summary(),
                diag.producer_match,
                diag.can_import_shared,
            );
            (overlay, alignment, shared)
        }
        None => (
            "uninitialized".to_owned(),
            "unpinned (overlay D3D12 device not initialized; adapter mismatch risk)".to_owned(),
            "disabled (overlay D3D12 device not initialized)".to_owned(),
        ),
    };
    format!(
        "Windows graphics baseline: [cef-angle] backend=d3d11 (use-angle=d3d11) gpu_compositing={gpu_compositing} adapter_alignment={alignment_summary}; [cef-shared-import] status={shared_import_summary}; [overlay-d3d12] adapter={overlay_summary}; note: [mpv-video-decode] d3d11va is separate from CEF/ANGLE and overlay D3D12, and Chromium negative frame-latency logs are separate observations unless paired with a [cef-shared-import] or [overlay-d3d12] failure"
    )
}

/// Builds CefSettings and calls `CefInitialize` once for the browser owner.
pub(crate) fn jfn_cef_initialize(
    _runtime: &crate::LoadedCef,
    platform: &dyn jfn_platform_abi::Platform,
    options: &crate::InitOptions,
) -> Result<(), crate::InitError> {
    let mut pending_switches = platform_switches(platform.display());
    if options.disable_gpu_compositing {
        pending_switches.push(state::PendingSwitch::flag("disable-gpu-compositing"));
    }
    #[cfg(windows)]
    if platform.display() == DisplayBackend::Windows {
        let diag = jfn_gpu_paint::surfaces().map(jfn_gpu_paint::Surfaces::diagnostics);
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Info,
            &format_windows_cef_graphics_baseline(diag, options.disable_gpu_compositing),
        );
    }
    state::configure(state::Config { pending_switches });

    // Settings.json singleton must be initialized before the renderer
    // process reads it during OnContextCreated.
    let settings_path = jfn_paths::config_dir().join("settings.json");
    jfn_config::settings_init(&settings_path);

    let mut settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        disable_signal_handlers: 1,
        log_severity: options.log_severity.native(),
        remote_debugging_port: options.remote_debugging_port.native(),
        locale: CefString::from("en-US"),
        user_agent: CefString::from(concat!(
            "Mozilla/5.0 jellium-desktop/",
            env!("JFN_APP_VERSION")
        )),
        root_cache_path: CefString::from(jfn_paths::cache_dir().to_string_lossy().as_ref()),
        ..Settings::default()
    };
    let cef_host = platform.cef_host();
    if cef_host.is_some() {
        settings.external_message_pump = 1;
    } else {
        settings.multi_threaded_message_loop = 1;
    }

    fill_paths(&mut settings, platform);

    // An external pump must install its run-loop hooks before
    // CefInitialize so the first OnScheduleMessagePumpWork (fired
    // synchronously during init) finds them ready.
    if let Some(host) = cef_host {
        host.pump_init();
    }

    // chrome/browser/chrome_browser_main_posix.cc installs SIGINT/SIGTERM
    // handlers during CefInitialize and that path is not gated by
    // disable_signal_handlers. Snapshot the caller's handlers and restore
    // afterward so Chromium's installs are confined to the init window.
    let _sig_guard = jfn_platform_abi::SignalGuard::new();

    let mut app = JfnAppBuilder::new(JfnApp::new());
    let full_argv = platform.display().cef_full_browser_argv();
    let main_args = if full_argv {
        args::Args::new().as_main_args().clone()
    } else {
        // Windows `MainArgs` carries an HINSTANCE, not argv, and always
        // takes the `full_argv` path above — `browser_main_args` is
        // unbuildable there.
        #[cfg(not(windows))]
        {
            browser_main_args()
        }
        #[cfg(windows)]
        {
            args::Args::new().as_main_args().clone()
        }
    };
    let ok = initialize(
        Some(&main_args),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if !ok {
        if let Some(host) = cef_host {
            host.pump_shutdown();
        }
        crate::ready::stop();
        return Err(crate::InitError::Native);
    }
    if crate::ready::post_cef_ready().is_err() {
        if let Some(host) = cef_host {
            host.pump_shutdown();
        }
        crate::ready::stop();
        shutdown();
        return Err(crate::InitError::Readiness);
    }
    Ok(())
}

// Construct Chromium's browser-process `MainArgs` as `[argv[0]]`. The
// CString + pointer Vec are leaked into process-lifetime statics because
// CEF retains the raw pointers past the `initialize()` call (and across
// the lifetime of the run loop on some code paths).
#[cfg(not(windows))]
fn browser_main_args() -> MainArgs {
    struct CleanArgv {
        argc: c_int,
        argv: *mut *mut c_char,
    }
    // The pointers are valid for the process lifetime (leaked) and we
    // only hand them to CEF, which treats them as immutable input.
    unsafe impl Send for CleanArgv {}
    unsafe impl Sync for CleanArgv {}

    static CLEAN: OnceLock<CleanArgv> = OnceLock::new();
    let c = CLEAN.get_or_init(|| {
        let program = std::env::args()
            .next()
            .unwrap_or_else(|| "jellium-desktop".to_string());
        let cstr = CString::new(program).unwrap_or_default();
        let cstr_ptr = cstr.as_ptr() as *mut c_char;
        // Keep the backing buffer alive for the process lifetime.
        Box::leak(Box::new(cstr));
        let argv_vec: Vec<*mut c_char> = vec![cstr_ptr];
        let leaked: &'static mut Vec<*mut c_char> = Box::leak(Box::new(argv_vec));
        CleanArgv {
            argc: leaked.len() as c_int,
            argv: leaked.as_mut_ptr(),
        }
    });
    MainArgs {
        argc: c.argc,
        argv: c.argv,
    }
}

pub(crate) fn jfn_cef_shutdown(runtime: &crate::InitializedCef) {
    // Gate further external-pump dispatches before tearing down CEF state.
    if let Some(host) = runtime.platform().cef_host() {
        host.pump_shutdown();
    }
    shutdown();
}

// ---- helpers ---------------------------------------------------------------

fn fill_paths(settings: &mut Settings, platform: &dyn jfn_platform_abi::Platform) {
    let paths = platform.cef_paths();
    let set = |dst: &mut CefString, v: Option<std::path::PathBuf>| {
        if let Some(v) = v {
            *dst = CefString::from(v.to_string_lossy().as_ref());
        }
    };
    set(
        &mut settings.browser_subprocess_path,
        paths.browser_subprocess_path,
    );
    set(&mut settings.framework_dir_path, paths.framework_dir_path);
    set(&mut settings.resources_dir_path, paths.resources_dir_path);
    set(&mut settings.locales_dir_path, paths.locales_dir_path);
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use jfn_gpu_paint::{AdapterDiagnostics, ProducerMatch, wgpu};

    fn sample_adapter(
        adapter_luid: Option<i64>,
        producer_match: ProducerMatch,
        can_import_shared: bool,
    ) -> AdapterDiagnostics {
        AdapterDiagnostics {
            name: "NVIDIA GeForce RTX 4090".to_owned(),
            backend: wgpu::Backend::Dx12,
            device_type: wgpu::DeviceType::DiscreteGpu,
            vendor: 0x10de,
            device: 0x2684,
            adapter_luid,
            producer_match,
            import_capable: true,
            can_import_shared,
        }
    }

    #[test]
    fn windows_switches_keep_angle_d3d11_and_pin_luid_when_available() {
        let luid = (2_i64 << 32) | 99;
        assert_eq!(
            windows_switches_for_luid(Some(luid)),
            vec![
                state::PendingSwitch::with_value("use-angle", "d3d11"),
                state::PendingSwitch::with_value("use-adapter-luid", "2,99"),
            ]
        );
        assert_eq!(
            windows_switches_for_luid(None),
            vec![state::PendingSwitch::with_value("use-angle", "d3d11")]
        );
    }

    #[test]
    fn windows_graphics_baseline_reports_matched_adapter_and_enabled_shared_import() {
        let diag = sample_adapter(
            Some((1_i64 << 32) | 0x1234),
            ProducerMatch::Unconstrained,
            true,
        );
        let report = format_windows_cef_graphics_baseline(Some(&diag), false);
        assert!(report.contains("[cef-angle] backend=d3d11 (use-angle=d3d11)"));
        assert!(report.contains("adapter_alignment=matched (use-adapter-luid=1,4660)"));
        assert!(report.contains("[cef-shared-import] status=enabled"));
        assert!(report.contains("[overlay-d3d12] adapter=\"NVIDIA GeForce RTX 4090\""));
        assert!(report.contains("[mpv-video-decode] d3d11va is separate"));
    }

    #[test]
    fn windows_graphics_baseline_reports_adapter_mismatch_and_disabled_shared_import() {
        let diag = sample_adapter(
            Some((1_i64 << 32) | 0x1234),
            ProducerMatch::Mismatch {
                requested: (1_i64 << 32) | 0x9999,
            },
            false,
        );
        let report = format_windows_cef_graphics_baseline(Some(&diag), false);
        assert!(report.contains("adapter_alignment=mismatch"));
        assert!(
            report.contains("[cef-shared-import] status=disabled (overlay can_import_shared=false")
        );
    }

    #[test]
    fn windows_graphics_baseline_reports_disabled_gpu_compositing() {
        let diag = sample_adapter(
            Some((1_i64 << 32) | 0x1234),
            ProducerMatch::Unconstrained,
            true,
        );
        let report = format_windows_cef_graphics_baseline(Some(&diag), true);
        assert!(report.contains("gpu_compositing=disabled (disable-gpu-compositing)"));
        assert!(report.contains("[cef-shared-import] status=disabled (disable-gpu-compositing)"));
    }
}
