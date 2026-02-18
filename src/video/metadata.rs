use crate::settings::TimeFormat;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameSegment {
    pub count: u32,
    pub fps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoMetadata {
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub resolution: String,
    pub total_frames: u32,
    pub frames: Vec<FrameSegment>,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

impl VideoMetadata {
    /// Parse metadata from a JSON file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        Self::from_str(&content)
    }

    /// Parse metadata from a JSON string.
    pub fn from_str(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Failed to parse metadata JSON: {e}"))
    }

    /// Compute the timestamp (in seconds) for a given frame index.
    /// Frame indices are 0-based.
    pub fn frame_to_time(&self, frame_index: u32) -> f64 {
        let mut accumulated_frames: u32 = 0;
        let mut accumulated_time: f64 = 0.0;

        for segment in &self.frames {
            if segment.fps <= 0.0 || segment.count == 0 {
                accumulated_frames += segment.count;
                continue;
            }

            let segment_end = accumulated_frames + segment.count;
            if frame_index < segment_end {
                let frames_into_segment = frame_index - accumulated_frames;
                return accumulated_time + (frames_into_segment as f64) / segment.fps;
            }

            accumulated_time += (segment.count as f64) / segment.fps;
            accumulated_frames = segment_end;
        }

        // Past the last frame - return total duration
        accumulated_time
    }

    /// Compute the frame index closest to a given timestamp (in seconds).
    pub fn time_to_frame(&self, time: f64) -> u32 {
        let mut accumulated_frames: u32 = 0;
        let mut accumulated_time: f64 = 0.0;

        for segment in &self.frames {
            if segment.fps <= 0.0 || segment.count == 0 {
                accumulated_frames += segment.count;
                continue;
            }

            let segment_duration = (segment.count as f64) / segment.fps;
            if time < accumulated_time + segment_duration {
                let time_into_segment = time - accumulated_time;
                let frames_into_segment = (time_into_segment * segment.fps).round() as u32;
                return accumulated_frames + frames_into_segment.min(segment.count - 1);
            }

            accumulated_time += segment_duration;
            accumulated_frames += segment.count;
        }

        // Past duration - return last frame
        self.total_frames.saturating_sub(1)
    }

    /// Total duration in seconds computed from segments.
    pub fn total_duration(&self) -> f64 {
        self.frames
            .iter()
            .filter(|s| s.fps > 0.0)
            .map(|s| (s.count as f64) / s.fps)
            .sum()
    }

    /// Get the FPS for the segment containing the given frame index.
    pub fn fps_at_frame(&self, frame_index: u32) -> f64 {
        let mut accumulated: u32 = 0;
        for segment in &self.frames {
            if frame_index < accumulated + segment.count {
                return segment.fps;
            }
            accumulated += segment.count;
        }
        self.fps
    }

    /// Format time string according to the chosen time format.
    /// Milliseconds: `SS.mmm ms`  Microseconds: `SS.mmm ms UUU us`
    pub fn format_time(time_seconds: f64, fmt: TimeFormat) -> String {
        let abs_time = time_seconds.abs();
        let sign = if time_seconds < 0.0 { "-" } else { "" };

        let s = abs_time.floor() as u64;
        let sec = s % 60;
        let m = (s / 60) % 60;
        let h = s / 3600;

        let ms = ((abs_time.fract()) * 1000.0).floor() as u64;

        let time_part = if h > 0 {
            format!("{sign}{h}:{m:02}:{sec:02}.{ms:03}")
        } else if m > 0 {
            format!("{sign}{m}:{sec:02}.{ms:03}")
        } else {
            format!("{sign}{sec}.{ms:03}")
        };

        match fmt {
            TimeFormat::Milliseconds => {
                format!("{time_part} ms")
            }
            TimeFormat::Microseconds => {
                let total_us = ((abs_time.fract()) * 1_000_000.0).round() as u64;
                let us_part = total_us % 1000;
                format!("{time_part} ms {us_part:03} us")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata() -> VideoMetadata {
        VideoMetadata::from_str(
            r#"{
                "engine": "Custom SDK",
                "fps": 960,
                "width": 1920,
                "height": 1080,
                "resolution": "1920x1080",
                "totalFrames": 540,
                "frames": [
                    {"count": 30, "fps": 30},
                    {"count": 480, "fps": 960},
                    {"count": 30, "fps": 30}
                ],
                "file": "test.mp4",
                "size": 77717547,
                "sha256": "abc123"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn test_frame_to_time_first_segment() {
        let meta = sample_metadata();
        assert!((meta.frame_to_time(0) - 0.0).abs() < 1e-9);
        assert!((meta.frame_to_time(1) - 1.0 / 30.0).abs() < 1e-9);
        assert!((meta.frame_to_time(29) - 29.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn test_frame_to_time_second_segment() {
        let meta = sample_metadata();
        // Frame 30 = first frame of second segment
        let expected = 30.0 / 30.0; // 1 second from first segment
        assert!((meta.frame_to_time(30) - expected).abs() < 1e-9);

        // Frame 31 = 1 second + 1/960
        let expected = 1.0 + 1.0 / 960.0;
        assert!((meta.frame_to_time(31) - expected).abs() < 1e-9);
    }

    #[test]
    fn test_frame_to_time_third_segment() {
        let meta = sample_metadata();
        // Frame 510 = first frame of third segment
        let expected = 30.0 / 30.0 + 480.0 / 960.0; // 1.0 + 0.5 = 1.5
        assert!((meta.frame_to_time(510) - expected).abs() < 1e-9);
    }

    #[test]
    fn test_total_duration() {
        let meta = sample_metadata();
        let expected = 30.0 / 30.0 + 480.0 / 960.0 + 30.0 / 30.0; // 1.0 + 0.5 + 1.0 = 2.5
        assert!((meta.total_duration() - expected).abs() < 1e-9);
    }

    #[test]
    fn test_time_to_frame_roundtrip() {
        let meta = sample_metadata();
        for frame in [0, 15, 29, 30, 100, 300, 509, 510, 539] {
            let time = meta.frame_to_time(frame);
            let recovered = meta.time_to_frame(time);
            assert_eq!(recovered, frame, "Roundtrip failed for frame {frame}");
        }
    }

    #[test]
    fn test_format_time_ms() {
        let ms = TimeFormat::Milliseconds;
        assert_eq!(VideoMetadata::format_time(0.0, ms), "0.000 ms");
        assert_eq!(VideoMetadata::format_time(1.042, ms), "1.042 ms");
        assert_eq!(VideoMetadata::format_time(61.5, ms), "1:01.500 ms");
        assert_eq!(VideoMetadata::format_time(3661.0, ms), "1:01:01.000 ms");
        assert_eq!(VideoMetadata::format_time(-0.5, ms), "-0.500 ms");
    }

    #[test]
    fn test_format_time_us() {
        let us = TimeFormat::Microseconds;
        assert_eq!(VideoMetadata::format_time(0.0, us), "0.000 ms 000 us");
        assert_eq!(VideoMetadata::format_time(1.042, us), "1.042 ms 000 us");
        assert_eq!(VideoMetadata::format_time(1.042123, us), "1.042 ms 123 us");
        assert_eq!(VideoMetadata::format_time(-0.5, us), "-0.500 ms 000 us");
    }
}
