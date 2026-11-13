use moxcms::ColorProfile;

#[derive(Debug, Clone, Default)]
pub struct HeifMetadata {
    pub icc_profile: Option<Vec<u8>>,
    pub cicp: Option<CicpInfo>,
    pub exif: Option<Vec<u8>>,
    pub bit_depth: Option<u8>,
    pub orientation_degrees: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct CicpInfo {
    pub color_primaries: u16,
    pub transfer_characteristics: u16,
    pub matrix_coefficients: u16,
    pub full_range: bool,
}

impl CicpInfo {
    pub fn to_moxcms_profile(&self) -> Option<ColorProfile> {
        match self.color_primaries {
            12 | 11 => Some(ColorProfile::new_display_p3()), // Display P3 / DCI-P3
            9 => Some(ColorProfile::new_bt2020()),          // Rec.2020
            1 => Some(ColorProfile::new_srgb()),            // BT.709 / sRGB
            _ => None,
        }
    }
}

pub fn is_isobmff_heif_or_avif(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    if &bytes[4..8] != b"ftyp" {
        return false;
    }
    let major_brand = &bytes[8..12];
    matches!(
        major_brand,
        b"heic"
            | b"heix"
            | b"hevc"
            | b"hevx"
            | b"heim"
            | b"heis"
            | b"mif1"
            | b"msf1"
            | b"avif"
            | b"avis"
            | b"miaf"
    )
}

pub fn parse_heif_metadata(bytes: &[u8]) -> HeifMetadata {
    let mut meta = HeifMetadata::default();
    scan_boxes(bytes, 0, bytes.len(), &mut meta);
    meta
}

fn scan_boxes(bytes: &[u8], start: usize, end: usize, meta: &mut HeifMetadata) {
    let mut cursor = start;
    while cursor + 8 <= end {
        let box_len_raw = u32::from_be_bytes(match bytes[cursor..cursor + 4].try_into() {
            Ok(b) => b,
            Err(_) => break,
        }) as usize;
        let box_type = &bytes[cursor + 4..cursor + 8];

        let (header_len, box_len) = if box_len_raw == 1 {
            // 64-bit length
            if cursor + 16 > end {
                break;
            }
            let len64 = u64::from_be_bytes(match bytes[cursor + 8..cursor + 16].try_into() {
                Ok(b) => b,
                Err(_) => break,
            }) as usize;
            (16, len64)
        } else if box_len_raw == 0 {
            // Box runs to end of file
            (8, end - cursor)
        } else {
            (8, box_len_raw)
        };

        if box_len < header_len || cursor + box_len > end {
            break;
        }

        let payload_start = cursor + header_len;
        let payload_end = cursor + box_len;
        let payload = &bytes[payload_start..payload_end];

        match box_type {
            b"meta" => {
                // FullBox: 4 bytes version & flags
                if payload.len() > 4 {
                    scan_boxes(bytes, payload_start + 4, payload_end, meta);
                }
            }
            b"iprp" | b"ipco" => {
                scan_boxes(bytes, payload_start, payload_end, meta);
            }
            b"colr" => {
                if payload.len() >= 4 {
                    let colr_type = &payload[..4];
                    if colr_type == b"nclx" && payload.len() >= 11 {
                        let primaries = u16::from_be_bytes([payload[4], payload[5]]);
                        let transfer = u16::from_be_bytes([payload[6], payload[7]]);
                        let matrix = u16::from_be_bytes([payload[8], payload[9]]);
                        let full_range = (payload[10] & 0x80) != 0;
                        meta.cicp = Some(CicpInfo {
                            color_primaries: primaries,
                            transfer_characteristics: transfer,
                            matrix_coefficients: matrix,
                            full_range,
                        });
                    } else if (colr_type == b"prof" || colr_type == b"rICC") && payload.len() > 4 {
                        meta.icc_profile = Some(payload[4..].to_vec());
                    }
                }
            }
            b"pixi" => {
                // FullBox: 4 bytes (version+flags), 1 byte num_channels, then bits per channel
                if payload.len() >= 6 {
                    let num_channels = payload[4] as usize;
                    if payload.len() >= 5 + num_channels && num_channels > 0 {
                        meta.bit_depth = Some(payload[5]);
                    }
                }
            }
            b"irot" => {
                if !payload.is_empty() {
                    let angle = payload[0] & 0x03;
                    meta.orientation_degrees = match angle {
                        1 => Some(90),
                        2 => Some(180),
                        3 => Some(270),
                        _ => Some(0),
                    };
                }
            }
            _ => {
                // Check if this box contains Exif data directly or scanning sub-boxes
                if box_type == b"Exif" || box_type == b"exif" {
                    meta.exif = extract_exif_payload(payload);
                }
            }
        }

        cursor += box_len;
    }

    // If exif not found yet, do a fast pattern scan for the standard TIFF header (II*\0 or MM\0*)
    if meta.exif.is_none() {
        meta.exif = find_embedded_tiff_exif(bytes);
    }
}

fn extract_exif_payload(payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() < 8 {
        return None;
    }
    // Check if there is a 4-byte prefix indicating the offset to the TIFF header
    let offset_prefix = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if offset_prefix > 0 && offset_prefix + 4 < payload.len() {
        let sub = &payload[4 + offset_prefix..];
        if sub.starts_with(b"II*\0") || sub.starts_with(b"MM\0*") {
            return Some(sub.to_vec());
        }
    }
    if payload.starts_with(b"II*\0") || payload.starts_with(b"MM\0*") {
        return Some(payload.to_vec());
    }
    // Check for "Exif\0\0" header
    if payload.starts_with(b"Exif\0\0") && payload.len() > 6 {
        let sub = &payload[6..];
        if sub.starts_with(b"II*\0") || sub.starts_with(b"MM\0*") {
            return Some(sub.to_vec());
        }
    }
    None
}

fn find_embedded_tiff_exif(bytes: &[u8]) -> Option<Vec<u8>> {
    // Fast search for the Exif TIFF header within the first 64KB
    let search_limit = bytes.len().min(65536);
    let slice = &bytes[..search_limit];

    for i in 0..slice.len().saturating_sub(12) {
        if &slice[i..i + 4] == b"Exif" && &slice[i + 4..i + 6] == b"\0\0" {
            let candidate = &slice[i + 6..];
            if candidate.starts_with(b"II*\0") || candidate.starts_with(b"MM\0*") {
                return Some(candidate.to_vec());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isobmff_detection() {
        let mut heic_header = vec![0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c'];
        heic_header.extend_from_slice(&[0; 12]);
        assert!(is_isobmff_heif_or_avif(&heic_header));

        let mut avif_header = vec![0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f'];
        avif_header.extend_from_slice(&[0; 12]);
        assert!(is_isobmff_heif_or_avif(&avif_header));

        let png_header = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(!is_isobmff_heif_or_avif(&png_header));
    }

    #[test]
    fn test_parse_heif_boxes() {
        // Construct synthetic ISOBMFF container with colr(nclx) and pixi(10-bit)
        let mut data = Vec::new();

        // 1. ftyp box
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x14, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c', 0, 0, 0, 0, b'm', b'i', b'f', b'1']);

        // 2. colr nclx box (primaries=12 Display P3, transfer=13 sRGB-like, matrix=6, full=1)
        let colr_payload = [
            b'n', b'c', b'l', b'x',
            0x00, 12, // primaries = 12 (Display P3)
            0x00, 13, // transfer = 13 (IEC 61966-2-1 / sRGB)
            0x00, 6,  // matrix = 6
            0x80,     // full range
        ];
        let colr_len = (8 + colr_payload.len()) as u32;
        data.extend_from_slice(&colr_len.to_be_bytes());
        data.extend_from_slice(b"colr");
        data.extend_from_slice(&colr_payload);

        // 3. pixi box (10-bit, 3 channels)
        let pixi_payload = [
            0x00, 0x00, 0x00, 0x00, // version/flags
            3,                      // 3 channels
            10, 10, 10,             // 10-bit each
        ];
        let pixi_len = (8 + pixi_payload.len()) as u32;
        data.extend_from_slice(&pixi_len.to_be_bytes());
        data.extend_from_slice(b"pixi");
        data.extend_from_slice(&pixi_payload);

        // 4. irot box (angle = 1 -> 90 degrees)
        let irot_len = 9u32;
        data.extend_from_slice(&irot_len.to_be_bytes());
        data.extend_from_slice(b"irot");
        data.push(1);

        let meta = parse_heif_metadata(&data);
        assert!(meta.cicp.is_some());
        let cicp = meta.cicp.unwrap();
        assert_eq!(cicp.color_primaries, 12);
        assert_eq!(meta.bit_depth, Some(10));
        assert_eq!(meta.orientation_degrees, Some(90));
        assert!(cicp.to_moxcms_profile().is_some());
    }
}
