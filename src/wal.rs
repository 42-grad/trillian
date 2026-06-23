//! Write-Ahead-Log für **durable** Updates.
//!
//! Jede Änderung (neue Dictionary-Terme + Insert/Delete-Operationen) wird
//! append-only ins Log geschrieben und per `fsync` auf Platte gezwungen. Beim
//! Start wird das Log nach dem mmap-Snapshot zurückgespielt. Damit überleben
//! `INSERT/DELETE DATA`-Updates einen Absturz/Neustart, ohne nach jeder
//! Änderung den gesamten Snapshot neu zu schreiben.
//!
//! Record-Format (append-only, little-endian):
//! * `0x02` Term:   `[len:u32][utf8][type]`  → `dict.insert_with_type` (nächste ID)
//! * `0x00` Insert: `[s:u32][p:u32][o:u32]`
//! * `0x01` Delete: `[s:u32][p:u32][o:u32]`
//!
//! `type` ist: `0`=IRI, `1`=BlankNode, `2`=Literal, `3`=Literal+lang `[len][utf8]`,
//! `4`=Literal+datatype `[len][utf8]`.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};

use crate::hypertrie::{TermType, TripleStore};

pub struct Wal {
    writer: BufWriter<File>,
}

impl Wal {
    /// Öffnet das Log zum Anhängen (legt es bei Bedarf an).
    pub fn open_append(path: &str) -> std::io::Result<Wal> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Wal {
            writer: BufWriter::new(file),
        })
    }

    /// Loggt einen neu ins Dictionary aufgenommenen Term.
    pub fn log_term(&mut self, value: &str, typ: &TermType) -> std::io::Result<()> {
        self.writer.write_all(&[0x02])?;
        self.writer.write_all(&(value.len() as u32).to_le_bytes())?;
        self.writer.write_all(value.as_bytes())?;
        write_type(&mut self.writer, typ)
    }

    /// Loggt eine Insert- (`insert=true`) oder Delete-Operation.
    pub fn log_op(&mut self, insert: bool, s: u32, p: u32, o: u32) -> std::io::Result<()> {
        self.writer.write_all(&[if insert { 0x00 } else { 0x01 }])?;
        self.writer.write_all(&s.to_le_bytes())?;
        self.writer.write_all(&p.to_le_bytes())?;
        self.writer.write_all(&o.to_le_bytes())?;
        Ok(())
    }

    /// Erzwingt das Schreiben auf Platte (Durabilität).
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Spielt das Log auf einen (frisch geladenen) Store zurück. Liefert die
    /// Anzahl angewandter Operationen. Ein angerissener letzter Record (Crash
    /// mitten im Schreiben) wird ignoriert.
    pub fn replay(path: &str, store: &mut TripleStore) -> std::io::Result<usize> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(0), // kein Log -> nichts zu tun
        };
        let mut buf = Vec::new();
        BufReader::new(file).read_to_end(&mut buf)?;

        let mut p = 0usize;
        let mut applied = 0usize;
        while p < buf.len() {
            let tag = buf[p];
            p += 1;
            match tag {
                0x02 => {
                    let Some((value, typ, np)) = read_term(&buf, p) else {
                        break;
                    };
                    store.dict.insert_with_type(&value, typ);
                    p = np;
                }
                0x00 | 0x01 => {
                    if p + 12 > buf.len() {
                        break;
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
                _ => break,
            }
        }
        Ok(applied)
    }
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

/// Liest einen Term-Record ab Position `p` (nach dem Tag). `None` bei zu kurzem Puffer.
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

    /// Eindeutiger Temp-Pfad pro Testfall (keine `Date`/`rand`-APIs nötig).
    fn temp_path(tag: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("trillian_wal_{tag}_{n}.log"))
            .to_string_lossy()
            .into_owned()
    }

    /// Schreibt drei IRI-Terme (IDs 0,1,2) + ein Insert und prüft, dass der
    /// Replay in einen frischen Store dieselben IDs vergibt und das Triple setzt.
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
        let applied = Wal::replay(&path, &mut store).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(store.triple_count(), 1);
        assert_eq!(store.dict.lookup_iri("http://ex/s"), Some(0));
        assert_eq!(store.dict.lookup_iri("http://ex/o"), Some(2));

        let _ = std::fs::remove_file(&path);
    }

    /// Insert gefolgt von Delete desselben Triples -> Store wieder leer.
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
        let applied = Wal::replay(&path, &mut store).unwrap();
        assert_eq!(applied, 2); // ein Insert + ein Delete angewandt
        assert_eq!(store.triple_count(), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Der Typ (Sprach-Literal) muss den Replay verlustfrei überleben.
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
            .expect("Sprach-Literal nach Replay vorhanden");
        assert!(matches!(
            store.dict.resolve_type(id),
            Some(TermType::Literal { lang: Some(_), .. })
        ));

        let _ = std::fs::remove_file(&path);
    }

    /// Ein angerissener letzter Record (Crash beim Schreiben) wird ignoriert,
    /// die davor vollständig geschriebene Operation bleibt erhalten.
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
        // Halben Insert-Record anhängen: Tag + nur 4 statt 12 Byte.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0x00]).unwrap();
            f.write_all(&7u32.to_le_bytes()).unwrap();
            f.sync_data().unwrap();
        }

        let mut store = TripleStore::new();
        let applied = Wal::replay(&path, &mut store).unwrap();
        assert_eq!(applied, 1); // nur die vollständige Operation
        assert_eq!(store.triple_count(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
