//! D3D12 shared-handle import for CEF accelerated-paint frames.

use wgpu_hal::dx12;
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
};

use crate::SharedTexture;
use crate::error::SurfaceLost;
use crate::shared::{ImportFailed, Imported, Opened};

/// An adapter LUID, packed into one integer so it compares by value.
pub type ProducerId = i64;

/// The adapter this device opened on, packed high:low. Chromium is pinned to
/// it on the command line, so no frame ever has to name it.
pub(crate) fn adapter_luid(adapter: &wgpu::Adapter) -> Option<ProducerId> {
    unsafe { adapter.as_hal::<dx12::Api>() }
        .and_then(|hal| unsafe { hal.raw_adapter().GetDesc1() }.ok())
        .map(|desc| pack_luid(desc.AdapterLuid))
}

pub(crate) fn adapter_matches(adapter: &wgpu::Adapter, want: ProducerId) -> bool {
    let luid = adapter_luid(adapter);
    tracing::info!(
        "gpu_paint [overlay-d3d12]: candidate adapter LUID {luid:#018x?} (want {want:#018x})",
        luid = luid,
    );
    luid == Some(want)
}

const fn pack_luid(luid: LUID) -> ProducerId {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64)
}

/// Unpacks a packed [`ProducerId`] into `(HighPart, LowPart)` matching
/// `LUID` and Chromium's `--use-adapter-luid=<high>,<low>` switch format.
pub const fn unpack_luid(luid: ProducerId) -> (i32, u32) {
    ((luid >> 32) as i32, luid as u32)
}

/// Formats a packed [`ProducerId`] for Chromium's `--use-adapter-luid` switch.
pub fn format_luid_switch(luid: ProducerId) -> String {
    let (high, low) = unpack_luid(luid);
    format!("{high},{low}")
}

/// D3D12 can always open a shared handle; whether the *frame* opens is a
/// per-import question and whether this is CEF's adapter is the caller's.
pub(crate) fn open_device(adapter: &wgpu::Adapter) -> Result<Opened, SurfaceLost> {
    let (device, queue) = pollster::block_on(adapter.request_device(&device_descriptor(adapter)))?;
    Ok(Opened {
        device,
        queue,
        import_capable: true,
    })
}

fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("jfn_gpu_paint device"),
        required_features: wgpu::Features::empty(),
        // Adapter limits — the swapchain may be larger than the downlevel
        // 2048×2048 cap on modern displays.
        required_limits: adapter.limits(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }
}

/// Stateless: the handle is CEF's and is only valid inside the paint callback,
/// so nothing survives the frame.
pub(crate) struct Importer;

impl Importer {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn import(
        &mut self,
        device: &wgpu::Device,
        frame: &SharedTexture,
    ) -> Result<Imported, ImportFailed> {
        let handle = frame.handle();
        if handle.is_null() {
            return Err(ImportFailed("null shared handle"));
        }
        let hal_device =
            unsafe { device.as_hal::<dx12::Api>() }.ok_or(ImportFailed("not a D3D12 device"))?;

        // CEF owns this handle and reclaims it when the paint callback
        // returns — `OpenSharedHandle` does not take it, and closing it here
        // would pull the texture out of CEF's pool.
        let mut resource: Option<ID3D12Resource> = None;
        unsafe {
            hal_device
                .raw_device()
                .OpenSharedHandle(HANDLE(handle), &mut resource)
        }
        .map_err(|e| {
            let coded = frame.coded();
            tracing::warn!(
                "gpu_paint [cef-shared-import]: D3D12 OpenSharedHandle failed (handle={handle:?}, coded={}x{}, hr={:#010x}): {e:?}",
                coded.w,
                coded.h,
                e.code().0,
            );
            ImportFailed("OpenSharedHandle")
        })?;
        let resource = resource.ok_or(ImportFailed("OpenSharedHandle returned null"))?;

        // The resource states its own extent and format; CEF's coded size
        // describes the frame, not necessarily this allocation.
        let desc = unsafe { resource.GetDesc() };
        let format = wgpu_format(desc.Format).ok_or_else(|| {
            tracing::warn!(
                "gpu_paint [cef-shared-import]: unsupported shared texture format {:?} (width={}, height={})",
                desc.Format,
                desc.Width,
                desc.Height,
            );
            ImportFailed("unsupported shared format")
        })?;
        let width = u32::try_from(desc.Width).map_err(|_| ImportFailed("resource too wide"))?;
        let size = wgpu::Extent3d {
            width,
            height: desc.Height,
            depth_or_array_layers: 1,
        };

        let hal_texture = unsafe {
            dx12::Device::texture_from_raw(resource, format, wgpu::TextureDimension::D2, size, 1, 1)
        };
        let texture = unsafe {
            device.create_texture_from_hal::<dx12::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("jfn_gpu_paint shared"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };
        Ok(Imported {
            texture,
            acquire: None,
            reused: false,
        })
    }
}

fn wgpu_format(format: DXGI_FORMAT) -> Option<wgpu::TextureFormat> {
    // `DXGI_FORMAT` is a newtype, so these are values rather than patterns.
    if format == DXGI_FORMAT_B8G8R8A8_UNORM {
        Some(wgpu::TextureFormat::Bgra8Unorm)
    } else if format == DXGI_FORMAT_R8G8B8A8_UNORM {
        Some(wgpu::TextureFormat::Rgba8Unorm)
    } else {
        None
    }
}

/// No queue-family transfer on D3D12: the resource crosses devices in the
/// COMMON state and wgpu's own barriers take it from there.
pub(crate) fn acquire_barrier(
    _device: &wgpu::Device,
    _encoder: &mut wgpu::CommandEncoder,
    _image: u64,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;

    #[test]
    fn luid_roundtrips_high_and_low_parts_into_chromium_switch() {
        let luid = LUID {
            LowPart: 0x89ab_cdef,
            HighPart: -42,
        };
        let packed = pack_luid(luid);
        assert_eq!(unpack_luid(packed), (-42, 0x89ab_cdef));
        assert_eq!(format_luid_switch(packed), "-42,2309737967");
    }

    #[test]
    fn shared_texture_format_accepts_bgra8_and_rgba8_only() {
        assert_eq!(
            wgpu_format(DXGI_FORMAT_B8G8R8A8_UNORM),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
        assert_eq!(
            wgpu_format(DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(wgpu_format(DXGI_FORMAT_R16G16B16A16_FLOAT), None);
    }
}
