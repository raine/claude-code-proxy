use std::io::Cursor;

use base64::Engine as _;
use tiktoken_rs::o200k_base_singleton;

use super::translate::request::{GrokContentPart, GrokInputItem, GrokResponsesRequest};

const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const TOOL_OVERHEAD_TOKENS: u64 = 4;
const IMAGE_OVERHEAD_TOKENS: u64 = 256;
const IMAGE_TILE_EDGE: u64 = 512;
const IMAGE_TILE_TOKENS: u64 = 256;
const MAX_IMAGE_ESTIMATE_TOKENS: u64 = 65_536;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8; 2] = b"\xff\xd8";
/// Bound both decode work and dimensions independently of the compressed file size. The input
/// limit alone does not protect against a highly-compressible image with attacker-chosen geometry.
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_PNG_DECODER_BYTES: usize = 64 * 1024 * 1024;
/// JPEG validation decodes to one luminance byte per pixel, so the pixel cap is also a hard output
/// buffer cap. This avoids allocating RGB/CMYK output that is immediately discarded.
const MAX_JPEG_VALIDATION_BYTES: usize = MAX_IMAGE_PIXELS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Base64ImageError {
    Invalid,
    TooSmall,
    TooLarge,
}

pub fn count_tokens(request: &GrokResponsesRequest) -> u64 {
    let instructions = request
        .instructions
        .as_deref()
        .map(text_token_count)
        .unwrap_or(0);
    let input: u64 = request.input.iter().map(count_input_item).sum();
    let tools: u64 = request
        .tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|tool| {
            text_token_count(&serde_json::to_string(tool).unwrap_or_default())
                + TOOL_OVERHEAD_TOKENS
        })
        .sum();

    (instructions
        + input
        + tools
        + request.input.len() as u64 * MESSAGE_OVERHEAD_TOKENS
        + text_token_count(&request.model))
    .max(1)
}

fn count_input_item(item: &GrokInputItem) -> u64 {
    match item {
        GrokInputItem::Message { content, .. } => content
            .iter()
            .map(|part| match part {
                GrokContentPart::InputText { text } | GrokContentPart::OutputText { text } => {
                    text_token_count(text)
                }
                GrokContentPart::InputImage {
                    image_url,
                    estimated_tokens,
                } => estimated_tokens.unwrap_or_else(|| estimate_image_tokens(image_url)),
            })
            .sum(),
        GrokInputItem::FunctionCall {
            name, arguments, ..
        } => text_token_count(name) + text_token_count(arguments),
        GrokInputItem::FunctionCallOutput { output, .. } => text_token_count(output),
    }
}

fn estimate_image_tokens(image_url: &str) -> u64 {
    let Some((metadata, encoded)) = image_url.split_once(',') else {
        return IMAGE_OVERHEAD_TOKENS + text_token_count(image_url);
    };
    let Some(media_type) = metadata
        .strip_prefix("data:")
        .and_then(|metadata| metadata.strip_suffix(";base64"))
    else {
        return IMAGE_OVERHEAD_TOKENS + text_token_count(image_url);
    };
    validate_and_estimate_base64_image(encoded, usize::MAX, media_type)
        .unwrap_or(IMAGE_OVERHEAD_TOKENS)
}

/// Validate an embedded image in one streaming decode pass and cache the
/// estimate at translation time. Base64 input and decoded geometry are bounded before a mature
/// format decoder consumes the complete image. PNG is decoded row-by-row; JPEG is decoded in
/// strict mode to a bounded luminance buffer after its marker framing is checked.
pub(super) fn validate_and_estimate_base64_image(
    encoded: &str,
    max_decoded_bytes: usize,
    expected_media_type: &str,
) -> Result<u64, Base64ImageError> {
    if encoded.len() > max_base64_len(max_decoded_bytes) {
        return Err(Base64ImageError::TooLarge);
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Base64ImageError::Invalid)?;
    if decoded.len() > max_decoded_bytes {
        return Err(Base64ImageError::TooLarge);
    }

    let (width, height) = match expected_media_type {
        "image/png" if decoded.starts_with(PNG_SIGNATURE) => decode_png(&decoded)?,
        "image/jpeg" if decoded.starts_with(JPEG_SIGNATURE) => decode_jpeg(&decoded)?,
        _ => return Err(Base64ImageError::Invalid),
    };
    validate_image_dimensions(width, height)?;

    Ok(estimate_image_tokens_from_dimensions(width, height))
}

fn max_base64_len(max_decoded_bytes: usize) -> usize {
    max_decoded_bytes
        .saturating_add(2)
        .checked_div(3)
        .unwrap_or(0)
        .saturating_mul(4)
}

fn estimate_image_tokens_from_dimensions(width: u64, height: u64) -> u64 {
    let estimate = width
        .div_ceil(IMAGE_TILE_EDGE)
        .saturating_mul(height.div_ceil(IMAGE_TILE_EDGE))
        .saturating_mul(IMAGE_TILE_TOKENS);
    estimate.clamp(IMAGE_OVERHEAD_TOKENS, MAX_IMAGE_ESTIMATE_TOKENS)
}

fn validate_image_dimensions(width: u64, height: u64) -> Result<(), Base64ImageError> {
    let pixels = width.saturating_mul(height);
    if width < 8 || height < 8 || pixels < 512 {
        return Err(Base64ImageError::TooSmall);
    }
    if pixels > MAX_IMAGE_PIXELS {
        return Err(Base64ImageError::TooLarge);
    }
    Ok(())
}

fn decode_png(bytes: &[u8]) -> Result<(u64, u64), Base64ImageError> {
    let mut cursor = Cursor::new(bytes);
    let decoder = png::Decoder::new_with_limits(
        &mut cursor,
        png::Limits {
            bytes: MAX_PNG_DECODER_BYTES,
        },
    );
    let mut reader = decoder.read_info().map_err(map_png_error)?;
    let width = u64::from(reader.info().width);
    let height = u64::from(reader.info().height);
    validate_image_dimensions(width, height)?;
    if reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_PNG_DECODER_BYTES)
        .is_none()
    {
        return Err(Base64ImageError::TooLarge);
    }

    while reader.next_row().map_err(map_png_error)?.is_some() {}
    reader.finish().map_err(map_png_error)?;
    drop(reader);
    if cursor.position() != bytes.len() as u64 {
        return Err(Base64ImageError::Invalid);
    }
    Ok((width, height))
}

fn map_png_error(error: png::DecodingError) -> Base64ImageError {
    match error {
        png::DecodingError::LimitsExceeded => Base64ImageError::TooLarge,
        _ => Base64ImageError::Invalid,
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<(u64, u64), Base64ImageError> {
    // Some image decoders deliberately conceal premature EOI markers so browsers can display a
    // partial image. Reject empty scans and trailing data before asking the decoder to validate
    // the actual Huffman/arithmetic stream.
    validate_jpeg_framing(bytes)?;

    let options = zune_core::options::DecoderOptions::new_safe()
        .set_strict_mode(true)
        .set_max_width(usize::from(u16::MAX))
        .set_max_height(usize::from(u16::MAX))
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::Luma);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|_| Base64ImageError::Invalid)?;
    let info = decoder.info().ok_or(Base64ImageError::Invalid)?;
    let width = u64::from(info.width);
    let height = u64::from(info.height);
    validate_image_dimensions(width, height)?;

    let output_size = decoder
        .output_buffer_size()
        .filter(|size| *size <= MAX_JPEG_VALIDATION_BYTES)
        .ok_or(Base64ImageError::TooLarge)?;
    let mut output = vec![0; output_size];
    decoder
        .decode_into(&mut output)
        .map_err(|_| Base64ImageError::Invalid)?;
    Ok((width, height))
}

fn validate_jpeg_framing(bytes: &[u8]) -> Result<(), Base64ImageError> {
    if !bytes.starts_with(JPEG_SIGNATURE) {
        return Err(Base64ImageError::Invalid);
    }

    let mut offset = JPEG_SIGNATURE.len();
    let mut saw_scan = false;
    while offset < bytes.len() {
        if bytes[offset] != 0xff {
            return Err(Base64ImageError::Invalid);
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset).ok_or(Base64ImageError::Invalid)?;
        offset += 1;

        match marker {
            0xd9 => {
                return if saw_scan && offset == bytes.len() {
                    Ok(())
                } else {
                    Err(Base64ImageError::Invalid)
                };
            }
            0xd8 | 0x00 | 0xd0..=0xd7 => return Err(Base64ImageError::Invalid),
            // TEM is the only standalone marker that may occur outside entropy-coded data.
            0x01 => continue,
            _ => {}
        }

        let length_bytes = bytes
            .get(offset..offset.saturating_add(2))
            .ok_or(Base64ImageError::Invalid)?;
        let segment_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if segment_length < 2 {
            return Err(Base64ImageError::Invalid);
        }
        offset = offset
            .checked_add(segment_length)
            .filter(|end| *end <= bytes.len())
            .ok_or(Base64ImageError::Invalid)?;

        if marker != 0xda {
            continue;
        }

        saw_scan = true;
        let mut entropy_bytes = 0_usize;
        while offset < bytes.len() {
            if bytes[offset] != 0xff {
                entropy_bytes += 1;
                offset += 1;
                continue;
            }

            let marker_start = offset;
            while offset < bytes.len() && bytes[offset] == 0xff {
                offset += 1;
            }
            let next = *bytes.get(offset).ok_or(Base64ImageError::Invalid)?;
            match next {
                // Byte-stuffed 0xff is entropy data; restart markers remain inside the scan.
                0x00 => {
                    entropy_bytes += 1;
                    offset += 1;
                }
                0xd0..=0xd7 => offset += 1,
                _ => {
                    offset = marker_start;
                    break;
                }
            }
        }
        if entropy_bytes == 0 {
            return Err(Base64ImageError::Invalid);
        }
    }

    Err(Base64ImageError::Invalid)
}

fn text_token_count(text: &str) -> u64 {
    u64::try_from(o200k_base_singleton().count_with_special_tokens(text)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::grok::translate::request::translate_request;
    use serde_json::json;

    fn png_bytes(width: u32, height: u32, ancillary_bytes: usize) -> Vec<u8> {
        let pixel_count = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
        let mut image = Vec::new();
        let mut encoder = png::Encoder::new(&mut image, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        if ancillary_bytes > 0 {
            encoder
                .add_text_chunk("Comment".into(), "x".repeat(ancillary_bytes))
                .unwrap();
        }
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![0; pixel_count])
            .unwrap();
        image
    }

    fn png_base64(width: u32, height: u32) -> String {
        base64::engine::general_purpose::STANDARD.encode(png_bytes(width, height, 0))
    }

    fn jpeg_bytes(width: u16, height: u16, app_payload_bytes: usize) -> Vec<u8> {
        assert!(app_payload_bytes <= usize::from(u16::MAX) - 2);
        let mut image = Vec::new();
        jpeg_encoder::Encoder::new(&mut image, 90)
            .encode(
                &vec![0; usize::from(width) * usize::from(height)],
                width,
                height,
                jpeg_encoder::ColorType::Luma,
            )
            .unwrap();
        if app_payload_bytes > 0 {
            let mut app = Vec::with_capacity(app_payload_bytes + 4);
            app.extend_from_slice(&[0xff, 0xef]);
            app.extend_from_slice(&((app_payload_bytes + 2) as u16).to_be_bytes());
            app.resize(app.len() + app_payload_bytes, 0);
            image.splice(2..2, app);
        }
        image
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn append_raw_png_chunk(output: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(&(data.len() as u32).to_be_bytes());
        output.extend_from_slice(chunk_type);
        output.extend_from_slice(data);
        let mut crc_input = Vec::with_capacity(chunk_type.len() + data.len());
        crc_input.extend_from_slice(chunk_type);
        crc_input.extend_from_slice(data);
        output.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    fn png_with_empty_pixel_stream(width: u32, height: u32) -> Vec<u8> {
        png_with_empty_pixel_stream_and_color(width, height, 0)
    }

    fn png_with_empty_pixel_stream_and_color(width: u32, height: u32, color_type: u8) -> Vec<u8> {
        let mut image = PNG_SIGNATURE.to_vec();
        let mut header = Vec::with_capacity(13);
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, color_type, 0, 0, 0]);
        append_raw_png_chunk(&mut image, b"IHDR", &header);
        // This is a valid zlib stream that expands to zero bytes. The PNG container and every CRC
        // are valid, but it cannot contain the declared scanlines.
        append_raw_png_chunk(
            &mut image,
            b"IDAT",
            &[0x78, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        append_raw_png_chunk(&mut image, b"IEND", &[]);
        image
    }

    fn jpeg_with_truncated_entropy(width: u16, height: u16) -> Vec<u8> {
        let image = jpeg_bytes(width, height, 0);
        let mut offset = 2;
        while offset + 4 <= image.len() {
            assert_eq!(image[offset], 0xff);
            let marker = image[offset + 1];
            let length = usize::from(u16::from_be_bytes([image[offset + 2], image[offset + 3]]));
            if marker == 0xda {
                let scan_start = offset + 2 + length;
                let mut truncated = image[..scan_start].to_vec();
                truncated.extend_from_slice(&[0xff, 0xd9]);
                return truncated;
            }
            offset += 2 + length;
        }
        panic!("encoded JPEG lacks an SOS marker")
    }

    fn translated_request(value: serde_json::Value) -> GrokResponsesRequest {
        let request: MessagesRequest = serde_json::from_value(value).unwrap();
        translate_request(&request, "grok-4.5".into()).unwrap()
    }

    #[test]
    fn count_tokens_returns_a_positive_count() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert!(count_tokens(&request) > 0);
    }

    #[test]
    fn text_count_uses_o200k_for_cjk_code_and_punctuation() {
        let text = "修复工具调用：fn main() { println!(\"你好\"); }";
        let expected =
            u64::try_from(o200k_base_singleton().count_with_special_tokens(text)).unwrap();

        assert_eq!(text_token_count(text), expected);
        assert!(
            expected > 4,
            "CJK/code text must not collapse to word count"
        );
    }

    #[test]
    fn count_tokens_accepts_nested_system_and_ignores_cache_control() {
        let plain = translated_request(json!({
            "model": "grok-4.5",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "follow rules"}]},
                {"role": "user", "content": "hello"}
            ]
        }));
        let cached = translated_request(json!({
            "model": "grok-4.5",
            "messages": [
                {"role": "system", "content": [{
                    "type": "text",
                    "text": "follow rules",
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }]},
                {"role": "user", "content": "hello"}
            ]
        }));

        assert!(count_tokens(&plain) > 0);
        assert_eq!(count_tokens(&plain), count_tokens(&cached));
    }

    #[test]
    fn count_tokens_increases_for_more_input() {
        let short = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let long = translated_request(json!({
            "model": "grok-4.5",
            "system": "Follow all instructions carefully.",
            "messages": [{"role": "user", "content": "hello, please explain this request in detail"}],
            "tools": [{"name": "lookup", "description": "Look up a record", "input_schema": {"type": "object"}}]
        }));

        assert!(count_tokens(&long) > count_tokens(&short));
    }

    #[test]
    fn count_tokens_includes_hosted_tool_filters() {
        let plain = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "find docs"}],
            "tools": [{
                "type": "web_search_20260318",
                "name": "web_search",
                "allowed_callers": ["direct"]
            }]
        }));
        let filtered = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "find docs"}],
            "tools": [{
                "type": "web_search_20260318",
                "name": "web_search",
                "allowed_callers": ["direct"],
                "allowed_domains": ["docs.rs", "rust-lang.org"]
            }]
        }));

        assert!(count_tokens(&filtered) > count_tokens(&plain));
    }

    #[test]
    fn count_tokens_is_deterministic() {
        let request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "repeatable input"}]
        }));

        assert_eq!(count_tokens(&request), count_tokens(&request));
    }

    #[test]
    fn count_tokens_handles_images_without_counting_base64_bytes_as_text() {
        let small_image = base64::engine::general_purpose::STANDARD.encode(png_bytes(32, 32, 0));
        let larger_payload_image =
            base64::engine::general_purpose::STANDARD.encode(png_bytes(32, 32, 1024));
        let small = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": [{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":small_image}
            }]}]
        }));
        let larger_payload = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": [{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":larger_payload_image}
            }]}]
        }));

        assert!(count_tokens(&small) >= IMAGE_OVERHEAD_TOKENS);
        assert_eq!(count_tokens(&small), count_tokens(&larger_payload));
    }

    #[test]
    fn streaming_base64_validation_enforces_the_decoded_byte_boundary() {
        let image = png_bytes(32, 32, 0);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image);

        assert!(validate_and_estimate_base64_image(&encoded, image.len(), "image/png").is_ok());
        assert_eq!(
            validate_and_estimate_base64_image(&encoded, image.len() - 1, "image/png"),
            Err(Base64ImageError::TooLarge)
        );
    }

    #[test]
    fn streaming_base64_validation_reads_and_validates_the_entire_payload() {
        let mut invalid_tail =
            base64::engine::general_purpose::STANDARD.encode(png_bytes(32, 32, 16 * 1024));
        invalid_tail.pop();
        invalid_tail.push('%');

        assert_eq!(
            validate_and_estimate_base64_image(&invalid_tail, 20 * 1024, "image/png"),
            Err(Base64ImageError::Invalid)
        );

        let mut trailing_bytes = png_bytes(32, 32, 0);
        trailing_bytes.push(0);
        let trailing_bytes = base64::engine::general_purpose::STANDARD.encode(trailing_bytes);
        assert_eq!(
            validate_and_estimate_base64_image(&trailing_bytes, 20 * 1024, "image/png"),
            Err(Base64ImageError::Invalid)
        );

        let mut trailing_jpeg = jpeg_bytes(32, 32, 0);
        trailing_jpeg.push(0);
        let trailing_jpeg = base64::engine::general_purpose::STANDARD.encode(trailing_jpeg);
        assert_eq!(
            validate_and_estimate_base64_image(&trailing_jpeg, 20 * 1024, "image/jpeg"),
            Err(Base64ImageError::Invalid)
        );

        let mut trailing_base64 = png_base64(32, 32);
        trailing_base64.push('%');
        assert_eq!(
            validate_and_estimate_base64_image(&trailing_base64, 20 * 1024, "image/png"),
            Err(Base64ImageError::Invalid)
        );
    }

    #[test]
    fn streaming_image_estimate_reads_png_and_jpeg_dimensions() {
        let png = png_base64(1024, 512);
        assert_eq!(
            validate_and_estimate_base64_image(&png, 1024 * 1024, "image/png").unwrap(),
            2 * IMAGE_TILE_TOKENS
        );

        let jpeg = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes(1024, 512, 0));
        assert_eq!(
            validate_and_estimate_base64_image(&jpeg, 1024 * 1024, "image/jpeg").unwrap(),
            2 * IMAGE_TILE_TOKENS
        );
    }

    #[test]
    fn streaming_image_validation_rejects_dimensions_below_grok_minimum() {
        for image in [
            png_base64(1, 512),
            png_base64(512, 1),
            png_base64(7, 73),
            png_base64(8, 8),
            png_base64(8, 63),
        ] {
            assert_eq!(
                validate_and_estimate_base64_image(&image, 1024, "image/png"),
                Err(Base64ImageError::TooSmall)
            );
        }
        for image in [png_base64(8, 64), png_base64(16, 32), png_base64(512, 8)] {
            assert!(validate_and_estimate_base64_image(&image, 1024, "image/png").is_ok());
        }
    }

    #[test]
    fn image_dimensions_are_bounded_independently_of_compressed_size() {
        assert!(validate_image_dimensions(8192, 8192).is_ok());
        assert_eq!(
            validate_image_dimensions(8192, 8193),
            Err(Base64ImageError::TooLarge)
        );
        assert_eq!(
            validate_image_dimensions(u64::MAX, u64::MAX),
            Err(Base64ImageError::TooLarge)
        );

        // The geometry is below the pixel cap, but RGBA output would exceed the decoder budget.
        let oversized_output = base64::engine::general_purpose::STANDARD
            .encode(png_with_empty_pixel_stream_and_color(4096, 4097, 6));
        assert_eq!(
            validate_and_estimate_base64_image(&oversized_output, 1024 * 1024, "image/png"),
            Err(Base64ImageError::TooLarge)
        );
    }

    #[test]
    fn jpeg_dimensions_beyond_64_kib_are_still_validated() {
        let jpeg = jpeg_bytes(8, 8, usize::from(u16::MAX) - 2);
        assert!(jpeg.len() > 64 * 1024);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&jpeg);

        assert_eq!(
            validate_and_estimate_base64_image(&encoded, jpeg.len(), "image/jpeg"),
            Err(Base64ImageError::TooSmall)
        );
    }

    #[test]
    fn streaming_image_validation_rejects_fake_truncated_and_corrupt_images() {
        let fake_png = base64::engine::general_purpose::STANDARD.encode(b"hello");
        let fake_jpeg = base64::engine::general_purpose::STANDARD
            .encode([0xff, 0xd8, b'h', b'e', b'l', b'l', b'o']);
        let mut truncated_png = png_bytes(32, 32, 0);
        truncated_png.truncate(truncated_png.len() - 5);
        let mut truncated_jpeg = jpeg_bytes(32, 32, 0);
        truncated_jpeg.truncate(truncated_jpeg.len() - 1);
        let mut corrupt_png = png_bytes(32, 32, 0);
        corrupt_png[29] ^= 1;

        for (invalid, media_type) in [
            (fake_png, "image/png"),
            (fake_jpeg, "image/jpeg"),
            (
                base64::engine::general_purpose::STANDARD.encode(truncated_png),
                "image/png",
            ),
            (
                base64::engine::general_purpose::STANDARD.encode(truncated_jpeg),
                "image/jpeg",
            ),
            (
                base64::engine::general_purpose::STANDARD.encode(corrupt_png),
                "image/png",
            ),
        ] {
            assert_eq!(
                validate_and_estimate_base64_image(&invalid, 1024 * 1024, media_type),
                Err(Base64ImageError::Invalid)
            );
        }
    }

    #[test]
    fn complete_decoders_reject_invalid_idat_and_entropy_streams() {
        let invalid_png =
            base64::engine::general_purpose::STANDARD.encode(png_with_empty_pixel_stream(32, 32));
        let invalid_jpeg =
            base64::engine::general_purpose::STANDARD.encode(jpeg_with_truncated_entropy(32, 32));

        assert_eq!(
            validate_and_estimate_base64_image(&invalid_png, 1024 * 1024, "image/png"),
            Err(Base64ImageError::Invalid)
        );
        assert_eq!(
            validate_and_estimate_base64_image(&invalid_jpeg, 1024 * 1024, "image/jpeg"),
            Err(Base64ImageError::Invalid)
        );
    }

    #[test]
    fn streaming_image_validation_rejects_declared_media_type_mismatches() {
        let png = png_base64(32, 32);
        let jpeg = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes(32, 32, 0));

        assert_eq!(
            validate_and_estimate_base64_image(&png, 1024, "image/jpeg"),
            Err(Base64ImageError::Invalid)
        );
        assert_eq!(
            validate_and_estimate_base64_image(&jpeg, 1024, "image/png"),
            Err(Base64ImageError::Invalid)
        );
    }

    #[test]
    fn translated_base64_image_reuses_validation_time_estimate() {
        let mut request = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": [{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":png_base64(1024, 1024)}
            }]}]
        }));
        let before = count_tokens(&request);
        let image = request
            .input
            .iter_mut()
            .find_map(|item| match item {
                GrokInputItem::Message { content, .. } => {
                    content.iter_mut().find_map(|part| match part {
                        GrokContentPart::InputImage {
                            image_url,
                            estimated_tokens,
                        } => Some((image_url, estimated_tokens)),
                        _ => None,
                    })
                }
                _ => None,
            })
            .expect("translated image");
        assert!(image.1.is_some());
        *image.0 = "data:image/png;base64,this-is-deliberately-invalid".into();

        // If counting decoded the URL again this would fall back to a different estimate.
        assert_eq!(count_tokens(&request), before);
    }

    #[test]
    fn count_tokens_scales_base64_images_by_pixel_dimensions() {
        let tiny = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": [{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":png_base64(16, 32)}
            }]}]
        }));
        let large = translated_request(json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": [{
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":png_base64(1024, 1024)}
            }]}]
        }));

        assert_eq!(
            estimate_image_tokens(&format!("data:image/png;base64,{}", png_base64(1, 1))),
            IMAGE_OVERHEAD_TOKENS
        );
        assert!(count_tokens(&large) > count_tokens(&tiny));
    }

    #[test]
    fn image_estimate_is_bounded_for_untrusted_dimensions() {
        assert_eq!(
            estimate_image_tokens_from_dimensions(u64::MAX, u64::MAX),
            MAX_IMAGE_ESTIMATE_TOKENS
        );
    }
}
