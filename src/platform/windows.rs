/// Windows-specific platform utilities.

/// Check if a path is on a removable drive (USB flash drive).
/// For now, just checks if the path exists and is accessible.
pub fn is_removable_drive(path: &std::path::Path) -> bool {
    path.exists()
}

/// Get per-process GPU usage percentage.
/// On Windows 10+ this could use D3DKMT APIs, but it is complex and
/// vendor-specific. Returns None until a platform implementation is added.
pub fn gpu_usage_percent() -> Option<f32> {
    None
}

/// Get GPU memory usage and budget in megabytes via DXGI 1.4 (Windows 10+).
/// Returns (current_usage_mb, budget_mb) for the primary adapter's local memory segment.
/// Budget reflects how much GPU memory the OS is willing to give our process,
/// accounting for other processes' usage.
pub fn gpu_memory_info() -> Option<(f32, f32)> {
    use ::windows::core::Interface;
    use ::windows::Win32::Graphics::Dxgi::*;
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).ok()?;
        let adapter3: IDXGIAdapter3 = adapter.cast().ok()?;
        let mut info = std::mem::zeroed::<DXGI_QUERY_VIDEO_MEMORY_INFO>();
        adapter3
            .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
            .ok()?;
        let used = info.CurrentUsage as f32 / (1024.0 * 1024.0);
        let budget = info.Budget as f32 / (1024.0 * 1024.0);
        Some((used, budget))
    }
}

/// Get available and total disk space in bytes for the drive containing `path`.
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use std::os::windows::ffi::OsStrExt;

    // Get the root of the path (e.g., "D:\")
    let root: std::path::PathBuf = if let Some(prefix) = path.components().next() {
        let mut p = std::path::PathBuf::from(prefix.as_os_str());
        p.push("\\");
        p
    } else {
        return None;
    };

    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let mut free_bytes_available = 0u64;
    let mut total_bytes = 0u64;
    let mut _total_free_bytes = 0u64;

    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_bytes_available),
            Some(&mut total_bytes),
            Some(&mut _total_free_bytes),
        )
        .ok()?;
    }

    Some((total_bytes - free_bytes_available, total_bytes))
}
