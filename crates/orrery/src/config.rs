//! `moonraker.conf` parsing, following Moonraker's rules (`confighelper.py`): `key: value` or
//! `key = value`, full-line and inline (` #`, ` ;`) comments, indented continuation lines, and
//! `[include <glob>]`.
//!
//! Components read options through [`Section`] getters with defaults. Every read is recorded, so
//! `server.config` can report the parsed values the way Moonraker does.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

#[derive(Debug, Default)]
pub struct Config {
    /// Sections in file order, with options in file order. Option names are lowercase.
    sections: Vec<(String, Vec<(String, String)>)>,
    /// Each file read, with the sections it defines.
    files: Vec<(PathBuf, Vec<String>)>,
    /// Values read by components, as `server.config` reports them under `config`.
    parsed: Mutex<Map<String, Value>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = Config::default();
        let mut visited = BTreeSet::new();
        config.parse_file(path, &mut visited)?;
        Ok(config)
    }

    #[cfg(test)]
    pub fn from_str(text: &str) -> Result<Self> {
        let mut config = Config::default();
        config.parse_text(text, Path::new("moonraker.conf"), &mut BTreeSet::new())?;
        Ok(config)
    }

    fn parse_file(&mut self, path: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let canonical = path.canonicalize().with_context(|| format!("can't open {}", path.display()))?;
        if !visited.insert(canonical.clone()) {
            bail!("{} includes itself", path.display());
        }
        let text = std::fs::read_to_string(&canonical).with_context(|| format!("can't read {}", path.display()))?;
        self.parse_text(&text, path, visited)
    }

    fn parse_text(&mut self, text: &str, path: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let file_index = self.files.len();
        self.files.push((path.to_path_buf(), Vec::new()));
        let mut section: Option<usize> = None;
        let mut option_indent: Option<usize> = None;
        for (lineno, raw) in text.lines().enumerate() {
            let trimmed = raw.trim_start();
            if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
                continue;
            }
            let mut line = raw.replace('\t', "    ");
            if let Some(pos) = inline_comment(&line) {
                line.truncate(pos);
            }
            let line = line.replace(" \\#", " #").replace(" \\;", " ;");
            let indent = line.len() - line.trim_start().len();
            if let Some(oi) = option_indent
                && indent > oi
            {
                let (_, options) = &mut self.sections[section.expect("an option implies a section")];
                let value = &mut options.last_mut().expect("an indent implies an option").1;
                value.push('\n');
                value.push_str(line.trim());
                continue;
            }
            let content = line.trim();
            if let Some(name) = content.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                option_indent = None;
                let name = name.trim();
                if let Some(pattern) = name.strip_prefix("include ") {
                    self.include(pattern.trim(), path, visited)?;
                    section = None;
                    continue;
                }
                section = Some(match self.sections.iter().position(|(n, _)| n == name) {
                    Some(i) => i,
                    None => {
                        self.sections.push((name.to_owned(), Vec::new()));
                        self.sections.len() - 1
                    }
                });
                let file_sections = &mut self.files[file_index].1;
                if !file_sections.iter().any(|s| s == name) {
                    file_sections.push(name.to_owned());
                }
                continue;
            }
            let Some(sidx) = section else {
                bail!("{}:{}: option outside a section: {content}", path.display(), lineno + 1);
            };
            let Some(sep) = content.find([':', '=']) else {
                bail!("{}:{}: expected `option: value`, got {content:?}", path.display(), lineno + 1);
            };
            let key = content[..sep].trim().to_ascii_lowercase();
            let value = content[sep + 1..].trim().to_owned();
            let options = &mut self.sections[sidx].1;
            match options.iter_mut().find(|(k, _)| *k == key) {
                Some(existing) => existing.1 = value,
                None => options.push((key, value)),
            }
            option_indent = Some(indent);
        }
        Ok(())
    }

    fn include(&mut self, pattern: &str, from: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let base =
            if pattern.starts_with('/') { PathBuf::new() } else { from.parent().unwrap_or(Path::new(".")).into() };
        let full = base.join(pattern);
        let dir = full.parent().unwrap_or(Path::new("."));
        let name_pattern = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("[include {pattern}]: can't read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| glob_match(&name_pattern, &n.to_string_lossy())))
            .collect();
        if paths.is_empty() {
            bail!("no files match [include {pattern}]");
        }
        paths.sort();
        for p in paths {
            self.parse_file(&p, visited)?;
        }
        Ok(())
    }

    pub fn section(&self, name: &str) -> Section<'_> {
        let options = self.sections.iter().find(|(n, _)| n == name).map(|(_, o)| o.as_slice()).unwrap_or(&[]);
        let mut parsed = self.parsed.lock().expect("config lock");
        parsed.entry(name.to_owned()).or_insert_with(|| Value::Object(Map::new()));
        Section { config: self, name: name.to_owned(), options }
    }

    /// The names of sections that start with `prefix`, such as every `[webcam <name>]`.
    pub fn section_names(&self, prefix: &str) -> Vec<String> {
        self.sections.iter().map(|(n, _)| n).filter(|n| n.starts_with(prefix)).cloned().collect()
    }

    /// `server.config`'s `orig`: every section's raw options, after a `DEFAULT` entry.
    pub fn orig(&self) -> Value {
        let mut out = Map::new();
        out.insert("DEFAULT".into(), json!({}));
        for (name, options) in &self.sections {
            let opts: Map<String, Value> = options.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            out.insert(name.clone(), Value::Object(opts));
        }
        Value::Object(out)
    }

    pub fn parsed(&self) -> Value {
        Value::Object(self.parsed.lock().expect("config lock").clone())
    }

    /// `server.config`'s `files`: each file's name relative to the main file's folder.
    pub fn files(&self) -> Value {
        let base = self.files.first().and_then(|(p, _)| p.parent()).map(Path::to_path_buf).unwrap_or_default();
        let list: Vec<Value> = self
            .files
            .iter()
            .map(|(path, sections)| {
                let name = path.strip_prefix(&base).unwrap_or(path).display().to_string();
                json!({"filename": name, "sections": sections})
            })
            .collect();
        Value::Array(list)
    }
}

/// Finds an inline comment: a `#` or `;` preceded by at least one space.
fn inline_comment(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    (1..bytes.len()).find(|&i| matches!(bytes[i], b'#' | b';') && bytes[i - 1] == b' ').map(|i| {
        let mut start = i;
        while start > 0 && bytes[start - 1] == b' ' {
            start -= 1;
        }
        start
    })
}

/// Shell-style `*` and `?` matching, enough for `[include]` file names.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// One section's options. Each getter records the value it returns in `server.config`.
pub struct Section<'c> {
    config: &'c Config,
    name: String,
    options: &'c [(String, String)],
}

impl Section<'_> {
    fn raw(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn record(&self, key: &str, value: Value) {
        let mut parsed = self.config.parsed.lock().expect("config lock");
        if let Some(Value::Object(section)) = parsed.get_mut(&self.name) {
            section.insert(key.to_owned(), value);
        }
    }

    fn error(&self, key: &str, what: &str) -> anyhow::Error {
        anyhow::anyhow!("[{}] {key}: {what}, got {:?}", self.name, self.raw(key).unwrap_or_default())
    }

    pub fn get_str(&self, key: &str, default: &str) -> String {
        let v = self.raw(key).unwrap_or(default).to_owned();
        self.record(key, json!(v));
        v
    }

    pub fn get_opt_str(&self, key: &str) -> Option<String> {
        let v = self.raw(key).map(str::to_owned);
        self.record(key, json!(v));
        v
    }

    /// One item per line, as Moonraker's `getlist`. Blank lines are skipped.
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let v: Vec<String> = self
            .raw(key)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        self.record(key, json!(v));
        v
    }

    pub fn get_int(&self, key: &str, default: i64) -> Result<i64> {
        let v = match self.raw(key) {
            Some(s) => s.parse().map_err(|_| self.error(key, "expected a whole number"))?,
            None => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }

    pub fn get_float(&self, key: &str, default: f64) -> Result<f64> {
        let v = match self.raw(key) {
            Some(s) => s.parse().map_err(|_| self.error(key, "expected a number"))?,
            None => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }

    pub fn get_bool(&self, key: &str, default: bool) -> Result<bool> {
        let v = match self.raw(key).map(str::to_ascii_lowercase).as_deref() {
            Some("true" | "yes" | "on" | "1") => true,
            Some("false" | "no" | "off" | "0") => false,
            Some(_) => return Err(self.error(key, "expected true or false")),
            None => default,
        };
        self.record(key, json!(v));
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_comments_and_multiline() {
        let c = Config::from_str(
            "# comment\n[server]\nhost: 0.0.0.0  # inline\nPort = 7125\n\n[authorization]\ntrusted_clients:\n  10.0.0.0/8\n  127.0.0.1\n",
        )
        .unwrap();
        let s = c.section("server");
        assert_eq!(s.get_str("host", ""), "0.0.0.0");
        assert_eq!(s.get_int("port", 0).unwrap(), 7125);
        assert_eq!(c.orig()["authorization"]["trusted_clients"], json!("\n10.0.0.0/8\n127.0.0.1"));
        assert_eq!(c.parsed()["server"]["port"], json!(7125));
        assert_eq!(c.orig()["server"]["host"], json!("0.0.0.0"));
    }

    #[test]
    fn missing_section_gives_defaults() {
        let c = Config::from_str("[server]\n").unwrap();
        assert!(!c.section("history").get_bool("x", false).unwrap());
        assert_eq!(c.parsed()["history"]["x"], json!(false));
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.conf", "a.conf"));
        assert!(!glob_match("*.conf", "a.cfg"));
        assert!(glob_match("x?.conf", "x1.conf"));
    }
}
