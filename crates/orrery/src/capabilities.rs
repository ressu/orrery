//! `server.info`'s `capabilities`: what this server's API does, for clients to check instead of
//! comparing versions (docs/capabilities.md, docs/design/2026-11-27-capabilities.md).
//!
//! Each capability has a `version`, raised when what it enables changes in a way a client has
//! to know. Moonraker's `server.info` has no `capabilities`, so a client that finds none falls
//! back to what it does for Moonraker.

use serde_json::{Value, json};

use crate::files::VALID_GCODE_EXTS;
use crate::server::App;

pub fn capabilities(app: &App) -> Value {
    // A file a reader reads, such as `.ufp`, is that reader's format, not gcode.
    let gcode: Vec<&str> = VALID_GCODE_EXTS.iter().copied().filter(|e| !app.files.readers.reads(e)).collect();
    let mut formats = vec![json!({"name": "gcode", "extensions": gcode})];
    formats.extend(app.files.readers.iter().map(|r| json!({"name": r.name, "extensions": [r.extension]})));
    json!({
        "file_info": {"version": 1},
        "download_content": {"version": 1},
        "file_formats": {"version": 1, "formats": formats},
        "webcams": {"version": 1},
    })
}
