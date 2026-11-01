//! A read-only reader for SQLite database files, in pure Rust, with just enough of the file
//! format (https://www.sqlite.org/fileformat2.html) to read Moonraker's `moonraker-sql.db`:
//! rowid tables, interior and leaf table pages, overflow pages, and records. It skips indexes
//! and refuses WAL databases and files with a hot journal next to them.
//!
//! `dump` prints every row of every table as one JSON line, so the output can be diffed against
//! Python's `sqlite3` (see `dump_sqlite.py`).

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

pub struct SqliteFile {
    data: Vec<u8>,
    page_size: usize,
    usable: usize,
}

#[derive(Debug, Clone)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

pub struct Table {
    pub name: String,
    pub root_page: u32,
    pub columns: Vec<String>,
    /// The column that is an alias for the rowid (`INTEGER PRIMARY KEY`), stored as NULL in records.
    pub rowid_column: Option<usize>,
}

impl SqliteFile {
    pub fn open(path: &Path) -> Result<Self> {
        let journal =
            path.with_file_name(format!("{}-journal", path.file_name().unwrap_or_default().to_string_lossy()));
        if journal.exists() && std::fs::metadata(&journal)?.len() > 0 {
            bail!(
                "{} has a journal next to it; it may be mid-write. Copy it again while Moonraker is stopped.",
                path.display()
            );
        }
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        ensure!(
            data.len() >= 100 && &data[..16] == b"SQLite format 3\0",
            "{} isn't an SQLite database",
            path.display()
        );
        let page_size = match u16::from_be_bytes([data[16], data[17]]) {
            1 => 65536,
            n => n as usize,
        };
        ensure!(data[18] == 1 && data[19] == 1, "WAL databases aren't supported; checkpoint it first");
        ensure!(u32::from_be_bytes(data[56..60].try_into()?) <= 1, "only UTF-8 databases are supported");
        let usable = page_size - data[20] as usize;
        Ok(Self { data, page_size, usable })
    }

    fn page(&self, n: u32) -> Result<&[u8]> {
        let start = (n as usize - 1) * self.page_size;
        self.data.get(start..start + self.page_size).with_context(|| format!("page {n} is past the end of the file"))
    }

    /// Every `(rowid, record)` in the table b-tree rooted at `root`, in rowid order.
    pub fn rows(&self, root: u32) -> Result<Vec<(i64, Vec<Cell>)>> {
        let mut out = Vec::new();
        self.walk(root, &mut out, 0)?;
        Ok(out)
    }

    fn walk(&self, n: u32, out: &mut Vec<(i64, Vec<Cell>)>, depth: usize) -> Result<()> {
        ensure!(depth < 64, "b-tree too deep; the file may be damaged");
        let page = self.page(n)?;
        let hdr = if n == 1 { 100 } else { 0 };
        let kind = page[hdr];
        let cells = u16::from_be_bytes([page[hdr + 3], page[hdr + 4]]) as usize;
        let ptrs = hdr + if kind == 0x05 { 12 } else { 8 };
        for i in 0..cells {
            let off = u16::from_be_bytes([page[ptrs + 2 * i], page[ptrs + 2 * i + 1]]) as usize;
            match kind {
                0x05 => {
                    let child = u32::from_be_bytes(page[off..off + 4].try_into()?);
                    self.walk(child, out, depth + 1)?;
                }
                0x0D => {
                    let (size, a) = varint(&page[off..]);
                    let (rowid, b) = varint(&page[off + a..]);
                    let payload = self.payload(page, off + a + b, size as usize)?;
                    out.push((rowid as i64, record(&payload)?));
                }
                _ => bail!("page {n} has type {kind:#x}, not a table page"),
            }
        }
        if kind == 0x05 {
            let right = u32::from_be_bytes(page[hdr + 8..hdr + 12].try_into()?);
            self.walk(right, out, depth + 1)?;
        }
        Ok(())
    }

    /// Reads a leaf cell's payload, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        let local = if size <= x {
            size
        } else {
            let m = ((u - 12) * 32 / 255) - 23;
            let k = m + (size - m) % (u - 4);
            if k <= x { k } else { m }
        };
        let mut out = page[start..start + local].to_vec();
        if local < size {
            let mut next = u32::from_be_bytes(page[start + local..start + local + 4].try_into()?);
            while out.len() < size {
                ensure!(next != 0, "overflow chain ends early");
                let p = self.page(next)?;
                next = u32::from_be_bytes(p[..4].try_into()?);
                let take = (size - out.len()).min(u - 4);
                out.extend_from_slice(&p[4..4 + take]);
            }
        }
        Ok(out)
    }

    /// Tables from `sqlite_schema` (page 1).
    pub fn tables(&self) -> Result<Vec<Table>> {
        let mut tables = Vec::new();
        for (_, row) in self.rows(1)? {
            let (Some(Cell::Text(kind)), Some(Cell::Text(name)), Some(Cell::Int(root)), Some(Cell::Text(sql))) =
                (row.first(), row.get(1), row.get(3), row.get(4))
            else {
                continue;
            };
            if kind != "table" || name.starts_with("sqlite_") {
                continue;
            }
            let (columns, rowid_column) = parse_columns(sql)?;
            tables.push(Table { name: name.clone(), root_page: *root as u32, columns, rowid_column });
        }
        Ok(tables)
    }
}

/// Column names from a `CREATE TABLE` statement, and the index of an `INTEGER PRIMARY KEY` column.
fn parse_columns(sql: &str) -> Result<(Vec<String>, Option<usize>)> {
    let open = sql.find('(').context("CREATE TABLE without columns")?;
    let close = sql.rfind(')').context("CREATE TABLE without columns")?;
    let mut parts = Vec::new();
    let (mut depth, mut cur) = (0, String::new());
    for ch in sql[open + 1..close].chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    parts.push(cur);
    let mut columns = Vec::new();
    let mut rowid = None;
    for part in parts {
        let words: Vec<String> = part.split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
        let Some(first) = words.first() else { continue };
        if matches!(first.as_str(), "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "CONSTRAINT") {
            continue;
        }
        if words.get(1).map(String::as_str) == Some("INTEGER")
            && words.windows(2).any(|w| w[0] == "PRIMARY" && w[1] == "KEY")
        {
            rowid = Some(columns.len());
        }
        columns.push(part.split_whitespace().next().unwrap_or_default().trim_matches(['"', '`', '[', ']']).to_owned());
    }
    Ok((columns, rowid))
}

fn varint(b: &[u8]) -> (u64, usize) {
    let mut v = 0u64;
    for (i, &byte) in b.iter().enumerate().take(9) {
        if i == 8 {
            return ((v << 8) | byte as u64, 9);
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    (v, b.len())
}

fn record(p: &[u8]) -> Result<Vec<Cell>> {
    let (hdr_len, mut pos) = varint(p);
    let mut types = Vec::new();
    while pos < hdr_len as usize {
        let (t, n) = varint(&p[pos..]);
        types.push(t);
        pos += n;
    }
    let mut body = hdr_len as usize;
    let mut cells = Vec::with_capacity(types.len());
    for t in types {
        let int = |len: usize, body: usize| -> Result<i64> {
            let bytes = p.get(body..body + len).context("record is shorter than its header says")?;
            let mut v = if bytes[0] & 0x80 != 0 { -1i64 } else { 0 };
            for &b in bytes {
                v = (v << 8) | b as i64;
            }
            Ok(v)
        };
        let (cell, len) = match t {
            0 => (Cell::Null, 0),
            1..=4 => (Cell::Int(int(t as usize, body)?), t as usize),
            5 => (Cell::Int(int(6, body)?), 6),
            6 => (Cell::Int(int(8, body)?), 8),
            7 => (Cell::Real(f64::from_bits(int(8, body)? as u64)), 8),
            8 => (Cell::Int(0), 0),
            9 => (Cell::Int(1), 0),
            n if n >= 12 && n % 2 == 0 => {
                let len = (n as usize - 12) / 2;
                (Cell::Blob(p.get(body..body + len).context("blob past the record")?.to_vec()), len)
            }
            n if n >= 13 => {
                let len = (n as usize - 13) / 2;
                let bytes = p.get(body..body + len).context("text past the record")?;
                (Cell::Text(String::from_utf8_lossy(bytes).into_owned()), len)
            }
            n => bail!("unknown serial type {n}"),
        };
        cells.push(cell);
        body += len;
    }
    Ok(cells)
}

/// Decodes a `namespace_store.value` the way Moonraker's `database.py` encodes it: a type tag
/// byte and a native-endian (little-endian on Pi and x86) struct, or JSON for lists and dicts.
pub fn decode_moonraker_record(b: &[u8]) -> Result<Value> {
    let Some((&tag, rest)) = b.split_first() else { bail!("empty record") };
    Ok(match tag {
        b'q' => json!(i64::from_le_bytes(rest.try_into().context("bad int record")?)),
        b'd' => json!(f64::from_le_bytes(rest.try_into().context("bad float record")?)),
        b'?' => json!(rest.first().is_some_and(|&v| v != 0)),
        b's' => json!(String::from_utf8_lossy(rest)),
        b'[' | b'{' => serde_json::from_slice(b)?,
        0 => Value::Null,
        t => bail!("unknown record tag {t:#x}"),
    })
}

fn cell_json(c: &Cell) -> Value {
    match c {
        Cell::Null => Value::Null,
        Cell::Int(i) => json!(i),
        Cell::Real(f) => json!(f),
        Cell::Text(s) => json!(s),
        Cell::Blob(b) => json!({"$blob": b.iter().map(|x| format!("{x:02x}")).collect::<String>()}),
    }
}

pub fn dump(path: &Path) -> Result<()> {
    let file = SqliteFile::open(path)?;
    let mut tables = file.tables()?;
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    for t in tables {
        for (rowid, cells) in file.rows(t.root_page)? {
            let mut row = Map::new();
            for (i, name) in t.columns.iter().enumerate() {
                let v = match (t.rowid_column == Some(i), cells.get(i)) {
                    (true, _) => json!(rowid),
                    (false, Some(c)) => cell_json(c),
                    // Columns added by ALTER TABLE are missing from older records.
                    (false, None) => Value::Null,
                };
                row.insert(name.clone(), v);
            }
            if t.name == "namespace_store"
                && let Some(Cell::Blob(b)) = cells.get(2)
            {
                row.insert("value".into(), decode_moonraker_record(b)?);
            }
            println!("{}", json!({"table": t.name, "row": row}));
        }
    }
    Ok(())
}
