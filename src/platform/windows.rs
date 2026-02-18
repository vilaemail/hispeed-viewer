/// Windows-specific platform utilities.

/// Check if a path is on a removable drive (USB flash drive).
/// For now, just checks if the path exists and is accessible.
pub fn is_removable_drive(path: &std::path::Path) -> bool {
    path.exists()
}

/// Open a folder in Windows Explorer.
pub fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
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

// --- GPU Usage Tracking via PDH Performance Counters ---

#[link(name = "pdh")]
extern "system" {
    fn PdhOpenQueryW(data_source: *const u16, user_data: usize, query: *mut isize) -> i32;
    fn PdhAddEnglishCounterW(
        query: isize,
        path: *const u16,
        user_data: usize,
        counter: *mut isize,
    ) -> i32;
    fn PdhCollectQueryData(query: isize) -> i32;
    fn PdhGetFormattedCounterArrayW(
        counter: isize,
        fmt: u32,
        buf_size: *mut u32,
        count: *mut u32,
        items: *mut PdhFmtCounterValueItem,
    ) -> i32;
    fn PdhCloseQuery(query: isize) -> i32;
}

const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_MORE_DATA: i32 = 0x800007D2_u32 as i32;

#[repr(C)]
struct PdhFmtCounterValue {
    status: u32,
    _pad: u32,
    double_value: f64,
}

#[repr(C)]
struct PdhFmtCounterValueItem {
    _name: *const u16,
    value: PdhFmtCounterValue,
}

/// Tracks per-process GPU engine utilization via Windows PDH performance counters.
/// Available on Windows 10 1709+. Falls back to inactive (returns None) on older systems.
pub struct GpuUsageTracker {
    query: isize,
    counter: isize,
    active: bool,
}

impl GpuUsageTracker {
    pub fn new(pid: u32) -> Self {
        let mut query: isize = 0;
        let mut counter: isize = 0;
        let mut active = false;

        let path = format!(
            "\\GPU Engine(pid_{}_*)\\Utilization Percentage",
            pid
        );
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) == 0 {
                if PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) == 0 {
                    // First collect primes the rate counter
                    let _ = PdhCollectQueryData(query);
                    active = true;
                }
            }
        }

        Self { query, counter, active }
    }

    /// Collect a new sample and return the total GPU utilization percentage for this process.
    pub fn sample(&mut self) -> Option<f32> {
        if !self.active {
            return None;
        }

        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }

            // Query required buffer size
            let mut buf_size: u32 = 0;
            let mut count: u32 = 0;
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut count,
                std::ptr::null_mut(),
            );

            if status != PDH_MORE_DATA || buf_size == 0 {
                return Some(0.0);
            }

            // Allocate buffer and fetch counter values
            let mut buf = vec![0u8; buf_size as usize];
            let items = buf.as_mut_ptr() as *mut PdhFmtCounterValueItem;
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut count,
                items,
            );

            if status != 0 {
                return None;
            }

            // Sum utilization across all GPU engines for this process
            let mut total = 0.0f64;
            for i in 0..count as isize {
                let item = &*items.offset(i);
                if item.value.status == 0 {
                    total += item.value.double_value;
                }
            }

            Some(total as f32)
        }
    }
}

impl Drop for GpuUsageTracker {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}
