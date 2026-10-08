//! Images found in transcript entries.
//!
//! The transcript parser sends the phone a reference `<rowid>.<n>` for each image a tool
//! returned and keeps the bytes behind. This module finds one again from the stored row, with
//! the same walk and the same count the parser uses.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;

use super::js;
use super::parser::replace_lone_surrogates;

/// One image a tool returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// The media types an image answer may carry as the row states them.
const MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The `index`-th image of a stored row, counted exactly as the transcript parser numbers its
/// `images`: across every list-valued `tool_result` of the row, in order.
pub fn tool_image_at(content: &str, index: usize) -> Option<ToolImage> {
    let frame = serde_json::from_str::<Value>(content)
        .ok()
        .or_else(|| serde_json::from_str::<Value>(&replace_lone_surrogates(content)).ok())?;
    let blocks = js::property(&frame, "message")
        .and_then(|message| js::property(message, "content"))
        .and_then(Value::as_array)?;

    let mut seen = 0_usize;
    for block in blocks {
        if js::property(block, "type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(results) = js::property(block, "content").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            if js::property(result, "type").and_then(Value::as_str) != Some("image") {
                continue;
            }
            if seen != index {
                seen += 1;
                continue;
            }
            let source = js::property(result, "source");
            let data = js::non_blank(source.and_then(|source| js::property(source, "data")))?;
            let bytes = STANDARD.decode(data).ok()?;
            let stated =
                js::non_blank(source.and_then(|source| js::property(source, "media_type")));
            let media_type = match stated {
                Some(stated) if MEDIA_TYPES.contains(&stated) => stated,
                _ => sniff_image_type(data),
            };
            return Some(ToolImage {
                media_type: media_type.to_owned(),
                bytes,
            });
        }
    }
    None
}

/// The media type the first characters of the base64 text name, for an image block that states
/// none (or one outside the four served).
fn sniff_image_type(base64: &str) -> &'static str {
    if base64.starts_with("iVBOR") {
        "image/png"
    } else if base64.starts_with("/9j/") {
        "image/jpeg"
    } else if base64.starts_with("R0lGOD") {
        "image/gif"
    } else if base64.starts_with("UklGR") {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}
