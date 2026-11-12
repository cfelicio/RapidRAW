use anyhow::{Result, anyhow};
use moxcms::{ColorProfile, DataColorSpace, Layout, RenderingIntent, TransformOptions};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tauri::Manager;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LutPrecision {
    Standard, // 33x33x33 Rgba8Unorm
    High,     // 65x65x65 Rgba16Float
}

impl LutPrecision {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            Some("high") => Self::High,
            _ => Self::Standard,
        }
    }

    pub fn size(&self) -> u32 {
        match self {
            Self::Standard => 33,
            Self::High => 65,
        }
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        match self {
            Self::Standard => wgpu::TextureFormat::Rgba8Unorm,
            Self::High => wgpu::TextureFormat::Rgba16Float,
        }
    }
}

pub struct DisplayLutData {
    pub size: u32,
    pub format: wgpu::TextureFormat,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IccProfileInfo {
    pub name: String,
    pub color_space: String,
    pub is_valid: bool,
    pub error: Option<String>,
}

/// Parse human-readable description from an ICC profile's raw bytes (supports ICC v2 'desc' and v4 'mluc').
pub fn extract_icc_description(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 132 {
        return None;
    }
    let tag_count = u32::from_be_bytes(bytes[128..132].try_into().ok()?) as usize;
    let mut offset = 132;

    for _ in 0..tag_count {
        if offset + 12 > bytes.len() {
            break;
        }
        let tag_sig = &bytes[offset..offset + 4];
        let tag_offset = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let tag_size = u32::from_be_bytes(bytes[offset + 8..offset + 12].try_into().ok()?) as usize;
        offset += 12;

        if tag_sig == b"desc" && tag_offset + tag_size <= bytes.len() {
            let data = &bytes[tag_offset..tag_offset + tag_size];
            if data.len() < 8 {
                continue;
            }
            let type_sig = &data[0..4];

            if type_sig == b"desc" {
                // ICC v2 textDescriptionType:
                // 0..4 type 'desc', 4..8 reserved 0, 8..12 ASCII count, 12.. ASCII bytes (null-terminated)
                if data.len() >= 12 {
                    let ascii_len = u32::from_be_bytes(data[8..12].try_into().ok()?) as usize;
                    if ascii_len > 0 && 12 + ascii_len <= data.len() {
                        let text_bytes = &data[12..12 + ascii_len];
                        let clean_bytes = text_bytes.split(|&b| b == 0).next().unwrap_or(text_bytes);
                        if let Ok(desc) = std::str::from_utf8(clean_bytes) {
                            let trimmed = desc.trim();
                            if !trimmed.is_empty() {
                                return Some(trimmed.to_string());
                            }
                        }
                    }
                }
            } else if type_sig == b"mluc" {
                // ICC v4 multiLocalizedUnicodeType:
                // 0..4 type 'mluc', 4..8 reserved 0, 8..12 count of records, 12..16 record size (12 bytes)
                // Records at 16..: 2 bytes lang, 2 bytes country, 4 bytes str length, 4 bytes str offset
                if data.len() >= 28 {
                    let str_len = u32::from_be_bytes(data[20..24].try_into().ok()?) as usize;
                    let str_offset = u32::from_be_bytes(data[24..28].try_into().ok()?) as usize;
                    if str_offset + str_len <= data.len() {
                        let utf16_bytes = &data[str_offset..str_offset + str_len];
                        let u16s: Vec<u16> = utf16_bytes
                            .chunks_exact(2)
                            .map(|c| u16::from_be_bytes([c[0], c[1]]))
                            .collect();
                        if let Ok(desc) = String::from_utf16(&u16s) {
                            let trimmed = desc.trim();
                            if !trimmed.is_empty() {
                                return Some(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Read and inspect an ICC profile at the given path, returning metadata for display in the UI.
pub fn inspect_icc_file(path_str: &str) -> IccProfileInfo {
    let path = Path::new(path_str);
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Display Profile")
        .to_string();

    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return IccProfileInfo {
                name: filename,
                color_space: "Unknown".to_string(),
                is_valid: false,
                error: Some(format!("Could not read file: {}", e)),
            };
        }
    };

    let profile = match ColorProfile::new_from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            return IccProfileInfo {
                name: filename,
                color_space: "Unknown".to_string(),
                is_valid: false,
                error: Some(format!("Invalid ICC profile: {}", e)),
            };
        }
    };

    if profile.color_space != DataColorSpace::Rgb {
        return IccProfileInfo {
            name: filename,
            color_space: format!("{:?}", profile.color_space),
            is_valid: false,
            error: Some("Profile is not an RGB display profile".to_string()),
        };
    }

    let parsed_name = extract_icc_description(&bytes).unwrap_or(filename);
    IccProfileInfo {
        name: parsed_name,
        color_space: "RGB".to_string(),
        is_valid: true,
        error: None,
    }
}

/// Evaluates a 3D LUT transforming from sRGB to the target display ICC profile.
pub fn generate_display_lut(
    icc_bytes: &[u8],
    precision: LutPrecision,
    intent_str: &str,
) -> Result<DisplayLutData> {
    let target = ColorProfile::new_from_slice(icc_bytes)
        .map_err(|e| anyhow!("Failed to parse display ICC profile: {e}"))?;

    if target.color_space != DataColorSpace::Rgb {
        return Err(anyhow!("Selected ICC profile is not in RGB color space"));
    }

    let rendering_intent = match intent_str {
        "perceptual" => RenderingIntent::Perceptual,
        "absolute" => RenderingIntent::AbsoluteColorimetric,
        _ => RenderingIntent::RelativeColorimetric,
    };

    let srgb = ColorProfile::new_srgb();
    let options = TransformOptions {
        rendering_intent,
        ..TransformOptions::default()
    };

    let transform = srgb
        .create_transform_f32(Layout::Rgb, &target, Layout::Rgb, options)
        .map_err(|e| anyhow!("Failed to create sRGB -> Display transform: {e}"))?;

    let size = precision.size();
    let n = size as usize;
    let total_pixels = n * n * n;

    // Generate input grid points in [0.0, 1.0]
    // 3D texture indexing: z (B), y (G), x (R)
    let mut input_rgb = vec![0.0f32; total_pixels * 3];
    let step = 1.0f32 / (n - 1) as f32;

    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let idx = ((b * n + g) * n + r) * 3;
                input_rgb[idx] = r as f32 * step;
                input_rgb[idx + 1] = g as f32 * step;
                input_rgb[idx + 2] = b as f32 * step;
            }
        }
    }

    // Evaluate transform in parallel
    let mut output_rgb = vec![0.0f32; total_pixels * 3];
    const CHUNK_SIZE: usize = 3 * 1024;
    output_rgb
        .par_chunks_mut(CHUNK_SIZE)
        .zip(input_rgb.par_chunks(CHUNK_SIZE))
        .for_each(|(dst, src)| {
            let _ = transform.transform(src, dst);
        });

    let (format, bytes) = match precision {
        LutPrecision::Standard => {
            let mut rgba8 = Vec::with_capacity(total_pixels * 4);
            for chunk in output_rgb.chunks_exact(3) {
                let r = (chunk[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                let g = (chunk[1].clamp(0.0, 1.0) * 255.0).round() as u8;
                let b = (chunk[2].clamp(0.0, 1.0) * 255.0).round() as u8;
                rgba8.extend_from_slice(&[r, g, b, 255]);
            }
            (precision.format(), rgba8)
        }
        LutPrecision::High => {
            let mut rgba16f = Vec::with_capacity(total_pixels * 8);
            for chunk in output_rgb.chunks_exact(3) {
                let r = half::f16::from_f32(chunk[0].clamp(0.0, 1.0));
                let g = half::f16::from_f32(chunk[1].clamp(0.0, 1.0));
                let b = half::f16::from_f32(chunk[2].clamp(0.0, 1.0));
                let a = half::f16::from_f32(1.0);
                rgba16f.extend_from_slice(&r.to_le_bytes());
                rgba16f.extend_from_slice(&g.to_le_bytes());
                rgba16f.extend_from_slice(&b.to_le_bytes());
                rgba16f.extend_from_slice(&a.to_le_bytes());
            }
            (precision.format(), rgba16f)
        }
    };

    Ok(DisplayLutData { size, format, bytes })
}

pub fn load_active_display_lut(
    settings: &crate::app_settings::AppSettings,
) -> Option<DisplayLutData> {
    if !settings.enable_display_color_management.unwrap_or(false) {
        return None;
    }
    let path_str = settings.display_icc_profile_path.as_deref()?;
    let bytes = fs::read(path_str).ok()?;
    let precision = LutPrecision::from_str_opt(settings.display_lut_precision.as_deref());
    let intent = settings
        .display_rendering_intent
        .as_deref()
        .unwrap_or("relative_bpc");
    generate_display_lut(&bytes, precision, intent).ok()
}

pub fn sync_display_color_management(app_handle: &tauri::AppHandle) {
    let Ok(settings) = crate::app_settings::load_settings(app_handle.clone()) else {
        return;
    };
    let state = app_handle.state::<crate::AppState>();
    let Some(ctx) = crate::gpu_processing::current_gpu_context(&state) else {
        return;
    };

    let lut_data = load_active_display_lut(&settings);

    // Update primary editor display
    if let Some(display) = ctx
        .display
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        display.update_display_lut(&ctx.device, &ctx.queue, lut_data.as_ref());
    }

    // Mirror to secondary detached viewer window
    if let Some(viewer) = ctx
        .viewer_display
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        viewer.update_display_lut(&ctx.device, &ctx.queue, lut_data.as_ref());
    }
}
