//! The recorder export: a CSV (optionally gzip) without header, one state row per line:
//! `entity_id,state,last_updated_ts,shared_attrs`. `last_updated_ts` is epoch seconds (float);
//! `shared_attrs` is a JSON object or empty. Fields may be quoted (`"…"`, `""` for a quote) and
//! a quoted field may span lines. Rows must be in time order.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub entity: String,
    pub state: String,
    /// `last_updated_ts`, epoch seconds.
    pub ts: f64,
    /// `shared_attrs`, when the export has them for this entity.
    pub attrs: Option<String>,
}

/// Streams [`Row`]s from a reader.
pub struct Rows<R> {
    r: R,
    line: u64,
    buf: String,
}

impl<R: BufRead> Rows<R> {
    pub fn new(r: R) -> Self {
        Rows { r, line: 0, buf: String::new() }
    }

    /// The next record's fields, or `None` at the end.
    fn record(&mut self) -> Result<Option<Vec<String>>> {
        let mut fields = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        loop {
            self.buf.clear();
            if self.r.read_line(&mut self.buf)? == 0 {
                if quoted {
                    return Err(Error::new(format!("line {}: unterminated quote", self.line)));
                }
                return Ok(None);
            }
            self.line += 1;
            let line = self.buf.trim_end_matches(['\n', '\r']);
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                match (quoted, c) {
                    (true, '"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        cur.push('"');
                    }
                    (true, '"') => quoted = false,
                    (false, '"') => quoted = true,
                    (false, ',') => fields.push(std::mem::take(&mut cur)),
                    (_, c) => cur.push(c),
                }
            }
            if quoted {
                cur.push('\n');
                continue;
            }
            if fields.is_empty() && cur.is_empty() {
                continue; // blank line
            }
            fields.push(cur);
            return Ok(Some(fields));
        }
    }

    fn row(&mut self) -> Result<Option<Row>> {
        let Some(f) = self.record()? else { return Ok(None) };
        let bad = |what: &str| Error::new(format!("line {}: {what}", self.line));
        let [entity, state, ts, attrs]: [String; 4] = f.try_into().map_err(|_| bad("expected 4 fields"))?;
        let ts = ts.trim().parse::<f64>().ok().filter(|t| t.is_finite()).ok_or_else(|| bad("bad timestamp"))?;
        Ok(Some(Row { entity, state, ts, attrs: (!attrs.is_empty()).then_some(attrs) }))
    }
}

impl<R: BufRead> Iterator for Rows<R> {
    type Item = Result<Row>;
    fn next(&mut self) -> Option<Result<Row>> {
        self.row().transpose()
    }
}

/// Open an export, gzip or plain (told apart by the gzip magic bytes).
pub fn open(path: &Path) -> Result<Rows<Box<dyn BufRead>>> {
    let err = |e: std::io::Error| Error::new(format!("{}: {e}", path.display()));
    let mut f = BufReader::new(File::open(path).map_err(err)?);
    let gz = f.fill_buf().map_err(err)?.starts_with(&[0x1f, 0x8b]);
    let r: Box<dyn BufRead> = if gz {
        Box::new(BufReader::with_capacity(1 << 16, flate2::bufread::MultiGzDecoder::new(f)))
    } else {
        Box::new(f)
    };
    Ok(Rows::new(r))
}

/// Read all rows (tests and small files).
pub fn read_all(r: impl Read) -> Result<Vec<Row>> {
    Rows::new(BufReader::new(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Plain and quoted fields, a doubled quote, a JSON attribute with commas, a quoted field
    /// across two lines, blank lines and CRLF endings.
    #[test]
    fn parses_the_export_format() {
        let csv = "sensor.a,1.5,1793660400.25,\r\n\n\"input_text.x\",\"a \"\"b\"\", c\",1793660401,\n\
                   number.o,15.2,1793660402.5,\"{\"\"max\"\": 29, \"\"min\"\": 6}\"\n\
                   input_text.y,\"two\nlines\",1793660403,\n";
        let rows = read_all(csv.as_bytes()).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], Row { entity: "sensor.a".into(), state: "1.5".into(), ts: 1793660400.25, attrs: None });
        assert_eq!(rows[1].state, "a \"b\", c");
        assert_eq!(rows[2].attrs.as_deref(), Some("{\"max\": 29, \"min\": 6}"));
        assert_eq!(rows[3].state, "two\nlines");
    }

    /// Malformed rows are errors with their line number.
    #[test]
    fn malformed_rows_are_errors() {
        assert_eq!(read_all("a,b,c\n".as_bytes()).unwrap_err().0, "line 1: expected 4 fields");
        assert_eq!(read_all("a,b,x,\n".as_bytes()).unwrap_err().0, "line 1: bad timestamp");
        assert_eq!(read_all("a,b,inf,\n".as_bytes()).unwrap_err().0, "line 1: bad timestamp");
        assert_eq!(read_all("a,\"b,1,\n".as_bytes()).unwrap_err().0, "line 1: unterminated quote");
    }

    /// `open` reads gzip and plain files alike, and reports a missing file.
    #[test]
    fn opens_gzip_and_plain() {
        let d = tempfile::tempdir().unwrap();
        let plain = d.path().join("h.csv");
        std::fs::write(&plain, "sensor.a,1,10,\n").unwrap();
        let gz = d.path().join("h.csv.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"sensor.a,1,10,\nsensor.b,2,11,\n").unwrap();
        std::fs::write(&gz, enc.finish().unwrap()).unwrap();
        assert_eq!(open(&plain).unwrap().count(), 1);
        let rows: Vec<Row> = open(&gz).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(rows[1].entity, "sensor.b");
        assert!(open(&d.path().join("nope")).is_err());
    }
}
