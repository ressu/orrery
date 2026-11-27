//! Puts what a reader made in place: the gcode in the cache, thumbnails in `.thumbs`, and
//! metadata for the store (docs/design/2026-11-27-file-tools.md, "Placing the results").
//!
//! Nothing in a work folder is trusted. Only what the output lists is taken, at a path inside
//! the folder, and only plain files. Thumbnails must be images of the size the output says.
//! The names are Orrery's: an output says what a file is, not where it goes.

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

use super::readers::{Output, Thumbnail};

/// Metadata fields a reader may give, with their types, as `server.files.metadata` answers
/// them. Orrery sets `size`, `modified`, `uuid`, and `thumbnails` itself.
const FIELDS: &[(&str, Kind)] = &[
    ("slicer", Kind::Text),
    ("slicer_version", Kind::Text),
    ("gcode_start_byte", Kind::Number),
    ("gcode_end_byte", Kind::Number),
    ("layer_count", Kind::Number),
    ("object_height", Kind::Number),
    ("estimated_time", Kind::Number),
    ("nozzle_diameter", Kind::Number),
    ("layer_height", Kind::Number),
    ("first_layer_height", Kind::Number),
    ("first_layer_extr_temp", Kind::Number),
    ("first_layer_bed_temp", Kind::Number),
    ("chamber_temp", Kind::Number),
    ("filament_name", Kind::Text),
    ("filament_type", Kind::Text),
    ("filament_colors", Kind::List),
    ("extruder_colors", Kind::List),
    ("filament_temps", Kind::List),
    ("referenced_tools", Kind::List),
    ("mmu_print", Kind::Number),
    ("filament_change_count", Kind::Number),
    ("filament_total", Kind::Number),
    ("filament_weight_total", Kind::Number),
    ("filament_weights", Kind::List),
    ("printer_vendor", Kind::Text),
    ("printer_model", Kind::Text),
    ("printer_variant", Kind::Text),
    ("profile_version", Kind::Text),
];

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Number,
    List,
}

/// What [`place`] put where.
#[derive(Debug, Default)]
pub struct Placed {
    /// The metadata fields Orrery took.
    pub metadata: Map<String, Value>,
    /// `thumbnails` as the metadata lists them, relative to the file's folder.
    pub thumbnails: Vec<Value>,
}

/// Places what `output` lists from `work`. `gcode_dest` is where its gcode goes, if it has
/// one. Thumbnails go to `.thumbs/<base>-<width>x<height>.<ext>` in `folder`. A file that
/// doesn't pass is left out, and logged; a gcode that doesn't is an error.
pub fn place(
    output: Output,
    work: &Path,
    gcode_dest: Option<&Path>,
    folder: &Path,
    base: &str,
) -> Result<Placed, String> {
    let mut placed = Placed::default();
    if let (Some(gcode), Some(dest)) = (&output.gcode, gcode_dest) {
        let from = inside(work, gcode).ok_or_else(|| format!("the gcode {gcode:?} isn't a file in the work folder"))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("can't make {}: {e}", parent.display()))?;
        }
        put(&from, dest).map_err(|e| format!("can't put the gcode in place: {e}"))?;
    }
    for (key, value) in output.metadata {
        match FIELDS.iter().find(|(k, _)| *k == key) {
            Some((_, kind)) if fits(*kind, &value) => {
                placed.metadata.insert(key, value);
            }
            Some(_) => tracing::info!("A reader gave {key} as {value}, which isn't its type; it's left out"),
            None => tracing::debug!("A reader gave {key}, which Orrery doesn't know; it's left out"),
        }
    }
    let thumbs = folder.join(".thumbs");
    let mut seen: Vec<(u32, u32)> = Vec::new();
    for thumb in output.thumbnails {
        if seen.contains(&(thumb.width, thumb.height)) {
            continue;
        }
        match place_thumbnail(&thumb, work, &thumbs, base) {
            Ok(entry) => {
                seen.push((thumb.width, thumb.height));
                placed.thumbnails.push(entry);
            }
            Err(e) => tracing::info!("A thumbnail is left out: {}: {e}", thumb.file),
        }
    }
    Ok(placed)
}

fn place_thumbnail(thumb: &Thumbnail, work: &Path, thumbs: &Path, base: &str) -> Result<Value, String> {
    let from = inside(work, &thumb.file).ok_or("it isn't a file in the work folder")?;
    let reader = image::ImageReader::open(&from).and_then(|r| r.with_guessed_format()).map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("it isn't an image")?;
    let ext = match format {
        image::ImageFormat::Png | image::ImageFormat::Qoi => "png",
        image::ImageFormat::Jpeg => "jpg",
        other => return Err(format!("it's {other:?}; thumbnails are PNG, JPEG, or QOI")),
    };
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if (w, h) != (thumb.width, thumb.height) {
        return Err(format!("it's {w}x{h}, not {}x{}", thumb.width, thumb.height));
    }
    std::fs::create_dir_all(thumbs).map_err(|e| format!("can't make {}: {e}", thumbs.display()))?;
    let name = format!("{base}-{w}x{h}.{ext}");
    let dest = thumbs.join(&name);
    if format == image::ImageFormat::Qoi {
        let im = image::open(&from).map_err(|e| e.to_string())?;
        im.save_with_format(&dest, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    } else {
        put(&from, &dest).map_err(|e| e.to_string())?;
    }
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    Ok(json!({"width": w, "height": h, "size": size, "relative_path": format!(".thumbs/{name}")}))
}

/// The plain file at `path` in `work`, or `None` for a path that leaves it, or isn't a plain
/// file, such as a symlink.
fn inside(work: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    if path.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = work.join(rel);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    meta.file_type().is_file().then_some(full)
}

/// Moves a file into place, and checks that what arrived is a plain file, in case something
/// changed it after it was checked.
fn put(from: &Path, dest: &Path) -> std::io::Result<()> {
    super::move_path(from, dest)?;
    let is_file = std::fs::symlink_metadata(dest).map(|m| m.file_type().is_file()).unwrap_or(false);
    if !is_file {
        let _ = std::fs::remove_file(dest);
        return Err(std::io::Error::other("it isn't a plain file"));
    }
    Ok(())
}

fn fits(kind: Kind, value: &Value) -> bool {
    match kind {
        Kind::Text => value.is_string(),
        Kind::Number => value.is_number(),
        Kind::List => value.is_array(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn thumb(file: &str, width: u32, height: u32) -> Thumbnail {
        Thumbnail { file: file.into(), width, height }
    }

    #[test]
    fn only_what_passes_is_placed_with_orrerys_names() {
        let work = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let w = work.path();
        std::fs::write(w.join("model.gcode"), "G28\n").unwrap();
        std::fs::write(w.join("a.png"), png(64, 48)).unwrap();
        std::fs::write(w.join("wrong.png"), png(10, 10)).unwrap();
        std::fs::write(w.join("text.png"), "not an image").unwrap();
        std::fs::write(folder.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(folder.path().join("secret"), w.join("link.png")).unwrap();
        let output = Output {
            gcode: Some("model.gcode".into()),
            thumbnails: vec![
                thumb("a.png", 64, 48),
                thumb("a.png", 64, 48),
                thumb("wrong.png", 20, 20),
                thumb("text.png", 1, 1),
                thumb("link.png", 1, 1),
                thumb("../secret", 1, 1),
                thumb("/etc/passwd", 1, 1),
            ],
            metadata: serde_json::from_value(json!({
                "slicer": "Cura", "estimated_time": 930, "layer_height": "0.2", "size": 1, "made_up": true
            }))
            .unwrap(),
            ..Output::default()
        };
        let dest = folder.path().join(".cache/cube.ufp.gcode");
        let placed = place(output, w, Some(&dest), folder.path(), "cube.ufp").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"G28\n");
        assert_eq!(
            placed.metadata,
            serde_json::from_value::<Map<_, _>>(json!({"slicer": "Cura", "estimated_time": 930})).unwrap()
        );
        assert_eq!(placed.thumbnails.len(), 1);
        assert_eq!(placed.thumbnails[0]["relative_path"], ".thumbs/cube.ufp-64x48.png");
        assert!(folder.path().join(".thumbs/cube.ufp-64x48.png").is_file());

        let escape = Output { gcode: Some("../secret".into()), ..Output::default() };
        assert!(place(escape, w, Some(&dest), folder.path(), "x").unwrap_err().contains("isn't a file"));
    }
}
