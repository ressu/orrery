//! Readers: one for each format, which tell Orrery what to print, the metadata, and the
//! thumbnails (docs/design/2026-11-27-file-tools.md). Gcode's reader is [`super::metadata`].
//! UFP's, sliced 3MF's, and PrusaSlicer's binary gcode's are built in. `[reader.<name>]` in `orrery.toml` changes one, or adds a program for
//! another format.
//!
//! Every reader writes only into its work folder, and gives an [`Output`]. [`super::place`]
//! puts what it lists in place.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::work::{TOOL_NICE, WorkDir};
use crate::config::Config;

/// The version of the result a program prints.
pub const OUTPUT_VERSION: u64 = 1;

/// A UFP file can take a while, as in Moonraker, which gives it 300 seconds.
const DEFAULT_TIMEOUT: f64 = 300.0;

/// The readers Orrery has without a table: name, extension, and what runs.
const BUILT_IN: &[(&str, &str, Run)] =
    &[("ufp", ".ufp", Run::Ufp), ("gcode_3mf", ".gcode.3mf", Run::Sliced3mf), ("bgcode", ".bgcode", Run::Bgcode)];

/// Gcode is read by Orrery's own metadata code, never by a table.
const GCODE_EXTS: &[&str] = &[".gcode", ".g", ".gco", ".nc"];

/// Where a UFP file keeps its gcode and its picture.
const UFP_MODEL: &str = "3D/model.gcode";
const UFP_THUMBNAIL: &str = "Metadata/thumbnail.png";

#[derive(Debug, Clone, PartialEq)]
enum Run {
    Ufp,
    Sliced3mf,
    Bgcode,
    Command(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reader {
    pub name: String,
    /// Lowercase, with its dot.
    pub extension: String,
    run: Run,
    timeout: Duration,
}

#[derive(Debug, Default)]
pub struct Readers {
    list: Vec<Reader>,
}

/// What a reader made. Every field is optional, and paths are in its work folder.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub gcode: Option<String>,
    #[serde(default)]
    pub thumbnails: Vec<Thumbnail>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Thumbnail {
    pub file: String,
    pub width: u32,
    pub height: u32,
}

impl Readers {
    pub fn new(config: &Config) -> Result<Self> {
        let mut names: Vec<String> = BUILT_IN.iter().map(|(n, _, _)| (*n).to_owned()).collect();
        for section in config.section_names("reader ") {
            let name = section["reader ".len()..].to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let mut list: Vec<Reader> = Vec::new();
        for name in names {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                bail!("[reader.{name}]: a reader name may use letters, digits, '_' and '-'");
            }
            let section = format!("reader {name}");
            let built_in = BUILT_IN.iter().find(|(n, _, _)| *n == name);
            // A built-in reader without a table isn't read as a section, so that server.config
            // shows only what the config has.
            if config.section_json(&section).is_none() {
                if let Some((_, extension, run)) = built_in {
                    let timeout = Duration::from_secs_f64(DEFAULT_TIMEOUT);
                    list.push(Reader { name, extension: (*extension).to_owned(), run: run.clone(), timeout });
                }
                continue;
            }
            let s = config.section(&section);
            if !s.get_bool("enabled", true)? {
                continue;
            }
            let extension = match (s.get_opt_str("extension"), built_in) {
                (Some(e), _) => e,
                (None, Some((_, e, _))) => (*e).to_owned(),
                (None, None) => bail!("[reader.{name}]: extension is missing: the files it reads, such as \".{name}\""),
            }
            .to_lowercase();
            if !extension.starts_with('.') || extension.len() < 2 || extension.contains('/') {
                bail!(
                    "[reader.{name}] extension: expected a file extension with its dot, such as \".{name}\", got {extension:?}"
                );
            }
            // An extension can have parts, such as `.gcode.3mf`, which a sliced 3MF has.
            if extension[1..].split('.').any(str::is_empty) {
                bail!("[reader.{name}] extension: expected a file extension such as \".{name}\", got {extension:?}");
            }
            if GCODE_EXTS.contains(&extension.as_str()) {
                bail!("[reader.{name}] extension: Orrery reads {extension} files itself; a processor changes gcode");
            }
            if let Some(other) = list.iter().find(|c| c.extension == extension) {
                bail!("[reader.{name}] extension: [reader.{}] reads {extension} files already", other.name);
            }
            let run = match s.get_value("command") {
                None if built_in.is_some() => built_in.map(|(_, _, run)| run.clone()).expect("built in"),
                None => bail!("[reader.{name}]: command is missing: the program that reads {extension} files"),
                Some(Value::String(p)) if !p.is_empty() => Run::Command(vec![p]),
                Some(Value::Array(items)) if !items.is_empty() => {
                    let words: Option<Vec<String>> = items.iter().map(|i| i.as_str().map(str::to_owned)).collect();
                    match words.filter(|w| !w[0].is_empty()) {
                        Some(w) => Run::Command(w),
                        None => bail!("[reader.{name}] command: expected a list of strings, starting with the program"),
                    }
                }
                Some(other) => {
                    bail!(
                        "[reader.{name}] command: expected the program, or a list of the program and its arguments, got {other}"
                    )
                }
            };
            let timeout = s.get_float("timeout", DEFAULT_TIMEOUT)?;
            if !(timeout > 0.0 && timeout.is_finite()) {
                bail!("[reader.{name}] timeout: expected a number of seconds above 0, got {timeout}");
            }
            list.push(Reader { name, extension, run, timeout: Duration::from_secs_f64(timeout) });
        }
        Ok(Self { list })
    }

    /// The reader for a file's extension, the longest that matches, so `.gcode.3mf` wins over
    /// `.3mf`. Gcode files have none: Orrery reads them itself.
    pub fn for_path(&self, name: &str) -> Option<&Reader> {
        // ASCII only, so the name keeps its length, and the extension can be cut off it.
        let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
        self.list
            .iter()
            .filter(|c| base.len() > c.extension.len() && base.ends_with(&c.extension))
            .max_by_key(|c| c.extension.len())
    }

    /// The readers, in the order they're configured: built in first.
    pub fn iter(&self) -> impl Iterator<Item = &Reader> {
        self.list.iter()
    }

    /// Whether a reader reads files with this extension, such as `.ufp`.
    pub fn reads(&self, extension: &str) -> bool {
        self.list.iter().any(|c| c.extension == extension)
    }
}

impl Reader {
    /// Reads `file`, writing into `work`. The error is the reader's reason.
    pub async fn read(&self, file: &Path, work: &WorkDir, cwd: &Path) -> Result<Output, String> {
        let output = match &self.run {
            Run::Ufp | Run::Sliced3mf | Run::Bgcode => {
                let (file, dir, run) = (file.to_path_buf(), work.dir.clone(), self.run.clone());
                let job = super::work::low_priority(move || {
                    let read = match run {
                        Run::Ufp => read_ufp(&file, &dir),
                        Run::Sliced3mf => read_sliced_3mf(&file, &dir),
                        _ => read_bgcode(&file, &dir),
                    };
                    read.map_err(|e| format!("{e:#}"))
                });
                match tokio::time::timeout(self.timeout, job).await {
                    Err(_) => return Err(self.too_long()),
                    Ok(None) => return Err("the reader stopped".into()),
                    Ok(Some(r)) => r?,
                }
            }
            Run::Command(command) => {
                let input = work
                    .link_input(file, &self.extension)
                    .map_err(|e| format!("can't give the reader {}: {e}", file.display()))?;
                self.run_command(command, &input, &work.dir, cwd).await?
            }
        };
        if output.version.is_some_and(|v| v != OUTPUT_VERSION) {
            return Err(format!(
                "[reader.{}] gave a result of version {:?}; Orrery reads {OUTPUT_VERSION}",
                self.name, output.version
            ));
        }
        if output.gcode.is_none() {
            return Err(format!("[reader.{}] gave no gcode to print", self.name));
        }
        Ok(output)
    }

    fn too_long(&self) -> String {
        format!("[reader.{}] took longer than {}s", self.name, self.timeout.as_secs_f64())
    }

    async fn run_command(&self, command: &[String], input: &Path, dir: &Path, cwd: &Path) -> Result<Output, String> {
        let program = crate::expand_home(Path::new(&command[0])).map_err(|e| e.to_string())?;
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&command[1..]).arg(input).arg(dir).stdin(std::process::Stdio::null()).kill_on_drop(true);
        if cwd.is_dir() {
            cmd.current_dir(cwd);
        }
        // SAFETY: the closure runs in the child between fork and exec, and makes only one system
        // call, setpriority, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setpriority_process(None, TOOL_NICE)?;
                Ok(())
            });
        }
        let out = match tokio::time::timeout(self.timeout, cmd.output()).await {
            Err(_) => return Err(self.too_long()),
            Ok(Err(e)) => return Err(format!("can't run {}: {e}", program.display())),
            Ok(Ok(out)) => out,
        };
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if !out.status.success() {
            let status = out.status.code().map_or_else(|| "a signal".to_owned(), |c| format!("code {c}"));
            return Err(if stderr.is_empty() {
                format!("[reader.{}] failed with {status}", self.name)
            } else {
                stderr
            });
        }
        if !stderr.is_empty() {
            tracing::info!("[reader.{}]: {stderr}", self.name);
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("[reader.{}] printed no result Orrery can read: {e}", self.name))
    }
}

/// UFP, Cura's Ultimaker Format Package: a zip with the gcode and a picture. Cura writes the
/// names with a leading `/`; either is taken. The picture gets a 32x32 copy, as in Moonraker.
fn read_ufp(input: &Path, dir: &Path) -> Result<Output> {
    let file = std::fs::File::open(input).map_err(|e| anyhow::anyhow!("can't open {}: {e}", input.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|_| anyhow::anyhow!("it isn't a UFP file: it isn't a zip file"))?;
    let name_of = |zip: &zip::ZipArchive<std::fs::File>, want: &str| {
        zip.file_names().flatten().find(|n| n.trim_start_matches('/') == want).map(|n| n.into_owned())
    };
    let Some(model) = name_of(&zip, UFP_MODEL) else { bail!("it isn't a UFP file: it has no /{UFP_MODEL}") };
    let picture = name_of(&zip, UFP_THUMBNAIL);
    copy_entry(&mut zip, &model, &dir.join("model.gcode"))?;
    let mut output = Output { version: Some(OUTPUT_VERSION), gcode: Some("model.gcode".into()), ..Output::default() };
    if let Some(name) = picture {
        let path = dir.join("picture.png");
        copy_entry(&mut zip, &name, &path)?;
        match image::image_dimensions(&path) {
            Ok((width, height)) => {
                if let Ok((w, h)) = super::metadata::make_miniature(&path, &dir.join("small.png")) {
                    output.thumbnails.push(Thumbnail { file: "small.png".into(), width: w, height: h });
                }
                output.thumbnails.push(Thumbnail { file: "picture.png".into(), width, height });
            }
            Err(e) => tracing::info!("The UFP file's picture can't be read: {e}"),
        }
    }
    Ok(output)
}

/// A sliced 3MF, as OrcaSlicer and BambuStudio export a plate: a zip with each sliced plate's
/// gcode at `Metadata/plate_<n>.gcode`, and its pictures at `Metadata/plate_<n>.png` and
/// `Metadata/plate_<n>_small.png`. The other pictures, such as `pick_<n>.png`, aren't of the
/// print. Exactly one plate must be sliced; the gcode reader reads its metadata.
fn read_sliced_3mf(input: &Path, dir: &Path) -> Result<Output> {
    let file = std::fs::File::open(input).map_err(|e| anyhow::anyhow!("can't open {}: {e}", input.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|_| anyhow::anyhow!("it isn't a 3MF file: it isn't a zip file"))?;
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    let plate = |n: &str| -> Option<u32> {
        n.trim_start_matches('/').strip_prefix("Metadata/plate_")?.strip_suffix(".gcode")?.parse().ok()
    };
    let mut plates: Vec<(u32, &String)> = names.iter().filter_map(|n| plate(n).map(|p| (p, n))).collect();
    plates.sort();
    let (number, gcode) = match plates.as_slice() {
        [] => bail!("it has no sliced plate: export a plate from the slicer after slicing it"),
        [one] => *one,
        more => bail!("it has {} sliced plates, and Orrery prints one: export one plate", more.len()),
    };
    copy_entry(&mut zip, gcode, &dir.join("model.gcode"))?;
    let mut output = Output { version: Some(OUTPUT_VERSION), gcode: Some("model.gcode".into()), ..Output::default() };
    for (want, file) in [
        (format!("Metadata/plate_{number}_small.png"), "small.png"),
        (format!("Metadata/plate_{number}.png"), "plate.png"),
    ] {
        let Some(entry) = names.iter().find(|n| n.trim_start_matches('/') == want) else { continue };
        copy_entry(&mut zip, entry, &dir.join(file))?;
        match image::image_dimensions(dir.join(file)) {
            Ok((width, height)) => output.thumbnails.push(Thumbnail { file: file.into(), width, height }),
            Err(e) => tracing::info!("The 3MF file's picture {want} can't be read: {e}"),
        }
    }
    // A 32x32 one for lists, from the largest, as for gcode.
    if let Some(largest) = output.thumbnails.iter().max_by_key(|t| t.width * t.height).cloned()
        && !output.thumbnails.iter().any(|t| t.width <= 32 && t.height <= 32)
        && let Ok((width, height)) = super::metadata::make_miniature(&dir.join(&largest.file), &dir.join("mini.png"))
    {
        output.thumbnails.insert(0, Thumbnail { file: "mini.png".into(), width, height });
    }
    Ok(output)
}

/// PrusaSlicer's binary gcode: the gcode as text, with the metadata as comments, which the
/// gcode reader reads, and the thumbnails as files. A 32x32 one is made when there's none, as
/// the gcode reader does for a text gcode, from the one with the most pixels: the gcode reader
/// goes by file size, as Moonraker does, which a QOI file's lack of compression would skew.
fn read_bgcode(input: &Path, dir: &Path) -> Result<Output> {
    let mut thumbnails = super::bgcode::read(input, dir, "model.gcode")?;
    if !thumbnails.iter().any(|t| t.width == 32 && t.height == 32)
        && let Some(largest) = thumbnails.iter().max_by_key(|t| t.width * t.height).cloned()
    {
        match super::metadata::make_miniature(&dir.join(&largest.file), &dir.join("small.png")) {
            Ok((width, height)) => thumbnails.insert(0, Thumbnail { file: "small.png".into(), width, height }),
            Err(e) => tracing::info!("{e}"),
        }
    }
    Ok(Output { version: Some(OUTPUT_VERSION), gcode: Some("model.gcode".into()), thumbnails, ..Output::default() })
}

fn copy_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str, dest: &Path) -> Result<()> {
    let shown = name.trim_start_matches('/');
    let mut entry = zip.by_name(name).map_err(|e| anyhow::anyhow!("can't read /{shown}: {e}"))?;
    let mut out = std::fs::File::create(dest).map_err(|e| anyhow::anyhow!("can't write {}: {e}", dest.display()))?;
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = entry.read(&mut buf).map_err(|e| anyhow::anyhow!("can't unpack /{shown}: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    out.flush()?;
    Ok(())
}

/// The gcode a reader made for `rel`, in the gcodes folder's cache: `.cache/<rel>.gcode`.
/// Klipper prints it from there.
pub fn cache_name(rel: &str) -> String {
    format!("{CACHE_DIR}/{rel}.gcode")
}

/// The cache, in the gcodes folder, where Klipper can print from it. Clients can't reach it.
pub const CACHE_DIR: &str = ".cache";

/// The file a name Klipper reports stands for: `.cache/cube.ufp.gcode` is `cube.ufp`, and
/// `/home/pi/printer_data/gcodes/.cache/cube.ufp.gcode` is `/home/pi/printer_data/gcodes/cube.ufp`.
/// `None` for a name that isn't in the cache.
pub fn file_for_printed(name: &str, readers: &Readers) -> Option<String> {
    let (before, rest) = if let Some(rest) = name.strip_prefix(&format!("{CACHE_DIR}/")) {
        ("", rest)
    } else {
        let marker = format!("/{CACHE_DIR}/");
        let i = name.find(&marker)?;
        (&name[..=i], &name[i + marker.len()..])
    };
    let file = rest.strip_suffix(".gcode")?;
    readers.for_path(file).map(|_| format!("{before}{file}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        let mut c = Config::parse("[server]\n").unwrap();
        c.parse_toml(toml, Path::new("orrery.toml")).unwrap();
        c
    }

    pub(crate) fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn ufp_is_built_in_and_tables_add_or_change_readers() {
        let r = Readers::new(&config("")).unwrap();
        let ufp = r.for_path("dir/Cube.UFP").unwrap();
        assert_eq!(ufp.run, Run::Ufp);
        assert_eq!(ufp.timeout, Duration::from_secs(300));
        assert!(r.for_path("cube.gcode").is_none());

        let toml = "[reader.ufp]\ntimeout = 60\n\n[reader.bgcode]\nextension = \".BGCODE\"\ncommand = [\"~/bgcode\", \"-q\"]\n";
        let r = Readers::new(&config(toml)).unwrap();
        assert_eq!(r.for_path("a.ufp").unwrap().timeout, Duration::from_secs(60));
        assert_eq!(r.for_path("a.bgcode").unwrap().run, Run::Command(vec!["~/bgcode".into(), "-q".into()]));

        let off = Readers::new(&config("[reader.ufp]\nenabled = false\n")).unwrap();
        assert!(off.for_path("a.ufp").is_none());

        // The longest extension wins, and a 3MF project that isn't sliced has no reader.
        let r = Readers::new(&config("[reader.project]\nextension = \".3mf\"\ncommand = \"x\"\n")).unwrap();
        assert_eq!(r.for_path("dir/Cube.GCODE.3MF").unwrap().name, "gcode_3mf");
        assert_eq!(r.for_path("cube.3mf").unwrap().name, "project");
        assert!(Readers::new(&config("")).unwrap().for_path("cube.3mf").is_none());
        assert!(Readers::new(&config("")).unwrap().for_path(".gcode.3mf").is_none());
    }

    #[test]
    fn a_table_must_name_a_new_extension_and_a_program() {
        let err = |t: &str| Readers::new(&config(t)).unwrap_err().to_string();
        assert!(err("[reader.sl1]\ncommand = \"x\"\n").contains("extension is missing"));
        assert!(err("[reader.x]\nextension = \".x\"\n").contains("command is missing"));
        assert!(err("[reader.x]\nextension = \"x\"\ncommand = \"x\"\n").contains("with its dot"));
        assert!(err("[reader.x]\nextension = \".tar..gz\"\ncommand = \"x\"\n").contains("expected a file extension"));
        assert!(err("[reader.x]\nextension = \".gcode\"\ncommand = \"x\"\n").contains("reads .gcode files itself"));
        assert!(err("[reader.x]\nextension = \".ufp\"\ncommand = \"x\"\n").contains("[reader.ufp] reads .ufp"));
        assert!(err("[reader.x]\nextension = \".x\"\ncommand = \"x\"\ntimeout = 0\n").contains("above 0"));
        assert!(err("[reader.x]\nextension = \".x\"\ncommand = 3\n").contains("expected the program"));
    }

    #[test]
    fn ufp_gives_the_gcode_and_the_picture() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("cube.ufp");
        let picture = png(64, 48);
        write_zip(&input, &[("/3D/model.gcode", b"G28\n"), ("/Metadata/thumbnail.png", &picture), ("/other", b"x")]);
        let out = tempfile::tempdir().unwrap();
        let output = read_ufp(&input, out.path()).unwrap();
        assert_eq!(output.gcode.as_deref(), Some("model.gcode"));
        assert_eq!(std::fs::read(out.path().join("model.gcode")).unwrap(), b"G28\n");
        assert_eq!(
            output.thumbnails,
            [
                Thumbnail { file: "small.png".into(), width: 32, height: 24 },
                Thumbnail { file: "picture.png".into(), width: 64, height: 48 }
            ]
        );

        // Without the leading slash, and without a picture.
        write_zip(&input, &[("3D/model.gcode", b"G1\n")]);
        let output = read_ufp(&input, out.path()).unwrap();
        assert!(output.thumbnails.is_empty());

        write_zip(&input, &[("/Metadata/thumbnail.png", b"png")]);
        assert!(format!("{:#}", read_ufp(&input, out.path()).unwrap_err()).contains("no /3D/model.gcode"));
        std::fs::write(&input, "G28\n").unwrap();
        assert!(format!("{:#}", read_ufp(&input, out.path()).unwrap_err()).contains("isn't a zip file"));
    }

    #[test]
    fn a_sliced_3mf_gives_its_one_plate() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("cube.gcode.3mf");
        let (big, small) = (png(512, 512), png(128, 128));
        write_zip(
            &input,
            &[
                ("3D/3dmodel.model", b"<model/>"),
                ("Metadata/plate_2.gcode", b"G28\n"),
                ("Metadata/plate_2.gcode.md5", b"X"),
                ("Metadata/plate_2.png", &big),
                ("Metadata/plate_2_small.png", &small),
                ("Metadata/pick_2.png", &big),
            ],
        );
        let out = tempfile::tempdir().unwrap();
        let output = read_sliced_3mf(&input, out.path()).unwrap();
        assert_eq!(std::fs::read(out.path().join("model.gcode")).unwrap(), b"G28\n");
        let sizes: Vec<(u32, u32)> = output.thumbnails.iter().map(|t| (t.width, t.height)).collect();
        assert_eq!(sizes, [(32, 32), (128, 128), (512, 512)]);

        write_zip(&input, &[("Metadata/plate_1.gcode", b"G28\n"), ("Metadata/plate_2.gcode", b"G28\n")]);
        assert!(format!("{:#}", read_sliced_3mf(&input, out.path()).unwrap_err()).contains("2 sliced plates"));
        write_zip(&input, &[("3D/3dmodel.model", b"<model/>")]);
        assert!(format!("{:#}", read_sliced_3mf(&input, out.path()).unwrap_err()).contains("no sliced plate"));
    }

    #[tokio::test]
    async fn a_program_gets_its_input_and_folder_and_prints_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("reader");
        std::fs::write(
            &script,
            "#!/bin/sh\ncp \"$1\" \"$2/out.gcode\"\necho '{\"version\": 1, \"gcode\": \"out.gcode\", \"metadata\": {\"slicer\": \"X\"}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let reader = Reader {
            name: "x".into(),
            extension: ".x".into(),
            run: Run::Command(vec![script.to_string_lossy().into_owned()]),
            timeout: Duration::from_secs(5),
        };
        let file = dir.path().join("cube.x");
        std::fs::write(&file, "G28\n").unwrap();
        let work = WorkDir::new(dir.path()).unwrap();
        let output = reader.read(&file, &work, dir.path()).await.unwrap();
        assert_eq!(output.gcode.as_deref(), Some("out.gcode"));
        assert_eq!(output.metadata["slicer"], "X");
        assert_eq!(std::fs::read(work.dir.join("out.gcode")).unwrap(), b"G28\n");

        std::fs::write(&script, "#!/bin/sh\necho \"no good: $(basename $1)\" >&2\nexit 3\n").unwrap();
        assert_eq!(reader.read(&file, &work, dir.path()).await.unwrap_err(), "no good: input.x");
        std::fs::write(&script, "#!/bin/sh\necho '{\"metadata\": {}}'\n").unwrap();
        assert!(reader.read(&file, &work, dir.path()).await.unwrap_err().contains("no gcode"));
        std::fs::write(&script, "#!/bin/sh\necho '{\"gcode\": \"a\", \"extra\": 1}'\n").unwrap();
        assert!(reader.read(&file, &work, dir.path()).await.unwrap_err().contains("no result Orrery can read"));
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        let quick = Reader { timeout: Duration::from_millis(100), ..reader };
        assert!(quick.read(&file, &work, dir.path()).await.unwrap_err().contains("took longer than 0.1s"));
    }

    #[test]
    fn printed_names_map_back_to_their_files() {
        let r = Readers::new(&config("")).unwrap();
        assert_eq!(cache_name("sub/cube.ufp"), ".cache/sub/cube.ufp.gcode");
        assert_eq!(file_for_printed(".cache/sub/cube.ufp.gcode", &r).as_deref(), Some("sub/cube.ufp"));
        assert_eq!(
            file_for_printed("/home/pi/printer_data/gcodes/.cache/cube.ufp.gcode", &r).as_deref(),
            Some("/home/pi/printer_data/gcodes/cube.ufp")
        );
        assert_eq!(file_for_printed("cube.gcode", &r), None);
        assert_eq!(file_for_printed(".cache/cube.gcode", &r), None);
        assert_eq!(file_for_printed(".cache/cube.ufp", &r), None);
    }
}
