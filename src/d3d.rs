use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use tracing::{info, warn};

/// Dispositivo Direct3D 11 compartido entre WGC (captura), VideoProcessor (NV12)
/// y Media Foundation MFT (encoder H.264) para mantener un pipeline 100% Zero-Copy en GPU.
#[derive(Clone)]
pub struct SharedD3DDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub winrt_device: IDirect3DDevice,
    pub adapter_name: String,
    #[allow(dead_code)]
    pub has_video_support: bool,
}

impl SharedD3DDevice {
    pub fn new() -> Result<Self> {
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        // Intentamos habilitar VIDEO_SUPPORT (necesario para ID3D11VideoProcessor en GPU).
        // Si el driver/GPU antigua lo rechaza, hacemos fallback a solo BGRA_SUPPORT.
        let (device, context, has_video_support) =
            match create_device(&feature_levels, D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
                Ok((dev, ctx)) => (dev, ctx, true),
                Err(err) => {
                    warn!("D3D11 con VIDEO_SUPPORT falló ({err:#}); reintentando solo con BGRA_SUPPORT");
                    let (dev, ctx) = create_device(&feature_levels, D3D11_CREATE_DEVICE_BGRA_SUPPORT)
                        .context("No se pudo crear el dispositivo D3D11 por hardware")?;
                    (dev, ctx, false)
                }
            };

        // Activar protección multihilo en el contexto D3D11 para compartirlo de forma segura
        // entre WGC y el encoder de Media Foundation.
        let multithread: ID3D11Multithread = context
            .cast()
            .context("No se pudo obtener ID3D11Multithread desde el contexto D3D11")?;
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }

        // Envolver IDXGIDevice como IDirect3DDevice de WinRT para Windows Graphics Capture.
        let dxgi_device: IDXGIDevice = device
            .cast()
            .context("No se pudo obtener IDXGIDevice desde ID3D11Device")?;
        // Prioridad elevada en cola GPU (best-effort; evita inanición cuando el juego usa 99% GPU).
        unsafe {
            let _ = dxgi_device.SetGPUThreadPriority(5);
        }
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
            .context("CreateDirect3D11DeviceFromDXGIDevice falló")?;
        let winrt_device: IDirect3DDevice = inspectable
            .cast()
            .context("No se pudo convertir IInspectable a IDirect3DDevice")?;

        let adapter_desc = describe_adapter(&dxgi_device).unwrap_or_else(|| "Desconocida".to_string());
        info!(
            adapter = %adapter_desc,
            video_support = has_video_support,
            "Dispositivo D3D11 compartido inicializado"
        );

        Ok(Self {
            device,
            context,
            winrt_device,
            adapter_name: adapter_desc,
            has_video_support,
        })
    }
}

fn create_device(
    feature_levels: &[D3D_FEATURE_LEVEL],
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut achieved_level = D3D_FEATURE_LEVEL::default();

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            flags,
            Some(feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut achieved_level),
            Some(&mut context),
        )?;
    }

    let device = device.context("D3D11CreateDevice devolvió device = None")?;
    let context = context.context("D3D11CreateDevice devolvió context = None")?;
    Ok((device, context))
}

fn describe_adapter(dxgi_device: &IDXGIDevice) -> Option<String> {
    let adapter = unsafe { dxgi_device.GetAdapter().ok()? };
    let desc = unsafe { adapter.GetDesc().ok()? };
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    Some(String::from_utf16_lossy(&desc.Description[..len]))
}

/// Consulta el nombre del adaptador DXGI hardware principal (el mismo que elige
/// `D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, ...)`) sin crear dispositivo D3D11 ni usar VRAM.
pub fn primary_adapter_name() -> Option<String> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut idx = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(idx) } {
        idx += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
            continue;
        }
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..len]);
        if !name.trim().is_empty() {
            return Some(name);
        }
    }
    None
}
