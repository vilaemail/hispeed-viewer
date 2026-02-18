use crate::settings::{CompressionAlgorithm, CompressionDevice};
use block_compression::{BC7Settings, CompressionVariant, GpuBlockCompressor};
use std::sync::{Arc, Mutex};

// ── GPU availability (probed once at startup) ────────────────────────

pub struct GpuAvailability {
    pub has_discrete: bool,
    pub has_integrated: bool,
    pub discrete_name: String,
    pub integrated_name: String,
}

impl GpuAvailability {
    pub fn probe() -> Self {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let high = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok();
        let low = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .ok();

        let high_info = high.as_ref().map(|a| a.get_info());
        let low_info = low.as_ref().map(|a| a.get_info());

        // If both adapters resolve to the same device, we only have one GPU
        let same_device = match (&high_info, &low_info) {
            (Some(h), Some(l)) => h.name == l.name && h.vendor == l.vendor && h.device == l.device,
            _ => false,
        };

        let (has_discrete, discrete_name) = match &high_info {
            Some(info) => (true, info.name.clone()),
            None => (false, String::new()),
        };

        let (has_integrated, integrated_name) = if same_device {
            // Only one GPU present — classify it based on device type
            match &high_info {
                Some(info)
                    if info.device_type == wgpu::DeviceType::IntegratedGpu =>
                {
                    (true, info.name.clone())
                }
                _ => (false, String::new()),
            }
        } else {
            match &low_info {
                Some(info) => (true, info.name.clone()),
                None => (false, String::new()),
            }
        };

        // If same device and it's discrete, don't also report as integrated
        let has_discrete = if same_device {
            match &high_info {
                Some(info) if info.device_type == wgpu::DeviceType::IntegratedGpu => false,
                Some(_) => true,
                None => false,
            }
        } else {
            has_discrete
        };

        log::info!(
            "GPU probe: discrete={} ({:?}), integrated={} ({:?})",
            has_discrete,
            discrete_name,
            has_integrated,
            integrated_name,
        );

        Self {
            has_discrete,
            has_integrated,
            discrete_name,
            integrated_name,
        }
    }

    pub fn is_available(&self, device: CompressionDevice) -> bool {
        match device {
            CompressionDevice::Cpu => true,
            CompressionDevice::Igpu => self.has_integrated,
            CompressionDevice::Gpu => self.has_discrete,
        }
    }

    /// Return the best available fallback: Gpu → Igpu → Cpu.
    pub fn best_fallback(&self, preferred: CompressionDevice) -> CompressionDevice {
        match preferred {
            CompressionDevice::Gpu => {
                if self.has_discrete {
                    CompressionDevice::Gpu
                } else if self.has_integrated {
                    CompressionDevice::Igpu
                } else {
                    CompressionDevice::Cpu
                }
            }
            CompressionDevice::Igpu => {
                if self.has_integrated {
                    CompressionDevice::Igpu
                } else {
                    CompressionDevice::Cpu
                }
            }
            CompressionDevice::Cpu => CompressionDevice::Cpu,
        }
    }
}

// ── GPU compressor ───────────────────────────────────────────────────

pub struct GpuCompressor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compressor: GpuBlockCompressor,
    power_preference: wgpu::PowerPreference,
    // Reusable GPU resources (same dimensions across all frames in a video)
    cached_dims: Option<(u32, u32)>,
    input_texture: Option<wgpu::Texture>,
    output_buffer: Option<wgpu::Buffer>,
    staging_buffer: Option<wgpu::Buffer>,
}

impl GpuCompressor {
    pub fn new(power_preference: wgpu::PowerPreference) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference,
            ..Default::default()
        }))
        .ok()?;

        let info = adapter.get_info();
        log::info!(
            "GPU compressor: using adapter '{}' ({:?})",
            info.name,
            info.device_type
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("bc-compressor"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            },
        ))
        .ok()?;

        let compressor = GpuBlockCompressor::new(device.clone(), queue.clone());

        Some(Self {
            device,
            queue,
            compressor,
            power_preference,
            cached_dims: None,
            input_texture: None,
            output_buffer: None,
            staging_buffer: None,
        })
    }

    pub fn power_preference(&self) -> wgpu::PowerPreference {
        self.power_preference
    }

    /// Compress a single RGBA frame to BC1 or BC7.
    /// Returns the raw compressed block data (no header — caller prepends it).
    pub fn compress_frame(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        algo: CompressionAlgorithm,
        level: u32,
    ) -> Result<Vec<u8>, String> {
        let variant = match algo {
            CompressionAlgorithm::Bc1 => CompressionVariant::BC1,
            CompressionAlgorithm::Bc7 => {
                let settings = match level {
                    0 => BC7Settings::opaque_ultra_fast(),
                    1 => BC7Settings::opaque_very_fast(),
                    2 => BC7Settings::opaque_fast(),
                    3 => BC7Settings::opaque_basic(),
                    4 => BC7Settings::opaque_slow(),
                    _ => BC7Settings::opaque_very_fast(),
                };
                CompressionVariant::BC7(settings)
            }
            _ => return Err("GPU compression only supports BC1/BC7".into()),
        };

        // Pad to block alignment (multiples of 4)
        let padded_w = (width + 3) & !3;
        let padded_h = (height + 3) & !3;
        let padded = crate::video::compression::pad_to_block_alignment(
            rgba, width, height, padded_w, padded_h,
        );

        let output_size = variant.blocks_byte_size(padded_w, padded_h);

        // (Re)create input texture if dimensions changed
        if self.cached_dims != Some((padded_w, padded_h)) {
            self.input_texture = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("bc-input"),
                size: wgpu::Extent3d {
                    width: padded_w,
                    height: padded_h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.cached_dims = Some((padded_w, padded_h));
            // Force buffer recreation since dims changed
            self.output_buffer = None;
            self.staging_buffer = None;
        }

        // (Re)create output/staging buffers if size changed (dims or algorithm switch)
        let need_new_buffers = self.output_buffer.as_ref()
            .map_or(true, |b| b.size() as usize != output_size);
        if need_new_buffers {
            self.output_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bc-output"),
                size: output_size as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }));

            self.staging_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bc-staging"),
                size: output_size as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }

        let texture = self.input_texture.as_ref().unwrap();
        let output_buffer = self.output_buffer.as_ref().unwrap();
        let staging_buffer = self.staging_buffer.as_ref().unwrap();

        // Upload RGBA data to texture
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_w * 4),
                rows_per_image: Some(padded_h),
            },
            wgpu::Extent3d {
                width: padded_w,
                height: padded_h,
                depth_or_array_layers: 1,
            },
        );

        // Set up compression task
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.compressor.add_compression_task(
            variant,
            &view,
            padded_w,
            padded_h,
            output_buffer,
            None,
            None,
        );

        // Run compute pass
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bc-encoder"),
            });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("bc-compress"),
                timestamp_writes: None,
            });
            self.compressor.compress(&mut pass);
        }

        // Copy output → staging
        encoder.copy_buffer_to_buffer(output_buffer, 0, staging_buffer, 0, output_size as u64);
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map staging buffer and read results
        let slice = staging_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .map_err(|e| format!("GPU poll failed: {e}"))?;

        rx.recv()
            .map_err(|e| format!("GPU map channel error: {e}"))?
            .map_err(|e| format!("GPU buffer map failed: {e}"))?;

        let data = slice.get_mapped_range().to_vec();
        staging_buffer.unmap();

        Ok(data)
    }
}

/// Create an `Arc<Mutex<GpuCompressor>>` for a given compression device setting.
pub fn create_gpu_compressor(device: CompressionDevice) -> Option<Arc<Mutex<GpuCompressor>>> {
    let pref = match device {
        CompressionDevice::Cpu => return None,
        CompressionDevice::Igpu => wgpu::PowerPreference::LowPower,
        CompressionDevice::Gpu => wgpu::PowerPreference::HighPerformance,
    };
    GpuCompressor::new(pref).map(|c| Arc::new(Mutex::new(c)))
}
