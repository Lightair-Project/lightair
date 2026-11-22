//! What Enhanced Broadcasting's configuration service is told about this computer
//! (docs/plans/twitch-dual-format.md §3.5): the processor's cores and name, the memory, the
//! system's name and version, and the graphics processor. Exactly that, gathered when a
//! destination with Enhanced Broadcasting goes live, sent only to its platform's service.

use lightair_live::enhanced_broadcasting::{Gpu, Machine};

/// This computer, as the service asks for it.
pub fn now() -> Machine {
    let logical = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let mut m = Machine {
        physical_cores: logical,
        logical_cores: logical,
        os_name: crate::ui::OS.to_string(),
        arm: cfg!(target_arch = "aarch64"),
        ..Default::default()
    };
    fill(&mut m);
    m
}

#[cfg(target_os = "macos")]
fn fill(m: &mut Machine) {
    let number = |name: &str| -> Option<u64> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let ok = unsafe {
            libc::sysctlbyname(name.as_ptr(), (&mut value as *mut u64).cast(), &mut size, std::ptr::null_mut(), 0)
        };
        // (some are 32-bit: the low bytes are filled, the rest stay zero)
        (ok == 0).then_some(value)
    };
    let text = |name: &str| -> Option<String> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut size = 0usize;
        unsafe { libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let ok = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
        (ok == 0).then(|| String::from_utf8_lossy(&buf).trim_end_matches('\0').trim().to_string()).filter(|s| !s.is_empty())
    };
    if let Some(n) = number("hw.physicalcpu").filter(|n| *n > 0) {
        m.physical_cores = n as u32;
    }
    if let Some(n) = number("hw.logicalcpu").filter(|n| *n > 0) {
        m.logical_cores = n as u32;
    }
    m.memory_total = number("hw.memsize").unwrap_or(0);
    let page = number("hw.pagesize").unwrap_or(16384);
    m.memory_free = number("vm.page_free_count").map_or(0, |p| (p & 0xFFFF_FFFF) * page);
    m.cpu_name = text("machdep.cpu.brand_string");
    m.os_version = text("kern.osproductversion").unwrap_or_default();
    // Apple's chips: the graphics are the chip's, sharing its memory (PCI vendor 0x106b)
    if m.arm {
        m.gpus.push(Gpu {
            model: m.cpu_name.clone().unwrap_or_else(|| "Apple silicon".into()),
            vendor_id: 0x106b,
            device_id: 0,
            dedicated_memory: 0,
            shared_memory: m.memory_total,
            driver: None,
        });
    }
}

#[cfg(windows)]
fn fill(m: &mut Machine) {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, IDXGIAdapter1, IDXGIFactory1,
        IDXGIFactory6,
    };
    // the graphics processor Windows ranks first for performance (the one libobs renders on)
    let gpu = || -> Option<Gpu> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
        let f6: IDXGIFactory6 = windows::core::Interface::cast(&factory).ok()?;
        for i in 0.. {
            let Ok(a) = (unsafe { f6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) })
            else {
                break;
            };
            let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let len = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
            return Some(Gpu {
                model: String::from_utf16_lossy(&d.Description[..len]),
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                dedicated_memory: d.DedicatedVideoMemory as u64,
                shared_memory: d.SharedSystemMemory as u64,
                driver: None,
            });
        }
        None
    };
    m.gpus.extend(gpu());
}

#[cfg(not(any(target_os = "macos", windows)))]
fn fill(_: &mut Machine) {}
