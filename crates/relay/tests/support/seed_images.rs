//! An invented data set for the image reads. Every id starts with `img-`. The rows belong to
//! the chat `img-chat`; each one holds the frame of an assistant-side user message that carries
//! tool results, as Conductor stores them. The repositories hold no icon files: a test makes
//! the directory it needs and gives a repository its root.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub const CHAT: &str = "img-chat";

/// Tiny valid images, as base64.
pub const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
pub const JPEG: &str = "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAAaADAAQAAAABAAAAAQAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8AAEQgAAQABAwEiAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAABAgMEBQYHCAkKC//EALUQAAIBAwMCBAMFBQQEAAABfQECAwAEEQUSITFBBhNRYQcicRQygZGhCCNCscEVUtHwJDNicoIJChYXGBkaJSYnKCkqNDU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6g4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2drh4uPk5ebn6Onq8fLz9PX29/j5+v/EAB8BAAMBAQEBAQEBAQEAAAAAAAABAgMEBQYHCAkKC//EALURAAIBAgQEAwQHBQQEAAECdwABAgMRBAUhMQYSQVEHYXETIjKBCBRCkaGxwQkjM1LwFWJy0QoWJDThJfEXGBkaJicoKSo1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoKDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uLj5OXm5+jp6vLz9PX29/j5+v/bAEMAAgICAgICAwICAwUDAwMFBgUFBQUGCAYGBgYGCAoICAgICAgKCgoKCgoKCgwMDAwMDA4ODg4ODw8PDw8PDw8PD//bAEMBAgICBAQEBwQEBxALCQsQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEP/dAAQAAf/aAAwDAQACEQMRAD8A7iiiiv8AQA+XP//Z";
pub const GIF: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
pub const WEBP: &str = "UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==";
/// Valid base64 (the bytes of "plain text") that no image format starts with.
pub const OTHER: &str = "cGxhaW4gdGV4dA==";

/// The rowids the rows get, in the order they are inserted below.
pub const ROW_SINGLE: i64 = 1;
pub const ROW_TWO_RESULTS: i64 = 2;
pub const ROW_SNIFFED: i64 = 3;
pub const ROW_FOREIGN_TYPE: i64 = 4;
pub const ROW_BAD_DATA: i64 = 5;
pub const ROW_NO_DATA: i64 = 6;
pub const ROW_PLAIN: i64 = 7;
pub const ROW_SURROGATE: i64 = 8;
pub const ROW_NOT_A_LIST: i64 = 9;

/// The repository without a root, and the one with an empty root.
pub const REPO_NO_ROOT: &str = "img-repo-no-root";
pub const REPO_EMPTY_ROOT: &str = "img-repo-empty-root";

fn image(media_type: Option<&str>, data: &str) -> Value {
    let mut source = json!({ "type": "base64", "data": data });
    if let Some(media_type) = media_type {
        source["media_type"] = json!(media_type);
    }
    json!({ "type": "image", "source": source })
}

fn result(id: &str, content: Value) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": content })
}

fn frame(blocks: Vec<Value>) -> String {
    json!({ "type": "user", "message": { "role": "user", "content": blocks } }).to_string()
}

fn insert_message(conn: &Connection, id: &str, second: u32, content: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at, sent_at) \
         VALUES (?1, ?2, 'user', ?3, ?4, ?4)",
        params![id, CHAT, content, format!("2026-09-14 10:00:{second:02}")],
    )
    .unwrap();
}

pub fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO sessions (id, status, title, created_at, updated_at) \
         VALUES (?1, 'idle', 'Untitled', '2026-09-14 09:00:00', '2026-09-14 09:00:00')",
        params![CHAT],
    )
    .unwrap();

    // ROW_SINGLE: a text block and one image that states its type.
    insert_message(
        conn,
        "img-msg-single",
        1,
        &frame(vec![result(
            "img-toolu-1",
            json!([
                { "type": "text", "text": "captured" },
                image(Some("image/png"), PNG)
            ]),
        )]),
    );

    // ROW_TWO_RESULTS: three images over three results, one of which is a plain string and
    // one of which has text between its images. Numbered 0 and 1 (first result), 2 (third).
    insert_message(
        conn,
        "img-msg-two-results",
        2,
        &frame(vec![
            result(
                "img-toolu-2",
                json!([
                    image(Some("image/png"), PNG),
                    { "type": "text", "text": "between" },
                    image(Some("image/gif"), GIF)
                ]),
            ),
            result("img-toolu-3", json!("only text, no image")),
            result("img-toolu-4", json!([image(Some("image/jpeg"), JPEG)])),
        ]),
    );

    // ROW_SNIFFED: no media type, so the first characters of the base64 name it.
    insert_message(
        conn,
        "img-msg-sniffed",
        3,
        &frame(vec![result(
            "img-toolu-5",
            json!([
                image(None, PNG),
                image(None, JPEG),
                image(None, GIF),
                image(None, WEBP),
                image(None, OTHER)
            ]),
        )]),
    );

    // ROW_FOREIGN_TYPE: a stated type outside the four is not trusted.
    insert_message(
        conn,
        "img-msg-foreign-type",
        4,
        &frame(vec![result(
            "img-toolu-6",
            json!([
                image(Some("image/svg+xml"), PNG),
                image(Some("text/html"), OTHER),
                image(Some("image/webp"), WEBP)
            ]),
        )]),
    );

    // ROW_BAD_DATA: base64 that does not decode.
    insert_message(
        conn,
        "img-msg-bad-data",
        5,
        &frame(vec![result(
            "img-toolu-7",
            json!([image(Some("image/png"), "not base64 at all!")]),
        )]),
    );

    // ROW_NO_DATA: an image block without data, and one with blank data.
    insert_message(
        conn,
        "img-msg-no-data",
        6,
        &frame(vec![result(
            "img-toolu-8",
            json!([
                { "type": "image", "source": { "type": "base64", "media_type": "image/png" } },
                image(Some("image/png"), "  ")
            ]),
        )]),
    );

    // ROW_PLAIN: a prompt, not a frame.
    insert_message(conn, "img-msg-plain", 7, "show me a screenshot");

    // ROW_SURROGATE: a lone surrogate escape in the text of the result, which only the
    // second parse accepts. Written by hand: serde_json would not produce it.
    let surrogate = format!(
        r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"img-toolu-9","content":[{{"type":"text","text":"half \ud83d"}},{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{PNG}"}}}}]}}]}}}}"#
    );
    insert_message(conn, "img-msg-surrogate", 8, &surrogate);

    // ROW_NOT_A_LIST: results whose content is a string or an object hold no images.
    insert_message(
        conn,
        "img-msg-not-a-list",
        9,
        &frame(vec![
            result("img-toolu-10", json!("text")),
            result("img-toolu-11", json!({ "image": PNG })),
        ]),
    );

    for (id, name, root) in [
        ("img-repo-1", REPO_NO_ROOT, None),
        ("img-repo-2", REPO_EMPTY_ROOT, Some("")),
    ] {
        conn.execute(
            "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
            params![id, name, root],
        )
        .unwrap();
    }
}
