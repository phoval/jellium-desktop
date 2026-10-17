//! The process's wgpu device coordinator, shared across surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Kind, SurfaceLost};
use crate::painter::{AlphaSource, Surface};
use crate::shared;
use crate::swapchain::Swapchain;
use crate::types::WindowTarget;
use crate::{FrameSize, ProducerId};

/// The one swapchain format the process presents in, on every window system.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// How the selected overlay GPU adapter relates to a producer adapter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProducerMatch {
    /// No producer adapter was specified when opening the overlay device
    /// (on Windows, the overlay D3D12 device opens first and pins CEF's
    /// ANGLE producer to its LUID via `--use-adapter-luid`).
    Unconstrained,
    /// A specific producer adapter was requested and the selected adapter matches it.
    Matched(ProducerId),
    /// A specific producer adapter was requested, but no matching adapter was found;
    /// fell back to the highest-ranked adapter (`can_import_shared` disabled).
    Mismatch { requested: ProducerId },
}

impl ProducerMatch {
    /// Whether this adapter selection is compatible with shared-texture import.
    pub const fn is_compatible(&self) -> bool {
        matches!(self, Self::Unconstrained | Self::Matched(_))
    }
}

impl std::fmt::Display for ProducerMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unconstrained => f.write_str("unconstrained (overlay selects adapter first)"),
            Self::Matched(id) => write!(f, "matched({id:?})"),
            Self::Mismatch { requested } => write!(f, "mismatch(requested={requested:?})"),
        }
    }
}

/// Diagnostic snapshot of the selected overlay GPU adapter and shared-texture capability.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AdapterDiagnostics {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    pub vendor: u32,
    pub device: u32,
    #[cfg(windows)]
    pub adapter_luid: Option<ProducerId>,
    pub producer_match: ProducerMatch,
    pub import_capable: bool,
    pub can_import_shared: bool,
}

impl AdapterDiagnostics {
    /// Formats the selected adapter's LUID for Windows diagnostic reports.
    #[cfg(windows)]
    pub fn luid_summary(&self) -> String {
        match self.adapter_luid {
            Some(luid) => format!("{} ({luid:#018x})", shared::format_luid_switch(luid)),
            None => "unavailable".to_owned(),
        }
    }
}

/// Lifecycle state of the process-wide overlay GPU device.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeviceLifecycleState {
    /// The active GPU device generation is healthy and ready to present.
    Healthy { generation: u64 },
    /// The GPU device generation was reported lost and awaits coordinated
    /// surface invalidation and device recreation.
    Lost { generation: u64, reason: String },
    /// A replacement device could not safely preserve adapter or shared-texture
    /// invariants; the application must restart rather than continuing with a
    /// stale or mismatched device.
    RestartRequired { generation: u64, reason: String },
}

/// Deterministic outcome of a coordinated GPU device-loss recovery attempt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeviceRecoveryOutcome {
    /// A replacement device was created (or was already created for this loss
    /// event) and verified compatible with the pinned CEF producer adapter.
    Recovered {
        generation: u64,
        diagnostics: AdapterDiagnostics,
    },
    /// Full recovery cannot safely preserve adapter compatibility or device
    /// creation failed; an application-level restart is required.
    RestartRequired { generation: u64, reason: String },
}

/// Pure, deterministic state machine governing GPU device-loss transitions and
/// replacement-adapter compatibility invariants.
///
/// On Windows, CEF's GPU process is launched once with `--use-adapter-luid`
/// pinned to the initial overlay adapter's LUID. Consequently, any replacement
/// overlay D3D12 device opened after device loss must land on that same adapter
/// LUID and retain shared-texture import capability before accelerated-paint
/// imports can safely resume.
#[derive(Clone, Debug)]
pub struct DeviceRecoveryCoordinator {
    state: DeviceLifecycleState,
    required_producer: Option<ProducerId>,
    require_shared_import: bool,
    active_diagnostics: AdapterDiagnostics,
}

impl DeviceRecoveryCoordinator {
    pub fn new(
        initial_diagnostics: AdapterDiagnostics,
        requested_producer: Option<ProducerId>,
    ) -> Self {
        #[cfg(windows)]
        let required_producer = initial_diagnostics.adapter_luid.or(requested_producer);
        #[cfg(not(windows))]
        let required_producer = requested_producer;

        let require_shared_import = initial_diagnostics.can_import_shared;
        Self {
            state: DeviceLifecycleState::Healthy { generation: 1 },
            required_producer,
            require_shared_import,
            active_diagnostics: initial_diagnostics,
        }
    }

    pub fn state(&self) -> &DeviceLifecycleState {
        &self.state
    }

    pub fn required_producer(&self) -> Option<ProducerId> {
        self.required_producer
    }

    pub fn require_shared_import(&self) -> bool {
        self.require_shared_import
    }

    pub fn diagnostics(&self) -> &AdapterDiagnostics {
        &self.active_diagnostics
    }

    /// Records that `generation` was lost. Idempotent if `generation` is
    /// already marked [`DeviceLifecycleState::Lost`], already recovered to a
    /// newer generation, or in [`DeviceLifecycleState::RestartRequired`].
    pub fn report_lost(
        &mut self,
        generation: u64,
        reason: impl Into<String>,
    ) -> DeviceLifecycleState {
        if let DeviceLifecycleState::Healthy { generation: active } = self.state
            && active == generation
        {
            self.state = DeviceLifecycleState::Lost {
                generation,
                reason: reason.into(),
            };
        }
        self.state.clone()
    }

    /// Evaluates a candidate replacement adapter against the process's pinned
    /// producer and shared-texture invariants, advancing to the next generation
    /// on compatibility or latching [`DeviceLifecycleState::RestartRequired`].
    pub fn evaluate_replacement(
        &mut self,
        failed_generation: u64,
        loss_reason: &str,
        candidate: Result<AdapterDiagnostics, String>,
    ) -> DeviceRecoveryOutcome {
        match &self.state {
            DeviceLifecycleState::RestartRequired { generation, reason } => {
                return DeviceRecoveryOutcome::RestartRequired {
                    generation: *generation,
                    reason: reason.clone(),
                };
            }
            DeviceLifecycleState::Healthy { generation } if *generation > failed_generation => {
                return DeviceRecoveryOutcome::Recovered {
                    generation: *generation,
                    diagnostics: self.active_diagnostics.clone(),
                };
            }
            DeviceLifecycleState::Healthy { generation } if *generation == failed_generation => {
                self.state = DeviceLifecycleState::Lost {
                    generation: *generation,
                    reason: loss_reason.to_owned(),
                };
            }
            _ => {}
        }

        let active_loss_reason = match &self.state {
            DeviceLifecycleState::Lost { reason, .. } => reason.clone(),
            _ => loss_reason.to_owned(),
        };

        let diag = match candidate {
            Ok(diag) => diag,
            Err(open_err) => {
                let reason = format!(
                    "device recreation failed after loss ({active_loss_reason}): {open_err}"
                );
                self.state = DeviceLifecycleState::RestartRequired {
                    generation: failed_generation,
                    reason: reason.clone(),
                };
                return DeviceRecoveryOutcome::RestartRequired {
                    generation: failed_generation,
                    reason,
                };
            }
        };

        if !diag.producer_match.is_compatible() {
            let reason = format!(
                "replacement adapter \"{}\" producer_match={} is incompatible with pinned CEF producer after loss ({active_loss_reason})",
                diag.name, diag.producer_match,
            );
            self.state = DeviceLifecycleState::RestartRequired {
                generation: failed_generation,
                reason: reason.clone(),
            };
            return DeviceRecoveryOutcome::RestartRequired {
                generation: failed_generation,
                reason,
            };
        }

        #[cfg(windows)]
        if let Some(required_luid) = self.required_producer
            && diag.adapter_luid != Some(required_luid)
        {
            let expected = format!(
                "{} ({required_luid:#018x})",
                shared::format_luid_switch(required_luid)
            );
            let reason = format!(
                "replacement adapter \"{}\" LUID {} does not match pinned CEF adapter LUID {expected} after loss ({active_loss_reason})",
                diag.name,
                diag.luid_summary(),
            );
            self.state = DeviceLifecycleState::RestartRequired {
                generation: failed_generation,
                reason: reason.clone(),
            };
            return DeviceRecoveryOutcome::RestartRequired {
                generation: failed_generation,
                reason,
            };
        }

        if self.require_shared_import && !diag.can_import_shared {
            let reason = format!(
                "replacement adapter \"{}\" lost shared-texture import capability (can_import_shared=false, import_capable={}, producer_match={}) after loss ({active_loss_reason})",
                diag.name, diag.import_capable, diag.producer_match,
            );
            self.state = DeviceLifecycleState::RestartRequired {
                generation: failed_generation,
                reason: reason.clone(),
            };
            return DeviceRecoveryOutcome::RestartRequired {
                generation: failed_generation,
                reason,
            };
        }

        let next_generation = failed_generation.saturating_add(1);
        self.active_diagnostics = diag.clone();
        self.state = DeviceLifecycleState::Healthy {
            generation: next_generation,
        };
        DeviceRecoveryOutcome::Recovered {
            generation: next_generation,
            diagnostics: diag,
        }
    }
}

/// One generation of the process's wgpu device and device-wide pipelines.
///
/// Surfaces hold an `Arc<DeviceGeneration>` for the generation they were built
/// on. When a device is lost and coordinated recovery runs, dropping all
/// surfaces on the lost generation plus the slot's reference releases the old
/// `wgpu::Device` (`ID3D12Device`) before the replacement generation opens.
pub(crate) struct DeviceGeneration {
    pub(crate) generation: u64,
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) bind_layout: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    /// `device.limits().max_texture_dimension_2d`, read once — `limits()`
    /// clones the whole limits struct and sits on per-frame paths.
    pub(crate) max_texture_dim: u32,
    /// Sample and write through, for producers that already premultiplied.
    pipeline: wgpu::RenderPipeline,
    /// Premultiply in the shader, for producers that hand over straight alpha.
    pipeline_premultiplied: wgpu::RenderPipeline,
    // wgpu-core's surface.configure drains the whole device queue and errors if
    // another thread submits mid-drain, leaving the surface unconfigured → next
    // acquire fatally panics. Configure takes the write side, submit the read.
    pub(crate) submit_gate: parking_lot::RwLock<()>,
    pub(crate) can_import_shared: bool,
    pub(crate) diagnostics: AdapterDiagnostics,
    lost_reason: Arc<parking_lot::Mutex<Option<String>>>,
    disposed: Arc<AtomicBool>,
}

impl DeviceGeneration {
    pub(crate) fn configure_surface(
        &self,
        surface: &wgpu::Surface<'static>,
        config: &wgpu::SurfaceConfiguration,
    ) {
        let _guard = self.submit_gate.write();
        surface.configure(&self.device, config);
    }

    pub(crate) fn pipeline(&self, alpha: AlphaSource) -> &wgpu::RenderPipeline {
        match alpha {
            AlphaSource::Premultiplied => &self.pipeline,
            AlphaSource::Straight => &self.pipeline_premultiplied,
        }
    }

    pub(crate) fn lost_reason(&self) -> Option<String> {
        self.lost_reason.lock().clone()
    }

    pub(crate) fn record_lost(&self, reason: impl Into<String>) -> String {
        let mut slot = self.lost_reason.lock();
        if let Some(existing) = slot.as_ref() {
            return existing.clone();
        }
        let reason = reason.into();
        *slot = Some(reason.clone());
        reason
    }

    fn mark_disposed(&self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}

struct DeviceSlot {
    active: Option<Arc<DeviceGeneration>>,
    coordinator: DeviceRecoveryCoordinator,
}

/// The only handle to wgpu in the process. Held once; [`crate::Surface`]s
/// borrow it.
///
/// Everything here is device-wide: the active device generation is shared
/// across every surface and popup on every platform, and coordinated recovery
/// replaces the generation atomically after dependent surfaces are invalidated.
pub struct Surfaces {
    slot: parking_lot::RwLock<DeviceSlot>,
}

static INSTANCE: std::sync::OnceLock<Option<Surfaces>> = std::sync::OnceLock::new();

/// The process's one wgpu device, or `None` before the first
/// [`Surfaces::init`] and on a machine with no usable adapter.
pub fn surfaces() -> Option<&'static Surfaces> {
    INSTANCE.get()?.as_ref()
}

impl Surfaces {
    /// Open the process's one wgpu device, or hand back the one already open.
    ///
    /// The first call fixes the adapter for the process lifetime; `producer`
    /// on any later call is ignored. `None` when this system has no usable GPU
    /// path at all.
    ///
    /// On the platforms that call it first, this creates the process's GPU
    /// instance — which on X11 must happen before the mpv proxy repoints
    /// `DISPLAY` and before mpv init: NVIDIA's Vulkan ICD
    /// does a lazy, one-time global init on first `vkCreateInstance` that
    /// includes an internal `XOpenDisplay`. Running it here keeps the ICD's
    /// connection on the real server and completes before mpv's VO thread is
    /// spawned, winning the loader-scan race that otherwise crashes NVIDIA
    /// proprietary (two threads reading a half-populated ICD dispatch table).
    pub fn init(producer: Option<ProducerId>) -> Option<&'static Surfaces> {
        INSTANCE
            .get_or_init(|| match Self::open(producer) {
                Ok(surfaces) => Some(surfaces),
                Err(e) => {
                    tracing::info!("gpu_paint: device init failed: {e}");
                    None
                }
            })
            .as_ref()
    }

    fn open(producer: Option<shared::ProducerId>) -> Result<Self, SurfaceLost> {
        let generation = Self::open_generation(1, producer, true)?;
        let coordinator = DeviceRecoveryCoordinator::new(generation.diagnostics.clone(), producer);
        Ok(Self {
            slot: parking_lot::RwLock::new(DeviceSlot {
                active: Some(Arc::new(generation)),
                coordinator,
            }),
        })
    }

    fn open_generation(
        generation: u64,
        producer: Option<shared::ProducerId>,
        use_cached_enumeration: bool,
    ) -> Result<DeviceGeneration, SurfaceLost> {
        let (instance, adapter, producer_match) = if use_cached_enumeration {
            let (adapter, matched) = pick_adapter(producer).ok_or(Kind::NoAdapter)?;
            (enumerated().instance.clone(), adapter, matched)
        } else {
            let fresh = enumerate_fresh();
            let (adapter, matched) =
                pick_adapter_from(&fresh.adapters, producer).ok_or(Kind::NoAdapter)?;
            (fresh.instance, adapter, matched)
        };
        let device_matched = producer_match.is_compatible();
        let info = adapter.get_info();

        let opened = shared::open_device(&adapter)?;
        let shared::Opened {
            device,
            queue,
            import_capable,
        } = opened;

        let lost_reason = Arc::new(parking_lot::Mutex::new(None));
        let disposed = Arc::new(AtomicBool::new(false));
        {
            let lost_reason = Arc::clone(&lost_reason);
            let disposed = Arc::clone(&disposed);
            device.set_device_lost_callback(move |reason, msg| {
                if disposed.load(Ordering::SeqCst)
                    || (reason == wgpu::DeviceLostReason::Destroyed && msg.is_empty())
                {
                    tracing::debug!(
                        "gpu_paint [overlay-d3d12]: device generation {generation} disposed ({reason:?})"
                    );
                    return;
                }
                let detail = format!("{reason:?}: {msg}");
                tracing::error!(
                    "gpu_paint [overlay-d3d12]: DEVICE LOST (generation={generation}): {detail}"
                );
                {
                    let mut slot = lost_reason.lock();
                    if slot.is_none() {
                        *slot = Some(detail.clone());
                    }
                }
                if let Some(surfaces) = INSTANCE.get().and_then(Option::as_ref) {
                    surfaces.report_generation_lost_by_id(generation, detail);
                }
            });
        }
        device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
            tracing::error!("gpu_paint [overlay-d3d12]: wgpu error (generation={generation}): {e}");
        }));

        // Importing needs both halves: this device's import path must be live,
        // and it must be the same device CEF allocates on — an import from a
        // different GPU fails at bind time.
        let can_import_shared = import_capable && device_matched;

        let diagnostics = AdapterDiagnostics {
            name: info.name.clone(),
            backend: info.backend,
            device_type: info.device_type,
            vendor: info.vendor,
            device: info.device,
            #[cfg(windows)]
            adapter_luid: shared::adapter_luid(&adapter),
            producer_match,
            import_capable,
            can_import_shared,
        };

        #[cfg(windows)]
        tracing::info!(
            "gpu_paint [overlay-d3d12]: device generation {generation} created on \"{}\" (backend={:?}, type={:?}, vendor={:#06x}, device={:#06x}, luid={}), producer_match={producer_match}, can_import_shared={can_import_shared} (device_matched={device_matched})",
            diagnostics.name,
            diagnostics.backend,
            diagnostics.device_type,
            diagnostics.vendor,
            diagnostics.device,
            diagnostics.luid_summary(),
        );
        #[cfg(not(windows))]
        tracing::info!(
            "gpu_paint: device generation {generation} created on {} ({:?}), producer_match={producer_match}, can_import_shared={can_import_shared} (device_matched={device_matched})",
            diagnostics.name,
            diagnostics.backend,
        );

        let Pipelines {
            bind_layout,
            sampler,
            pipeline,
            pipeline_premultiplied,
        } = build_pipelines(&device);

        let max_texture_dim = device.limits().max_texture_dimension_2d;

        Ok(DeviceGeneration {
            generation,
            instance,
            adapter,
            device,
            queue,
            bind_layout,
            sampler,
            max_texture_dim,
            pipeline,
            pipeline_premultiplied,
            submit_gate: parking_lot::RwLock::new(()),
            can_import_shared,
            diagnostics,
            lost_reason,
            disposed,
        })
    }

    pub(crate) fn active_generation(&self) -> Result<Arc<DeviceGeneration>, SurfaceLost> {
        let active_gen = {
            let slot = self.slot.read();
            if let DeviceLifecycleState::RestartRequired { reason, .. } = slot.coordinator.state() {
                return Err(SurfaceLost::device_lost(reason.clone()));
            }
            slot.active
                .as_ref()
                .cloned()
                .ok_or_else(|| SurfaceLost::device_lost("overlay GPU device is not available"))?
        };
        if let Some(loss) = self.check_generation_lost(&active_gen) {
            return Err(loss);
        }
        Ok(active_gen)
    }

    pub(crate) fn report_generation_lost(
        &self,
        active_gen: &Arc<DeviceGeneration>,
        reason: impl Into<String>,
    ) -> String {
        let recorded = active_gen.record_lost(reason);
        self.report_generation_lost_by_id(active_gen.generation, recorded.clone());
        recorded
    }

    fn report_generation_lost_by_id(&self, generation: u64, reason: String) {
        let mut slot = self.slot.write();
        slot.coordinator.report_lost(generation, reason);
    }

    pub(crate) fn check_generation_lost(
        &self,
        active_gen: &Arc<DeviceGeneration>,
    ) -> Option<SurfaceLost> {
        if let Some(reason) = active_gen.lost_reason() {
            self.report_generation_lost_by_id(active_gen.generation, reason.clone());
            return Some(SurfaceLost::device_lost(reason));
        }
        {
            let slot = self.slot.read();
            match slot.coordinator.state() {
                DeviceLifecycleState::RestartRequired { reason, .. } => {
                    return Some(SurfaceLost::device_lost(reason.clone()));
                }
                DeviceLifecycleState::Lost { generation, reason }
                    if *generation == active_gen.generation =>
                {
                    return Some(SurfaceLost::device_lost(reason.clone()));
                }
                _ => {}
            }
            if slot.active.as_ref().map(|a| a.generation) != Some(active_gen.generation) {
                return Some(SurfaceLost::device_lost(format!(
                    "surface bound to superseded GPU device generation {}",
                    active_gen.generation
                )));
            }
        }
        if let Some(reason) = shared::poll_device_removed_reason(&active_gen.device) {
            let recorded = self.report_generation_lost(active_gen, reason);
            return Some(SurfaceLost::device_lost(recorded));
        }
        None
    }

    /// Current lifecycle state of the process-wide GPU device.
    pub fn lifecycle_state(&self) -> DeviceLifecycleState {
        self.slot.read().coordinator.state().clone()
    }

    /// The active generation number (starts at `1`).
    pub fn current_generation(&self) -> u64 {
        let slot = self.slot.read();
        match slot.coordinator.state() {
            DeviceLifecycleState::Healthy { generation }
            | DeviceLifecycleState::Lost { generation, .. }
            | DeviceLifecycleState::RestartRequired { generation, .. } => *generation,
        }
    }

    /// Explicitly marks the current GPU device generation as lost (used when a
    /// dependent subsystem or simulated test injects a device-loss condition).
    pub fn report_device_lost(&self, reason: impl Into<String>) -> DeviceLifecycleState {
        let reason = reason.into();
        let mut slot = self.slot.write();
        let generation = match slot.coordinator.state() {
            DeviceLifecycleState::Healthy { generation }
            | DeviceLifecycleState::Lost { generation, .. }
            | DeviceLifecycleState::RestartRequired { generation, .. } => *generation,
        };
        if let Some(active) = slot.active.as_ref()
            && active.generation == generation
        {
            active.record_lost(reason.clone());
        }
        slot.coordinator.report_lost(generation, reason)
    }

    /// Coordinates recreation of the process-wide GPU device after dependent
    /// surfaces on `failed_generation` have been invalidated.
    ///
    /// Releases the old generation's device reference first so D3D12 can tear
    /// down the removed singleton device before enumerating and opening its
    /// replacement. Verifies that the replacement device retains adapter LUID
    /// compatibility with CEF and shared-texture capability before allowing
    /// frames to resume.
    pub fn recover_generation(
        &self,
        failed_generation: u64,
        reason: &str,
    ) -> DeviceRecoveryOutcome {
        let required_producer = {
            let mut slot = self.slot.write();
            match slot.coordinator.state() {
                DeviceLifecycleState::RestartRequired { generation, reason } => {
                    return DeviceRecoveryOutcome::RestartRequired {
                        generation: *generation,
                        reason: reason.clone(),
                    };
                }
                DeviceLifecycleState::Healthy { generation }
                    if *generation > failed_generation && slot.active.is_some() =>
                {
                    return DeviceRecoveryOutcome::Recovered {
                        generation: *generation,
                        diagnostics: slot.coordinator.diagnostics().clone(),
                    };
                }
                _ => {}
            }
            slot.coordinator.report_lost(failed_generation, reason);
            if let Some(old) = slot.active.take() {
                old.mark_disposed();
                drop(old);
            }
            slot.coordinator.required_producer()
        };

        let next_generation = failed_generation.saturating_add(1);
        let opened = Self::open_generation(next_generation, required_producer, false);

        let mut slot = self.slot.write();
        match opened {
            Ok(new_gen) => {
                let candidate_diag = new_gen.diagnostics.clone();
                let outcome = slot.coordinator.evaluate_replacement(
                    failed_generation,
                    reason,
                    Ok(candidate_diag),
                );
                match &outcome {
                    DeviceRecoveryOutcome::Recovered {
                        generation,
                        diagnostics,
                    } => {
                        slot.active = Some(Arc::new(new_gen));
                        #[cfg(windows)]
                        tracing::info!(
                            "gpu_paint [overlay-d3d12]: recovered GPU device to generation {generation} on \"{}\" (luid={}, producer_match={}, can_import_shared={})",
                            diagnostics.name,
                            diagnostics.luid_summary(),
                            diagnostics.producer_match,
                            diagnostics.can_import_shared,
                        );
                        #[cfg(not(windows))]
                        tracing::info!(
                            "gpu_paint: recovered GPU device to generation {generation} on \"{}\" (producer_match={}, can_import_shared={})",
                            diagnostics.name,
                            diagnostics.producer_match,
                            diagnostics.can_import_shared,
                        );
                    }
                    DeviceRecoveryOutcome::RestartRequired { generation, reason } => {
                        new_gen.mark_disposed();
                        drop(new_gen);
                        tracing::error!(
                            "gpu_paint [overlay-d3d12] [cef-shared-import]: GPU device recovery rejected at generation {generation}; application restart required: {reason}"
                        );
                    }
                }
                outcome
            }
            Err(e) => {
                let outcome = slot.coordinator.evaluate_replacement(
                    failed_generation,
                    reason,
                    Err(e.to_string()),
                );
                if let DeviceRecoveryOutcome::RestartRequired {
                    generation,
                    ref reason,
                } = outcome
                {
                    tracing::error!(
                        "gpu_paint [overlay-d3d12]: GPU device recreation failed at generation {generation}; application restart required: {reason}"
                    );
                }
                outcome
            }
        }
    }

    /// Whether *this device* can import CEF's shared buffers.
    ///
    /// The consumer half only. Whether CEF can *produce* them is a separate
    /// question, answered by `jfn_linux_util::dmabuf_probe`; callers AND the
    /// two to get the app-level answer CEF needs before any browser exists.
    pub fn can_import_shared(&self) -> bool {
        let slot = self.slot.read();
        slot.active.as_ref().map_or_else(
            || slot.coordinator.diagnostics().can_import_shared,
            |g| g.can_import_shared,
        )
    }

    /// Diagnostic snapshot of the selected adapter and its producer alignment.
    pub fn diagnostics(&self) -> AdapterDiagnostics {
        let slot = self.slot.read();
        slot.active.as_ref().map_or_else(
            || slot.coordinator.diagnostics().clone(),
            |g| g.diagnostics.clone(),
        )
    }

    /// Bind a swapchain to one window. `size` seeds the swapchain extent; the
    /// surface takes its frame kind from the first frame presented to it.
    pub fn new_surface(
        &self,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Surface<'_>, SurfaceLost> {
        Surface::new(self, target, size)
    }

    /// Bind a swapchain to one window that the caller draws into itself.
    ///
    /// Format, present mode and composite-alpha mode come from the same
    /// per-target policy [`Surface`] uses.
    pub fn new_swapchain(
        &self,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Swapchain<'_>, SurfaceLost> {
        Swapchain::new(self, target, size)
    }

    pub fn adapter(&self) -> Option<wgpu::Adapter> {
        self.slot.read().active.as_ref().map(|g| g.adapter.clone())
    }

    /// The LUID of the adapter this device was opened on, packed high:low.
    #[cfg(windows)]
    pub fn adapter_luid(&self) -> Option<ProducerId> {
        self.diagnostics().adapter_luid
    }

    pub fn device(&self) -> Option<wgpu::Device> {
        self.slot.read().active.as_ref().map(|g| g.device.clone())
    }

    pub fn queue(&self) -> Option<wgpu::Queue> {
        self.slot.read().active.as_ref().map(|g| g.queue.clone())
    }
}

/// Whether this system has any adapter worth opening a device on.
///
/// For callers that must fail early on a machine with no GPU but cannot yet
/// answer *which* adapter to open — that needs a frame from the producer,
/// which needs a browser. Opens no device and no surface, and warms the
/// enumeration so the frame that does open the device pays nothing for it.
pub fn any_adapter() -> bool {
    !enumerated().adapters.is_empty()
}

/// The device-wide draw state every surface shares.
struct Pipelines {
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    pipeline_premultiplied: wgpu::RenderPipeline,
}

fn build_pipelines(device: &wgpu::Device) -> Pipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("jfn_gpu_paint overlay"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/overlay.wgsl").into()),
    });

    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("jfn_gpu_paint bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("jfn_gpu_paint pl"),
        bind_group_layouts: &[Some(&bind_layout)],
        immediate_size: 0,
    });

    let pipeline = build_pipeline(device, &pipeline_layout, &shader, "fs_main");
    let pipeline_premultiplied =
        build_pipeline(device, &pipeline_layout, &shader, "fs_main_premultiplied");

    // Nearest, no anisotropy — 1:1 sampling, never stretch.
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("jfn_gpu_paint sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });

    Pipelines {
        bind_layout,
        sampler,
        pipeline,
        pipeline_premultiplied,
    }
}

fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    fragment_entry: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("jfn_gpu_paint pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn build_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: native_backends(),
        flags: wgpu::InstanceFlags::empty(),
        backend_options: instance_options(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

/// The one backend that can present on this platform. Kept to a single choice
/// so the adapter we probe is always the adapter we open.
const fn native_backends() -> wgpu::Backends {
    #[cfg(target_os = "linux")]
    {
        wgpu::Backends::VULKAN
    }
    #[cfg(windows)]
    {
        wgpu::Backends::DX12
    }
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
}

/// On dx12, stop wgpu fetching and waiting on a frame-latency waitable object:
/// the present path runs on a thread that must not block.
fn instance_options() -> wgpu::BackendOptions {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut options = wgpu::BackendOptions::default();
    #[cfg(windows)]
    {
        options.dx12.latency_waitable_object = wgpu::Dx12UseFrameLatencyWaitableObject::None;
    }
    options
}

/// The instance and the adapters it found.
struct Enumerated {
    instance: wgpu::Instance,
    adapters: Vec<wgpu::Adapter>,
}

static ENUMERATED: std::sync::OnceLock<Enumerated> = std::sync::OnceLock::new();

fn enumerate_fresh() -> Enumerated {
    let instance = build_instance();
    let adapters = pollster::block_on(instance.enumerate_adapters(native_backends()))
        .into_iter()
        .filter(|a| {
            !matches!(
                a.get_info().device_type,
                wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
            )
        })
        .collect();
    Enumerated { instance, adapters }
}

/// Enumerate the usable adapters once for initial boot, and keep them.
///
/// Enumeration is not cheap — dx12 opens and closes a device per adapter to
/// read its capabilities, about a second per GPU — and on Windows the paint
/// device is opened from CEF's first frame, on the thread CEF paints from.
/// Enumerating there stalls painting for as long as it takes, which is longer
/// than the startup overlay is on screen. So it happens once, at whichever
/// call comes first (a platform's pre-flight [`any_adapter`], or [`Surfaces::init`]
/// itself), and every later caller reuses the result.
fn enumerated() -> &'static Enumerated {
    ENUMERATED.get_or_init(enumerate_fresh)
}

/// Pick an adapter from the cached boot enumeration.
fn pick_adapter(producer: Option<shared::ProducerId>) -> Option<(wgpu::Adapter, ProducerMatch)> {
    pick_adapter_from(&enumerated().adapters, producer)
}

/// Pick an adapter from `adapters`, and report whether it is the one CEF
/// produces on.
fn pick_adapter_from(
    adapters: &[wgpu::Adapter],
    producer: Option<shared::ProducerId>,
) -> Option<(wgpu::Adapter, ProducerMatch)> {
    if let Some(want) = producer
        && let Some(found) = adapters.iter().find(|a| shared::adapter_matches(a, want))
    {
        return Some((found.clone(), ProducerMatch::Matched(want)));
    }

    let chosen = adapters
        .iter()
        .max_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 3,
            wgpu::DeviceType::IntegratedGpu => 2,
            wgpu::DeviceType::VirtualGpu => 1,
            _ => 0,
        })?;
    // With no device to match against, the best adapter is as good as it gets
    // and counts as compatible; a device we asked for and missed does not.
    let matched = match producer {
        None => ProducerMatch::Unconstrained,
        Some(requested) => ProducerMatch::Mismatch { requested },
    };
    Some((chosen.clone(), matched))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_match_distinguishes_unconstrained_matched_and_mismatch() {
        let unconstrained = ProducerMatch::Unconstrained;
        assert!(unconstrained.is_compatible());
        assert_eq!(
            unconstrained.to_string(),
            "unconstrained (overlay selects adapter first)"
        );

        #[cfg(windows)]
        {
            let matched = ProducerMatch::Matched(0x0000_0001_0000_1234);
            assert!(matched.is_compatible());
            assert!(matched.to_string().contains("matched("));

            let mismatch = ProducerMatch::Mismatch {
                requested: 0x0000_0001_0000_9999,
            };
            assert!(!mismatch.is_compatible());
            assert!(mismatch.to_string().contains("mismatch(requested="));
        }
    }

    #[cfg(windows)]
    #[test]
    fn adapter_diagnostics_formats_luid_summary_when_present_or_absent() {
        let with_luid = AdapterDiagnostics {
            name: "Test GPU".to_owned(),
            backend: wgpu::Backend::Dx12,
            device_type: wgpu::DeviceType::DiscreteGpu,
            vendor: 0x10de,
            device: 0x2684,
            adapter_luid: Some((1_i64 << 32) | 0x1234),
            producer_match: ProducerMatch::Unconstrained,
            import_capable: true,
            can_import_shared: true,
        };
        assert_eq!(with_luid.luid_summary(), "1,4660 (0x0000000100001234)");

        let without_luid = AdapterDiagnostics {
            adapter_luid: None,
            ..with_luid
        };
        assert_eq!(without_luid.luid_summary(), "unavailable");
    }

    #[cfg(windows)]
    fn sample_windows_diagnostics(
        luid: Option<ProducerId>,
        producer_match: ProducerMatch,
        can_import_shared: bool,
    ) -> AdapterDiagnostics {
        AdapterDiagnostics {
            name: "Test D3D12 Adapter".to_owned(),
            backend: wgpu::Backend::Dx12,
            device_type: wgpu::DeviceType::DiscreteGpu,
            vendor: 0x10de,
            device: 0x2684,
            adapter_luid: luid,
            producer_match,
            import_capable: can_import_shared,
            can_import_shared,
        }
    }

    #[cfg(windows)]
    #[test]
    fn device_recovery_coordinator_recovers_when_replacement_preserves_pinned_luid() {
        let pinned_luid = (1_i64 << 32) | 0x4321;
        let initial =
            sample_windows_diagnostics(Some(pinned_luid), ProducerMatch::Unconstrained, true);
        let mut coord = DeviceRecoveryCoordinator::new(initial, None);
        assert_eq!(
            coord.state(),
            &DeviceLifecycleState::Healthy { generation: 1 }
        );
        assert_eq!(coord.required_producer(), Some(pinned_luid));
        assert!(coord.require_shared_import());

        let lost_state = coord.report_lost(1, "DXGI_ERROR_DEVICE_REMOVED");
        assert_eq!(
            lost_state,
            DeviceLifecycleState::Lost {
                generation: 1,
                reason: "DXGI_ERROR_DEVICE_REMOVED".to_owned(),
            }
        );

        let replacement = sample_windows_diagnostics(
            Some(pinned_luid),
            ProducerMatch::Matched(pinned_luid),
            true,
        );
        let outcome =
            coord.evaluate_replacement(1, "DXGI_ERROR_DEVICE_REMOVED", Ok(replacement.clone()));
        assert_eq!(
            outcome,
            DeviceRecoveryOutcome::Recovered {
                generation: 2,
                diagnostics: replacement.clone(),
            }
        );
        assert_eq!(
            coord.state(),
            &DeviceLifecycleState::Healthy { generation: 2 }
        );

        // A second surface on generation 1 asking to recover sees the already-recovered generation 2.
        let idempotent = coord.evaluate_replacement(
            1,
            "stale surface on generation 1",
            Err("should not be evaluated".to_owned()),
        );
        assert_eq!(
            idempotent,
            DeviceRecoveryOutcome::Recovered {
                generation: 2,
                diagnostics: replacement,
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn device_recovery_coordinator_requires_restart_on_adapter_luid_mismatch() {
        let pinned_luid = (1_i64 << 32) | 0x4321;
        let wrong_luid = (1_i64 << 32) | 0x9999;
        let initial =
            sample_windows_diagnostics(Some(pinned_luid), ProducerMatch::Unconstrained, true);
        let mut coord = DeviceRecoveryCoordinator::new(initial, None);
        coord.report_lost(1, "DXGI_ERROR_DEVICE_RESET");

        let mismatched = sample_windows_diagnostics(
            Some(wrong_luid),
            ProducerMatch::Mismatch {
                requested: pinned_luid,
            },
            false,
        );
        let outcome = coord.evaluate_replacement(1, "DXGI_ERROR_DEVICE_RESET", Ok(mismatched));
        assert!(matches!(
            outcome,
            DeviceRecoveryOutcome::RestartRequired { generation: 1, ref reason }
                if reason.contains("incompatible with pinned CEF producer")
        ));
        assert!(matches!(
            coord.state(),
            DeviceLifecycleState::RestartRequired { generation: 1, .. }
        ));

        // Even if ProducerMatch::Unconstrained is passed with a different LUID,
        // the pinned LUID check rejects it and requires restart.
        let initial2 =
            sample_windows_diagnostics(Some(pinned_luid), ProducerMatch::Unconstrained, true);
        let mut coord2 = DeviceRecoveryCoordinator::new(initial2, None);
        let wrong_luid_unconstrained =
            sample_windows_diagnostics(Some(wrong_luid), ProducerMatch::Unconstrained, true);
        let outcome2 = coord2.evaluate_replacement(
            1,
            "DXGI_ERROR_DEVICE_REMOVED",
            Ok(wrong_luid_unconstrained),
        );
        assert!(matches!(
            outcome2,
            DeviceRecoveryOutcome::RestartRequired { generation: 1, ref reason }
                if reason.contains("does not match pinned CEF adapter LUID")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn device_recovery_coordinator_requires_restart_when_shared_import_lost_or_open_fails() {
        let pinned_luid = (1_i64 << 32) | 0x4321;
        let initial =
            sample_windows_diagnostics(Some(pinned_luid), ProducerMatch::Unconstrained, true);
        let mut coord = DeviceRecoveryCoordinator::new(initial.clone(), None);
        let degraded = sample_windows_diagnostics(
            Some(pinned_luid),
            ProducerMatch::Matched(pinned_luid),
            false,
        );
        let outcome = coord.evaluate_replacement(1, "TDR timeout", Ok(degraded));
        assert!(matches!(
            outcome,
            DeviceRecoveryOutcome::RestartRequired { generation: 1, ref reason }
                if reason.contains("lost shared-texture import capability")
        ));

        let mut coord_open_err = DeviceRecoveryCoordinator::new(initial, None);
        let err_outcome = coord_open_err.evaluate_replacement(
            1,
            "DXGI_ERROR_DEVICE_HUNG",
            Err("no usable adapter available".to_owned()),
        );
        assert!(matches!(
            err_outcome,
            DeviceRecoveryOutcome::RestartRequired { generation: 1, ref reason }
                if reason.contains("no usable adapter available")
        ));
    }
}
