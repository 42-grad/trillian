//! Write-ahead log for **durable** updates.
//!
//! Every change (new dictionary terms + insert/delete operations) is written
//! append-only to the log and forced to disk via `fsync`. On startup the log is
//! replayed after the mmap snapshot. This lets `INSERT/DELETE DATA` updates
//! survive a crash/restart without rewriting the entire snapshot after every
//! change.
//!
//! Record format (append-only, little-endian):
//! * `0x02` Term:   `[len:u32][utf8][type]`  → `dict.insert_with_type` (next ID)
//! * `0x00` Insert: `[s:u32][p:u32][o:u32]`
//! * `0x01` Delete: `[s:u32][p:u32][o:u32]`
//!
//! `type` is: `0`=IRI, `1`=BlankNode, `2`=Literal, `3`=Literal+lang `[len][utf8]`,
//! `4`=Literal+datatype `[len][utf8]`.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};

use crate::hypertrie::{TermType, TripleStore};

pub struct Wal {
    writer: BufWriter<File>,
}

impl Wal {
    /// Opens the log for appending (creating it if needed).
    pub fn open_append(path: &str) -> std::io::Result<Wal> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Wal {
            writer: BufWriter::new(file),
        })
    }

    /// Logs a term newly added to the dictionary.
    pub fn log_term(&mut self, value: &str, typ: &TermType) -> std::io::Result<()> {
        self.writer.write_all(&[0x02])?;
        self.writer.write_all(&(value.len() as u32).to_le_bytes())?;
        self.writer.write_all(value.as_bytes())?;
        write_type(&mut self.writer, typ)
    }

    /// Logs an insert (`insert=true`) or delete operation.
    pub fn log_op(&mut self, insert: bool, s: u32, p: u32, o: u32) -> std::io::Result<()> {
        self.writer.write_all(&[if insert { 0x00 } else { 0x01 }])?;
        self.writer.write_all(&s.to_le_bytes())?;
        self.writer.write_all(&p.to_le_bytes())?;
        self.writer.write_all(&o.to_le_bytes())?;
        Ok(())
    }

    /// Forces the write to disk (durability).
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Replays the log onto a (freshly loaded) store, then drops an unreadable
    /// tail so that whatever is appended next is still reachable on the next
    /// replay. A torn last record (crash mid-write) is what produces one.
    pub fn replay(path: &str, store: &mut TripleStore) -> std::io::Result<Replay> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(Replay::default()), // no log -> nothing to do
        };
        let mut buf = Vec::new();
        BufReader::new(file).read_to_end(&mut buf)?;

        let mut p = 0usize;
        let mut applied = 0usize;
        // Offset just past the last fully readable record.
        let valid = loop {
            if p >= buf.len() {
                break p;
            }
            let start = p;
            let tag = buf[p];
            p += 1;
            match tag {
                0x02 => {
                    let Some((value, typ, np)) = read_term(&buf, p) else {
                        break start;
                    };
                    store.dict.insert_with_type(&value, typ);
                    p = np;
                }
                0x00 | 0x01 => {
                    if p + 12 > buf.len() {
                        break start;
                    }
                    let s = rd_u32(&buf, p);
                    let pr = rd_u32(&buf, p + 4);
                    let o = rd_u32(&buf, p + 8);
                    p += 12;
                    if tag == 0x00 {
                        store.insert_triple(s, pr, o);
                    } else {
                        store.delete_triple(s, pr, o);
                    }
                    applied += 1;
                }
                _ => break start,
            }
        };

        // Without this, `open_append` writes past the bad record and every
        // later operation is silently lost on the next replay.
        let discarded = (buf.len() - valid) as u64;
        if discarded > 0 {
            OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(valid as u64)?;
        }
        Ok(Replay { applied, discarded })
    }
}

/// Outcome of a replay: operations applied, and bytes dropped from an
/// unreadable tail.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Replay {
    pub applied: usize,
    pub discarded: u64,
}

fn rd_u32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

fn write_type<W: Write>(w: &mut W, typ: &TermType) -> std::io::Result<()> {
    match typ {
        TermType::Iri => w.write_all(&[0]),
        TermType::BlankNode => w.write_all(&[1]),
        TermType::Literal {
            datatype: None,
            lang: None,
        } => w.write_all(&[2]),
        TermType::Literal { lang: Some(l), .. } => {
            w.write_all(&[3])?;
            w.write_all(&(l.len() as u32).to_le_bytes())?;
            w.write_all(l.as_bytes())
        }
        TermType::Literal {
            datatype: Some(d),
            lang: None,
        } => {
            w.write_all(&[4])?;
            w.write_all(&(d.len() as u32).to_le_bytes())?;
            w.write_all(d.as_bytes())
        }
    }
}

/// Reads a term record from position `p` (after the tag). `None` if the buffer is too short.
fn read_term(b: &[u8], mut p: usize) -> Option<(String, TermType, usize)> {
    if p + 4 > b.len() {
        return None;
    }
    let vlen = rd_u32(b, p) as usize;
    p += 4;
    if p + vlen + 1 > b.len() {
        return None;
    }
    let value = std::str::from_utf8(&b[p..p + vlen]).ok()?.to_string();
    p += vlen;
    let tag = b[p];
    p += 1;
    let typ = match tag {
        0 => TermType::Iri,
        1 => TermType::BlankNode,
        2 => TermType::literal_plain(),
        3 | 4 => {
            if p + 4 > b.len() {
                return None;
            }
            let alen = rd_u32(b, p) as usize;
            p += 4;
            if p + alen > b.len() {
                return None;
            }
            let aux = std::str::from_utf8(&b[p..p + alen]).ok()?.to_string();
            p += alen;
            if tag == 3 {
                TermType::literal_lang(aux)
            } else {
                TermType::literal_datatype(aux)
            }
        }
        _ => return None,
    };
    Some((value, typ, p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique temp path per test case (no `Date`/`rand` APIs needed).
    fn temp_path(tag: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("trillian_wal_{tag}_{n}.log"))
            .to_string_lossy()
            .into_owned();
        // The counter restarts per process, so a failed run leaves this behind.
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Writes three IRI terms (IDs 0,1,2) + one insert and checks that the
    /// replay into a fresh store assigns the same IDs and sets the triple.
    #[test]
    fn replays_terms_and_insert() {
        let path = temp_path("insert");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_term("http://ex/s", &TermType::Iri).unwrap();
            wal.log_term("http://ex/p", &TermType::Iri).unwrap();
            wal.log_term("http://ex/o", &TermType::Iri).unwrap();
            wal.log_op(true, 0, 1, 2).unwrap();
            wal.sync().unwrap();
        }

        let mut store = TripleStore::new();
        let applied = Wal::replay(&path, &mut store).unwrap().applied;
        assert_eq!(applied, 1);
        assert_eq!(store.triple_count(), 1);
        assert_eq!(store.dict.lookup_iri("http://ex/s"), Some(0));
        assert_eq!(store.dict.lookup_iri("http://ex/o"), Some(2));

        let _ = std::fs::remove_file(&path);
    }

    /// Insert followed by delete of the same triple -> store empty again.
    #[test]
    fn replays_delete_after_insert() {
        let path = temp_path("delete");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_term("a", &TermType::Iri).unwrap();
            wal.log_term("b", &TermType::Iri).unwrap();
            wal.log_term("c", &TermType::Iri).unwrap();
            wal.log_op(true, 0, 1, 2).unwrap();
            wal.log_op(false, 0, 1, 2).unwrap();
            wal.sync().unwrap();
        }

        let mut store = TripleStore::new();
        let applied = Wal::replay(&path, &mut store).unwrap().applied;
        assert_eq!(applied, 2); // one insert + one delete applied
        assert_eq!(store.triple_count(), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// The type (language literal) must survive the replay losslessly.
    #[test]
    fn replays_typed_literal_term() {
        let path = temp_path("typed");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_term("Alice", &TermType::literal_lang("en"))
                .unwrap();
            wal.sync().unwrap();
        }

        let mut store = TripleStore::new();
        Wal::replay(&path, &mut store).unwrap();
        let id = store
            .dict
            .lookup_term("Alice", &TermType::literal_lang("en"))
            .expect("language literal present after replay");
        assert!(matches!(
            store.dict.resolve_type(id),
            Some(TermType::Literal { lang: Some(_), .. })
        ));

        let _ = std::fs::remove_file(&path);
    }

    /// A torn last record (crash while writing) is ignored; the fully written
    /// operation before it is preserved.
    #[test]
    fn ignores_truncated_tail() {
        use std::io::Write;
        let path = temp_path("truncated");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_term("a", &TermType::Iri).unwrap();
            wal.log_term("b", &TermType::Iri).unwrap();
            wal.log_term("c", &TermType::Iri).unwrap();
            wal.log_op(true, 0, 1, 2).unwrap();
            wal.sync().unwrap();
        }
        // Append half an insert record: tag + only 4 instead of 12 bytes.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0x00]).unwrap();
            f.write_all(&7u32.to_le_bytes()).unwrap();
            f.sync_data().unwrap();
        }

        let mut store = TripleStore::new();
        let applied = Wal::replay(&path, &mut store).unwrap().applied;
        assert_eq!(applied, 1); // only the complete operation
        assert_eq!(store.triple_count(), 1);

        let _ = std::fs::remove_file(&path);
    }

    /// The replay drops the torn tail, so an operation appended afterwards is
    /// still reachable. Without the truncation the replay stops at the bad
    /// record and every later update is silently lost.
    #[test]
    fn an_op_appended_after_a_torn_record_survives() {
        use std::io::Write;
        let path = temp_path("torn_then_append");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_term("a", &TermType::Iri).unwrap();
            wal.log_term("b", &TermType::Iri).unwrap();
            wal.log_term("c", &TermType::Iri).unwrap();
            wal.sync().unwrap();
        }
        // Crash mid-record: a term tag with only part of its header.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0x02, 0x03, 0x00]).unwrap();
            f.sync_data().unwrap();
        }

        // Restart: replay repairs the tail, then the server appends to it.
        let mut store = TripleStore::new();
        let first = Wal::replay(&path, &mut store).unwrap();
        assert_eq!(first.discarded, 3, "the torn record should be dropped");
        {
            let mut wal = Wal::open_append(&path).unwrap();
            wal.log_op(true, 0, 1, 2).unwrap();
            wal.sync().unwrap();
        }

        // Next restart: the appended operation must still be applied.
        let mut store = TripleStore::new();
        let second = Wal::replay(&path, &mut store).unwrap();
        assert_eq!(second.applied, 1, "the op after the torn record was lost");
        assert_eq!(second.discarded, 0);
        assert_eq!(store.triple_count(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
