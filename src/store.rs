//! SQLite-backed index store. Every indexer (SCIP ingest, tree-sitter fallback)
//! writes through this API; the CLI, UI server and MCP server read through it.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  language TEXT NOT NULL,
  hash TEXT NOT NULL,
  approx INTEGER NOT NULL DEFAULT 0,
  indexer TEXT NOT NULL,
  text TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols(
  id INTEGER PRIMARY KEY,
  scip TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  parent INTEGER,
  def_file INTEGER,
  def_line INTEGER,
  def_col INTEGER,
  signature TEXT,
  doc TEXT,
  approx INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS symbols_parent ON symbols(parent);
CREATE TABLE IF NOT EXISTS occurrences(
  file_id INTEGER NOT NULL,
  symbol_id INTEGER NOT NULL,
  line INTEGER NOT NULL,
  col INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  end_col INTEGER NOT NULL,
  roles INTEGER NOT NULL,
  enclosing INTEGER
);
CREATE INDEX IF NOT EXISTS occ_file ON occurrences(file_id);
CREATE INDEX IF NOT EXISTS occ_sym ON occurrences(symbol_id);
CREATE TABLE IF NOT EXISTS edges(
  src INTEGER NOT NULL,
  dst INTEGER NOT NULL,
  kind TEXT NOT NULL,
  file_id INTEGER NOT NULL,
  line INTEGER NOT NULL,
  count INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY(src, dst, kind, file_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst, kind);
CREATE TABLE IF NOT EXISTS bookmarks(scip TEXT PRIMARY KEY, note TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS trails(name TEXT PRIMARY KEY, steps TEXT NOT NULL, updated INTEGER NOT NULL);
"#;

/// SCIP symbol role bit for definitions.
pub const ROLE_DEFINITION: i64 = 1;

/// Edge kinds stored in `edges.kind`.
pub const EDGE_CALL: &str = "call";
pub const EDGE_REF: &str = "ref";
pub const EDGE_INHERIT: &str = "inherit";

pub struct Store {
    conn: Connection,
}

/// A symbol to insert or update. `scip` is the unique key (a SCIP symbol string,
/// or a synthetic `approx <path> <name>` string for tree-sitter symbols).
#[derive(Debug, Clone, Default)]
pub struct NewSymbol {
    pub scip: String,
    pub name: String,
    pub kind: String,
    pub parent_scip: Option<String>,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub approx: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub id: i64,
    pub scip: String,
    pub name: String,
    pub kind: String,
    pub parent: Option<i64>,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub col: Option<i64>,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub approx: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub file: String,
    pub line: i64,
    pub col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub is_def: bool,
    /// Trimmed source line, for listing.
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub symbol: Symbol,
    pub kind: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Neighborhood {
    pub focus: Symbol,
    pub parent: Option<Symbol>,
    pub members: Vec<Symbol>,
    /// Symbols that call or reference the focus (and members, for types).
    pub incoming: Vec<Edge>,
    /// Symbols called or referenced by the focus.
    pub outgoing: Vec<Edge>,
    pub bases: Vec<Symbol>,
    pub derived: Vec<Symbol>,
    /// Totals before truncation, so the UI can show "+N more".
    pub incoming_total: i64,
    pub outgoing_total: i64,
    pub members_total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOccurrence {
    pub symbol_id: i64,
    pub line: i64,
    pub col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub is_def: bool,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub path: String,
    pub language: String,
    pub approx: bool,
    pub text: String,
    pub occurrences: Vec<FileOccurrence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextHit {
    pub file: String,
    pub line: i64,
    pub col: i64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub language: String,
    pub approx: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub files: i64,
    pub approx_files: i64,
    pub symbols: i64,
    pub occurrences: i64,
    pub edges: i64,
    pub languages: Vec<(String, i64)>,
}

const SYMBOL_COLS: &str = "s.id, s.scip, s.name, s.kind, s.parent, f.path, s.def_line, s.def_col, s.signature, s.doc, s.approx";

fn row_to_symbol(r: &rusqlite::Row) -> rusqlite::Result<Symbol> {
    Ok(Symbol {
        id: r.get(0)?,
        scip: r.get(1)?,
        name: r.get(2)?,
        kind: r.get(3)?,
        parent: r.get(4)?,
        file: r.get(5)?,
        line: r.get(6)?,
        col: r.get(7)?,
        signature: r.get(8)?,
        doc: r.get(9)?,
        approx: r.get::<_, i64>(10)? != 0,
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Content hash used to skip unchanged files on re-index.
pub fn hash_text(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening index {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF; PRAGMA busy_timeout=5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            anyhow::bail!(
                "index was created by an incompatible Cairnscope version (schema {version}, expected {SCHEMA_VERSION}); delete the .cairnscope directory and re-run `cairnscope index`"
            );
        }
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION}"))?;
        Ok(Store { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run `f` inside one transaction (huge speedup for bulk inserts).
    pub fn transaction<T>(&mut self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    // ---------- writes ----------

    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT hash FROM files WHERE path=?1", [path], |r| r.get(0))
            .optional()?)
    }

    /// Insert or replace a file. Clears that file's previous occurrences and edges,
    /// so callers re-add them. Returns the file id.
    pub fn upsert_file(
        &self,
        path: &str,
        language: &str,
        text: &str,
        approx: bool,
        indexer: &str,
    ) -> Result<i64> {
        let hash = hash_text(text);
        let existing: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE path=?1", [path], |r| r.get(0))
            .optional()?;
        let id = match existing {
            Some(id) => {
                self.clear_file_data(id)?;
                self.conn.execute(
                    "UPDATE files SET language=?2, hash=?3, approx=?4, indexer=?5, text=?6 WHERE id=?1",
                    params![id, language, hash, approx as i64, indexer, text],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO files(path, language, hash, approx, indexer, text) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![path, language, hash, approx as i64, indexer, text],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        Ok(id)
    }

    fn clear_file_data(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM occurrences WHERE file_id=?1", [file_id])?;
        self.conn
            .execute("DELETE FROM edges WHERE file_id=?1", [file_id])?;
        self.conn.execute(
            "UPDATE symbols SET def_file=NULL, def_line=NULL, def_col=NULL WHERE def_file=?1",
            [file_id],
        )?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        if let Some(id) = self
            .conn
            .query_row("SELECT id FROM files WHERE path=?1", [path], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            self.clear_file_data(id)?;
            self.conn.execute("DELETE FROM files WHERE id=?1", [id])?;
        }
        Ok(())
    }

    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM files")?;
        let v = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Insert or update a symbol; returns its id. Existing non-empty fields are
    /// only overwritten by non-empty new values.
    pub fn upsert_symbol(&self, s: &NewSymbol) -> Result<i64> {
        let parent = match &s.parent_scip {
            Some(p) if p != &s.scip => Some(self.ensure_symbol(p)?),
            _ => None,
        };
        self.conn.execute(
            "INSERT INTO symbols(scip, name, kind, parent, signature, doc, approx) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(scip) DO UPDATE SET
               name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE name END,
               kind = CASE WHEN excluded.kind <> 'unknown' THEN excluded.kind ELSE kind END,
               parent = COALESCE(excluded.parent, parent),
               signature = COALESCE(excluded.signature, signature),
               doc = COALESCE(excluded.doc, doc),
               approx = excluded.approx",
            params![s.scip, s.name, s.kind, parent, s.signature, s.doc, s.approx as i64],
        )?;
        self.symbol_id(&s.scip).map(|o| o.expect("just inserted"))
    }

    /// Get the id for a SCIP symbol string, creating a placeholder row if needed.
    pub fn ensure_symbol(&self, scip: &str) -> Result<i64> {
        if let Some(id) = self.symbol_id(scip)? {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO symbols(scip, name, kind) VALUES(?1, ?2, 'unknown')",
            params![scip, crate::scip_symbol::display_name(scip)],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn symbol_id(&self, scip: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM symbols WHERE scip=?1", [scip], |r| r.get(0))
            .optional()?)
    }

    pub fn set_definition(&self, symbol_id: i64, file_id: i64, line: i64, col: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE symbols SET def_file=?2, def_line=?3, def_col=?4 WHERE id=?1",
            params![symbol_id, file_id, line, col],
        )?;
        Ok(())
    }

    /// Lines and columns are 0-based. `enclosing` is the symbol whose body contains this occurrence.
    #[allow(clippy::too_many_arguments)]
    pub fn add_occurrence(
        &self,
        file_id: i64,
        symbol_id: i64,
        line: i64,
        col: i64,
        end_line: i64,
        end_col: i64,
        roles: i64,
        enclosing: Option<i64>,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO occurrences(file_id, symbol_id, line, col, end_line, end_col, roles, enclosing) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?
            .execute(params![file_id, symbol_id, line, col, end_line, end_col, roles, enclosing])?;
        Ok(())
    }

    /// Record `src -> dst` of `kind`, attributed to `file_id` (so it is cleared when that file is re-indexed).
    pub fn add_edge(&self, src: i64, dst: i64, kind: &str, file_id: i64, line: i64) -> Result<()> {
        if src == dst {
            return Ok(());
        }
        self.conn
            .prepare_cached(
                "INSERT INTO edges(src, dst, kind, file_id, line) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(src, dst, kind, file_id) DO UPDATE SET count = count + 1",
            )?
            .execute(params![src, dst, kind, file_id, line])?;
        Ok(())
    }

    /// Delete symbols that no longer have a definition, occurrence, edge or child.
    pub fn gc_symbols(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM symbols WHERE def_file IS NULL
               AND NOT EXISTS (SELECT 1 FROM occurrences o WHERE o.symbol_id = symbols.id)
               AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.src = symbols.id OR e.dst = symbols.id)
               AND NOT EXISTS (SELECT 1 FROM symbols c WHERE c.parent = symbols.id)",
            [],
        )?)
    }

    // ---------- reads ----------

    pub fn stats(&self) -> Result<Stats> {
        let c = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let mut st = self
            .conn
            .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY 2 DESC")?;
        let languages = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Stats {
            files: c("SELECT COUNT(*) FROM files")?,
            approx_files: c("SELECT COUNT(*) FROM files WHERE approx=1")?,
            symbols: c("SELECT COUNT(*) FROM symbols WHERE def_file IS NOT NULL")?,
            occurrences: c("SELECT COUNT(*) FROM occurrences")?,
            edges: c("SELECT COUNT(*) FROM edges")?,
            languages,
        })
    }

    pub fn symbol(&self, id: i64) -> Result<Option<Symbol>> {
        let sql = format!(
            "SELECT {SYMBOL_COLS} FROM symbols s LEFT JOIN files f ON f.id = s.def_file WHERE s.id=?1"
        );
        Ok(self.conn.query_row(&sql, [id], row_to_symbol).optional()?)
    }

    /// Ranked symbol search: exact name, then prefix, then substring; defined symbols first.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Result<Vec<Symbol>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        let like = format!("%{}%", escape_like(q));
        let prefix = format!("{}%", escape_like(q));
        let sql = format!(
            "SELECT {SYMBOL_COLS} FROM symbols s LEFT JOIN files f ON f.id = s.def_file
             WHERE s.name LIKE ?1 ESCAPE '\\' AND s.kind NOT IN ('parameter','local','unknown')
             ORDER BY (s.def_file IS NULL),
                      CASE WHEN s.name = ?2 THEN 0 WHEN s.name LIKE ?3 ESCAPE '\\' THEN 1 ELSE 2 END,
                      length(s.name), s.name
             LIMIT ?4"
        );
        let mut st = self.conn.prepare(&sql)?;
        let v = st
            .query_map(params![like, q, prefix, limit as i64], row_to_symbol)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn search_files(&self, query: &str, limit: usize) -> Result<Vec<FileEntry>> {
        let like = format!("%{}%", escape_like(query.trim()));
        let mut st = self.conn.prepare(
            "SELECT path, language, approx FROM files WHERE path LIKE ?1 ESCAPE '\\' ORDER BY length(path), path LIMIT ?2",
        )?;
        let v = st
            .query_map(params![like, limit as i64], |r| {
                Ok(FileEntry {
                    path: r.get(0)?,
                    language: r.get(1)?,
                    approx: r.get::<_, i64>(2)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Case-sensitive substring search over indexed file contents.
    // shortcut: linear scan over stored text (fine to ~100MB of source); switch to an FTS5 trigram index if it gets slow.
    pub fn grep(&self, needle: &str, limit: usize) -> Result<Vec<TextHit>> {
        let mut hits = Vec::new();
        if needle.is_empty() {
            return Ok(hits);
        }
        let mut st = self
            .conn
            .prepare("SELECT path, text FROM files WHERE instr(text, ?1) > 0 ORDER BY path")?;
        let mut rows = st.query([needle])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let text: String = r.get(1)?;
            for (i, l) in text.lines().enumerate() {
                if let Some(col) = l.find(needle) {
                    hits.push(TextHit {
                        file: path.clone(),
                        line: i as i64,
                        col: l[..col].chars().count() as i64,
                        snippet: snippet(l),
                    });
                    if hits.len() >= limit {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }

    pub fn file(&self, path: &str) -> Result<Option<FileView>> {
        let row: Option<(i64, String, i64, String)> = self
            .conn
            .query_row(
                "SELECT id, language, approx, text FROM files WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, language, approx, text)) = row else {
            return Ok(None);
        };
        let mut st = self.conn.prepare(
            "SELECT o.symbol_id, o.line, o.col, o.end_line, o.end_col, o.roles, s.kind
             FROM occurrences o JOIN symbols s ON s.id = o.symbol_id
             WHERE o.file_id=?1 ORDER BY o.line, o.col",
        )?;
        let occurrences = st
            .query_map([id], |r| {
                Ok(FileOccurrence {
                    symbol_id: r.get(0)?,
                    line: r.get(1)?,
                    col: r.get(2)?,
                    end_line: r.get(3)?,
                    end_col: r.get(4)?,
                    is_def: r.get::<_, i64>(5)? & ROLE_DEFINITION != 0,
                    kind: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(FileView {
            path: path.to_string(),
            language,
            approx: approx != 0,
            text,
            occurrences,
        }))
    }

    /// All files, for the file tree.
    pub fn files(&self) -> Result<Vec<FileEntry>> {
        self.search_files("", i64::MAX as usize)
    }

    /// Symbols defined in a file, in source order (file outline).
    pub fn file_symbols(&self, path: &str) -> Result<Vec<Symbol>> {
        let sql = format!(
            "SELECT {SYMBOL_COLS} FROM symbols s JOIN files f ON f.id = s.def_file
             WHERE f.path=?1 AND s.kind NOT IN ('parameter','local') ORDER BY s.def_line, s.def_col"
        );
        let mut st = self.conn.prepare(&sql)?;
        let v = st
            .query_map([path], row_to_symbol)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn references(&self, symbol_id: i64, limit: usize) -> Result<Vec<Location>> {
        let mut st = self.conn.prepare(
            "SELECT f.path, o.line, o.col, o.end_line, o.end_col, o.roles, f.text
             FROM occurrences o JOIN files f ON f.id = o.file_id
             WHERE o.symbol_id=?1 ORDER BY (o.roles & 1) DESC, f.path, o.line LIMIT ?2",
        )?;
        let v = st
            .query_map(params![symbol_id, limit as i64], |r| {
                let text: String = r.get(6)?;
                let line: i64 = r.get(1)?;
                Ok(Location {
                    file: r.get(0)?,
                    line,
                    col: r.get(2)?,
                    end_line: r.get(3)?,
                    end_col: r.get(4)?,
                    is_def: r.get::<_, i64>(5)? & ROLE_DEFINITION != 0,
                    snippet: text
                        .lines()
                        .nth(line as usize)
                        .map(snippet)
                        .unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Symbols that have an edge into `id` (callers when kind = call).
    pub fn incoming(&self, id: i64, kinds: &[&str], limit: usize) -> Result<(Vec<Edge>, i64)> {
        self.edges_for(id, kinds, limit, true)
    }

    /// Symbols `id` has an edge to (callees when kind = call).
    pub fn outgoing(&self, id: i64, kinds: &[&str], limit: usize) -> Result<(Vec<Edge>, i64)> {
        self.edges_for(id, kinds, limit, false)
    }

    fn edges_for(
        &self,
        id: i64,
        kinds: &[&str],
        limit: usize,
        incoming: bool,
    ) -> Result<(Vec<Edge>, i64)> {
        let (me, other) = if incoming {
            ("dst", "src")
        } else {
            ("src", "dst")
        };
        let kinds_sql = kinds
            .iter()
            .map(|k| format!("'{}'", k.replace('\'', "")))
            .collect::<Vec<_>>()
            .join(",");
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT {other}) FROM edges WHERE {me}=?1 AND kind IN ({kinds_sql})"
            ),
            [id],
            |r| r.get(0),
        )?;
        // Prefer call edges, then the most frequent, then defined symbols.
        let sql = format!(
            "SELECT {SYMBOL_COLS}, MIN(e.kind) AS k, SUM(e.count) AS n
             FROM edges e JOIN symbols s ON s.id = e.{other} LEFT JOIN files f ON f.id = s.def_file
             WHERE e.{me}=?1 AND e.kind IN ({kinds_sql})
             GROUP BY s.id ORDER BY (k <> 'call'), (s.def_file IS NULL), n DESC, s.name LIMIT ?2"
        );
        let mut st = self.conn.prepare(&sql)?;
        let v = st
            .query_map(params![id, limit as i64], |r| {
                Ok(Edge {
                    symbol: row_to_symbol(r)?,
                    kind: r.get(11)?,
                    count: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((v, total))
    }

    pub fn members(&self, id: i64, limit: usize) -> Result<(Vec<Symbol>, i64)> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE parent=?1 AND kind NOT IN ('parameter','local','unknown','type_parameter')",
            [id],
            |r| r.get(0),
        )?;
        let sql = format!(
            "SELECT {SYMBOL_COLS} FROM symbols s LEFT JOIN files f ON f.id = s.def_file
             WHERE s.parent=?1 AND s.kind NOT IN ('parameter','local','unknown','type_parameter')
             ORDER BY (s.def_file IS NULL), s.def_line, s.name LIMIT ?2"
        );
        let mut st = self.conn.prepare(&sql)?;
        let v = st
            .query_map(params![id, limit as i64], row_to_symbol)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((v, total))
    }

    /// Everything the graph view needs around one symbol, each list capped at `limit`.
    pub fn neighborhood(&self, id: i64, limit: usize) -> Result<Option<Neighborhood>> {
        let Some(focus) = self.symbol(id)? else {
            return Ok(None);
        };
        let parent = match focus.parent {
            Some(p) => self.symbol(p)?,
            None => None,
        };
        let (members, members_total) = self.members(id, limit)?;
        let (incoming, incoming_total) = self.incoming(id, &[EDGE_CALL, EDGE_REF], limit)?;
        let (outgoing, outgoing_total) = self.outgoing(id, &[EDGE_CALL, EDGE_REF], limit)?;
        let bases = self
            .outgoing(id, &[EDGE_INHERIT], limit)?
            .0
            .into_iter()
            .map(|e| e.symbol)
            .collect();
        let derived = self
            .incoming(id, &[EDGE_INHERIT], limit)?
            .0
            .into_iter()
            .map(|e| e.symbol)
            .collect();
        Ok(Some(Neighborhood {
            focus,
            parent,
            members,
            incoming,
            outgoing,
            bases,
            derived,
            incoming_total,
            outgoing_total,
            members_total,
        }))
    }

    /// Innermost symbol defined at or enclosing `line` in `path` (used for click-in-code -> graph).
    pub fn symbol_at(&self, path: &str, line: i64, col: i64) -> Result<Option<Symbol>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT o.symbol_id FROM occurrences o JOIN files f ON f.id = o.file_id
                 WHERE f.path=?1 AND o.line=?2 AND o.col<=?3 AND (o.end_line>?2 OR o.end_col>=?3)
                 ORDER BY o.col DESC LIMIT 1",
                params![path, line, col],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.symbol(id),
            None => Ok(None),
        }
    }

    // ---------- bookmarks & trails ----------

    pub fn add_bookmark(&self, scip: &str, note: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO bookmarks(scip, note, created) VALUES(?1,?2,?3)",
            params![scip, note, now()],
        )?;
        Ok(())
    }

    pub fn remove_bookmark(&self, scip: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM bookmarks WHERE scip=?1", [scip])?;
        Ok(())
    }

    /// Bookmarks with their current symbol (None if the symbol vanished after re-indexing).
    pub fn bookmarks(&self) -> Result<Vec<(String, String, Option<Symbol>)>> {
        let mut st = self
            .conn
            .prepare("SELECT scip, note FROM bookmarks ORDER BY created")?;
        let rows: Vec<(String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(scip, note)| {
                let sym = match self.symbol_id(&scip)? {
                    Some(id) => self.symbol(id)?,
                    None => None,
                };
                Ok((scip, note, sym))
            })
            .collect()
    }

    /// `steps` is a JSON array of SCIP symbol strings.
    pub fn save_trail(&self, name: &str, steps: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO trails(name, steps, updated) VALUES(?1,?2,?3)",
            params![name, steps, now()],
        )?;
        Ok(())
    }

    pub fn delete_trail(&self, name: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM trails WHERE name=?1", [name])?;
        Ok(())
    }

    pub fn trails(&self) -> Result<Vec<(String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT name, steps FROM trails ORDER BY updated DESC")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn snippet(line: &str) -> String {
    let t = line.trim();
    if t.chars().count() > 200 {
        t.chars().take(200).collect::<String>() + "…"
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(scip: &str, name: &str, kind: &str, parent: Option<&str>) -> NewSymbol {
        NewSymbol {
            scip: scip.into(),
            name: name.into(),
            kind: kind.into(),
            parent_scip: parent.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_graph_queries() -> Result<()> {
        let mut s = Store::open_in_memory()?;
        s.transaction(|s| {
            let f = s.upsert_file(
                "a.ts",
                "typescript",
                "class A {\n  run() { helper(); }\n}\nfunction helper() {}\n",
                false,
                "test",
            )?;
            let a = s.upsert_symbol(&sym("t . a/A#", "A", "class", None))?;
            let run = s.upsert_symbol(&sym("t . a/A#run().", "run", "method", Some("t . a/A#")))?;
            let helper = s.upsert_symbol(&sym("t . a/helper().", "helper", "function", None))?;
            s.set_definition(a, f, 0, 6)?;
            s.set_definition(run, f, 1, 2)?;
            s.set_definition(helper, f, 3, 9)?;
            s.add_occurrence(f, helper, 1, 10, 1, 16, 0, Some(run))?;
            s.add_occurrence(f, helper, 3, 9, 3, 15, ROLE_DEFINITION, None)?;
            s.add_edge(run, helper, EDGE_CALL, f, 1)?;
            s.add_edge(run, helper, EDGE_CALL, f, 1)?;
            Ok(())
        })?;
        let hits = s.search_symbols("help", 10)?;
        assert_eq!(hits[0].name, "helper");
        let helper = hits[0].id;
        let n = s.neighborhood(helper, 10)?.unwrap();
        assert_eq!(n.incoming.len(), 1);
        assert_eq!(n.incoming[0].symbol.name, "run");
        assert_eq!(n.incoming[0].count, 2);
        let run = n.incoming[0].symbol.id;
        let a = s
            .neighborhood(s.symbol_id("t . a/A#")?.unwrap(), 10)?
            .unwrap();
        assert_eq!(a.members[0].id, run);
        assert_eq!(s.references(helper, 10)?.len(), 2);
        assert_eq!(s.grep("helper()", 10)?.len(), 2);
        assert_eq!(s.symbol_at("a.ts", 1, 12)?.unwrap().name, "helper");

        // Re-indexing the file clears its edges; gc removes orphans.
        s.upsert_file("a.ts", "typescript", "// empty\n", false, "test")?;
        assert!(s.neighborhood(helper, 10)?.unwrap().incoming.is_empty());
        s.conn()
            .execute("DELETE FROM symbols WHERE parent IS NOT NULL", [])?;
        assert!(s.gc_symbols()? >= 1);
        Ok(())
    }

    #[test]
    fn like_wildcards_are_literal() -> Result<()> {
        let s = Store::open_in_memory()?;
        s.upsert_symbol(&sym("x", "a_b", "function", None))?;
        s.upsert_symbol(&sym("y", "axb", "function", None))?;
        assert_eq!(s.search_symbols("a_b", 10)?.len(), 1);
        Ok(())
    }
}
