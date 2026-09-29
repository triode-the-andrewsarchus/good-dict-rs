use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{params, Connection};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "dict",
    about = "Look up a word in Webster's 1913 — the good dictionary.\n\
             On first use the dictionary is downloaded and indexed (~14 MB).",
    after_help = "Based on the GCIDE (GNU Collaborative International Dictionary of English),\n\
                  derived from Webster's Revised Unabridged Dictionary, 1913 edition.\n\
                  As championed by James Somers: https://jsomers.net/blog/dictionary"
)]
struct Cli {
    /// Word(s) to look up
    #[arg(required = true)]
    words: Vec<String>,

    /// Show etymology
    #[arg(short, long, default_value_t = true)]
    etymology: bool,

    /// Show example quotations
    #[arg(short = 'q', long = "quotes", default_value_t = true)]
    quotes: bool,

    /// Show synonyms and related words
    #[arg(short, long, default_value_t = true)]
    synonyms: bool,

    /// Show rhymes (via Prosodic)
    #[arg(short = 'r', long = "rhymes", default_value_t = true)]
    rhymes: bool,

    /// Re-download and re-index the dictionary
    #[arg(long)]
    refresh: bool,
}

// ── Database ─────────────────────────────────────────────────────────────────

const GCIDE_URL: &str = "https://www.ibiblio.org/webster/gcide_xml-0.53.zip";
const DB_FILE: &str = "webster1913.db";
const DB_VERSION: i64 = 1;

fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .context("Cannot determine local data directory")?;
    let dir = base.join("good-dict");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(DB_FILE))
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS entries (
            id          INTEGER PRIMARY KEY,
            word        TEXT NOT NULL,        -- lowercased for search
            headword    TEXT NOT NULL,        -- original capitalisation
            pos         TEXT,                 -- part of speech
            pronunciation TEXT,
            etymology   TEXT,
            body        TEXT NOT NULL         -- full XML-stripped text blob
        );
        CREATE TABLE IF NOT EXISTS senses (
            id          INTEGER PRIMARY KEY,
            entry_id    INTEGER NOT NULL REFERENCES entries(id),
            num         TEXT,                 -- e.g. \"1.\", \"2.\", \"(a)\"
            definition  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS quotes (
            id          INTEGER PRIMARY KEY,
            entry_id    INTEGER NOT NULL REFERENCES entries(id),
            sense_id    INTEGER REFERENCES senses(id),
            text        TEXT NOT NULL,
            author      TEXT
        );
        CREATE TABLE IF NOT EXISTS synonyms (
            id          INTEGER PRIMARY KEY,
            entry_id    INTEGER NOT NULL REFERENCES entries(id),
            text        TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS word_rimes (
            word        TEXT PRIMARY KEY,
            rime        TEXT NOT NULL,
            num_sylls   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_entries_word ON entries(word);
        CREATE INDEX IF NOT EXISTS idx_word_rimes_rime ON word_rimes(rime, num_sylls);",
    )?;
    Ok(())
}

fn db_version(conn: &Connection) -> i64 {
    conn.query_row("SELECT value FROM meta WHERE key='version'", [], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

// ── XML parsing ──────────────────────────────────────────────────────────────

use quick_xml::events::Event;
use quick_xml::Reader;

/// Plain text extracted from XML by stripping all tags.
fn strip_tags(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(e)) => {
                out.push_str(&e.unescape().unwrap_or_default());
            }
            Ok(Event::CData(e)) => {
                out.push_str(std::str::from_utf8(e.as_ref()).unwrap_or(""));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    // Collapse whitespace runs
    let mut result = String::with_capacity(out.len());
    let mut prev_space = true;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(ch);
            prev_space = false;
        }
    }
    result.trim().to_string()
}

/// Extract inner text of the next matching end-tag, buffering XML fragments.
/// Returns (content_string, remaining_bytes_to_skip).
///
/// We parse entries using a simple state machine over the raw XML bytes.
/// Each `<p>` block may contain one `<ent>` (entry word) and various child
/// elements.  We collect everything between `<ent>` starts and the end of
/// the enclosing `<p>` block.

#[derive(Default)]
struct EntryBuilder {
    word: Option<String>,
    headword: Option<String>,
    pos: Option<String>,
    pronunciation: Option<String>,
    etymology: Option<String>,
    senses: Vec<Sense>,
    quotes: Vec<Quote>,
    synonyms: Vec<String>,
    // (no transient streaming state needed; new parser uses string scanning)
}

#[derive(Default, Clone)]
struct Sense {
    num: Option<String>,
    definition: String,
}

#[derive(Default, Clone)]
struct Quote {
    text: String,
    author: Option<String>,
}

/// Commit a finished entry to the database.
fn commit_entry(
    builder: &EntryBuilder,
    tx: &rusqlite::Transaction,
    count: &mut usize,
    pb: &ProgressBar,
) -> Result<()> {
    let word = match &builder.word {
        Some(w) => w,
        None => return Ok(()),
    };
    let headword = builder.headword.as_deref().unwrap_or(word).to_string();

    let body: String = builder
        .senses
        .iter()
        .map(|s| {
            format!("{} {}", s.num.as_deref().unwrap_or(""), s.definition)
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    tx.execute(
        "INSERT INTO entries (word, headword, pos, pronunciation, etymology, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            word,
            headword,
            builder.pos,
            builder.pronunciation,
            builder.etymology,
            body,
        ],
    )?;
    let entry_id = tx.last_insert_rowid();

    for sense in &builder.senses {
        tx.execute(
            "INSERT INTO senses (entry_id, num, definition) VALUES (?1, ?2, ?3)",
            params![entry_id, sense.num, sense.definition],
        )?;
    }
    for quote in &builder.quotes {
        tx.execute(
            "INSERT INTO quotes (entry_id, sense_id, text, author) VALUES (?1, ?2, ?3, ?4)",
            params![entry_id, Option::<i64>::None, quote.text, quote.author],
        )?;
    }
    for syn in &builder.synonyms {
        tx.execute(
            "INSERT INTO synonyms (entry_id, text) VALUES (?1, ?2)",
            params![entry_id, syn],
        )?;
    }

    *count += 1;
    if (*count).is_multiple_of(1000) {
        pb.set_message(format!("Indexed {count} entries…"));
    }
    Ok(())
}

/// Extract the text content of a specific XML element from a paragraph chunk,
/// handling nested tags by collecting all text nodes within.
/// Returns the joined inner text of the first matching element.
fn extract_tag(paragraph: &str, tag: &str) -> Option<String> {
    // Simple scan: find <tag> … </tag> and collect text within.
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = paragraph.find(&open)?;
    // find the end of the opening tag
    let tag_end = paragraph[start..].find('>')?;
    let content_start = start + tag_end + 1;
    let content_end = paragraph[content_start..].find(&close)?;
    Some(strip_tags(
        &paragraph[content_start..content_start + content_end],
    ))
}

#[allow(dead_code)]
fn extract_all_tags(paragraph: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut results = Vec::new();
    let mut pos = 0;
    while let Some(start) = paragraph[pos..].find(&open) {
        let abs_start = pos + start;
        if let Some(tag_end) = paragraph[abs_start..].find('>') {
            let content_start = abs_start + tag_end + 1;
            if let Some(content_end) = paragraph[content_start..].find(&close) {
                let text = strip_tags(&paragraph[content_start..content_start + content_end]);
                if !text.is_empty() {
                    results.push(text);
                }
                pos = content_start + content_end + close.len();
                continue;
            }
        }
        break;
    }
    results
}

/// Parse a GCIDE XML file and insert entries into the database.
/// Key insight: entries span multiple <p> paragraphs. We accumulate paragraphs
/// until a new <ent> tag appears, then commit the previous entry.
fn parse_and_insert(conn: &Connection, xml_data: &[u8], pb: &ProgressBar) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0usize;

    // Collect all <p>…</p> blocks as raw strings
    let xml_str = std::str::from_utf8(xml_data).unwrap_or_default();

    // Split on <p> boundaries to get paragraph-level chunks
    // We do this without a full XML parser to avoid the nested-tag text problem
    let paragraphs: Vec<&str> = {
        let mut ps = Vec::new();
        let mut rest = xml_str;
        while let Some(start) = rest.find("<p>") {
            rest = &rest[start + 3..];
            if let Some(end) = rest.find("</p>") {
                ps.push(&rest[..end]);
                rest = &rest[end + 4..];
            } else {
                break;
            }
        }
        ps
    };

    let mut builder = EntryBuilder::default();

    for para in &paragraphs {
        // Check if this paragraph starts a new entry
        let has_ent = para.contains("<ent>");

        if has_ent {
            // Flush previous entry
            if builder.word.is_some() {
                commit_entry(&builder, &tx, &mut count, pb)?;
            }
            builder = EntryBuilder::default();

            // Extract entry word
            if let Some(word) = extract_tag(para, "ent") {
                builder.word = Some(word.to_lowercase());
            }
            // Extract headword (first hw tag)
            if let Some(hw) = extract_tag(para, "hw") {
                let cleaned = hw.replace(['"', '*', '`'], "");
                builder.headword = Some(cleaned.trim().to_string());
            }
            // Extract pronunciation (first pr, skip the vmorph/conjugation ones)
            // We want the first pr before vmorph
            if builder.pronunciation.is_none() {
                if let Some(pr) = extract_tag(para, "pr") {
                    let t = pr
                        .trim()
                        .trim_matches(&['(', ')'] as &[char])
                        .trim()
                        .to_string();
                    if !t.is_empty() && t.len() < 60 {
                        builder.pronunciation = Some(t);
                    }
                }
            }
            // Extract part of speech
            if let Some(pos) = extract_tag(para, "pos") {
                let t = pos.trim().to_string();
                // skip vmorph/conjugation pos tags
                if !t.is_empty() && builder.pos.is_none() {
                    builder.pos = Some(t);
                }
            }
            // Extract etymology
            if let Some(ety) = extract_tag(para, "ety") {
                let t = ety.trim().to_string();
                if !t.is_empty() {
                    builder.etymology = Some(t);
                }
            }
        }

        // Only accumulate content if we're in an entry
        if builder.word.is_none() {
            continue;
        }

        // Extract senses (sn + def pairs) from this paragraph
        // A paragraph may have: <sn>N.</sn> ... <def>...</def>
        // Or just a <def>...</def> (unnumbered)
        // Or just a quote <q>...</q>
        // Or a synonym block <syn>...</syn>

        // Senses: find all <sn> tags and their following <def>
        {
            // Split paragraph into sense segments
            let mut para_rest = *para;
            let mut current_sn: Option<String> = None;

            while !para_rest.is_empty() {
                // Look for next <sn> or <def>
                let sn_pos = para_rest.find("<sn>").or_else(|| para_rest.find("<sd>"));
                let def_pos = para_rest.find("<def>");

                match (sn_pos, def_pos) {
                    (Some(sp), Some(dp)) if sp < dp => {
                        // sn comes first
                        if let Some(sn_text) =
                            extract_tag(para_rest, "sn").or_else(|| extract_tag(para_rest, "sd"))
                        {
                            current_sn = Some(sn_text);
                        }
                        // advance past the <sn>
                        let tag_end = para_rest.find("</sn>").unwrap_or(sp + 4);
                        let sd_end = para_rest.find("</sd>").unwrap_or(sp + 4);
                        para_rest = &para_rest[tag_end.min(sd_end) + 5..];
                    }
                    (_, Some(dp)) => {
                        // def comes next
                        let def_end = dp + 5;
                        if let Some(end) = para_rest[def_end..].find("</def>") {
                            let def_text = strip_tags(&para_rest[def_end..def_end + end]);
                            if !def_text.is_empty() && def_text != "." {
                                builder.senses.push(Sense {
                                    num: current_sn.take(),
                                    definition: def_text,
                                });
                            }
                            para_rest = &para_rest[def_end + end + 6..];
                        } else {
                            break;
                        }
                    }
                    (Some(sp), None) => {
                        // sn but no def — just advance
                        para_rest = &para_rest[sp + 4..];
                    }
                    (None, None) => break,
                }
            }
        }

        // Extract quotes
        {
            let mut para_rest = *para;
            while let Some(q_start) = para_rest.find("<q>") {
                let content_start = q_start + 3;
                if let Some(q_end) = para_rest[content_start..].find("</q>") {
                    let q_block = &para_rest[content_start..content_start + q_end];
                    let q_text = strip_tags(q_block);
                    if !q_text.is_empty() && q_text.len() > 5 {
                        // Also look for <qau> inside same rj block that follows
                        let after_q = &para_rest[content_start + q_end + 4..];
                        let author = after_q
                            .find("<rj>")
                            .and_then(|_| extract_tag(after_q, "qau"))
                            .or_else(|| extract_tag(after_q, "au"));

                        builder.quotes.push(Quote {
                            text: q_text,
                            author,
                        });
                    }
                    para_rest = &para_rest[content_start + q_end + 4..];
                } else {
                    break;
                }
            }
        }

        // Extract synonyms and usage notes
        if para.contains("<syn>") {
            if let Some(syn_text) = extract_tag(para, "syn") {
                let t = syn_text.trim().to_string();
                if !t.is_empty() {
                    builder.synonyms.push(t);
                }
            }
        }
        // Usage note (usage tag has lovely prose)
        if para.contains("<usage>") {
            if let Some(usage_text) = extract_tag(para, "usage") {
                let t = usage_text.trim().to_string();
                if !t.is_empty() && builder.synonyms.len() < 3 {
                    builder.synonyms.push(t);
                }
            }
        }
    }

    // Flush last entry
    if builder.word.is_some() {
        commit_entry(&builder, &tx, &mut count, pb)?;
    }

    tx.commit()?;
    Ok(count)
}

// ── Download & build database ─────────────────────────────────────────────────

fn build_database(db_path: &Path) -> Result<()> {
    println!(
        "{} {}",
        "Downloading".bold().green(),
        "Webster's 1913 dictionary from ibiblio.org…".dimmed()
    );

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb.set_message("Connecting…");

    let response = ureq::get(GCIDE_URL)
        .call()
        .context("Failed to download dictionary. Check your internet connection.")?;

    pb.set_message("Downloading…");
    let mut data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut data)
        .context("Download interrupted")?;

    pb.set_message(format!(
        "Downloaded {}. Extracting…",
        human_bytes(data.len())
    ));

    // Open zip
    let cursor = Cursor::new(&data);
    let mut zip = zip::ZipArchive::new(cursor).context("Invalid zip file")?;

    // Collect XML file names for per-letter files
    let xml_names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            let f = zip.by_index(i).ok()?;
            let name = f.name().to_string();
            // We want gcide_a.xml … gcide_z.xml (skip abbreviations, authorities)
            if name.contains("gcide_xml") && name.ends_with(".xml") {
                // main wrapper — skip, it includes all
            }
            if name.ends_with(".xml")
                && !name.contains("abbreviations")
                && !name.contains("authorities")
                && !name.contains("/gcide.xml")
                && !name.ends_with("/gcide.xml")
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    pb.set_message("Building database…");

    // Create (or recreate) database
    if db_path.exists() {
        std::fs::remove_file(db_path)?;
    }
    let conn = open_db(db_path)?;
    init_schema(&conn)?;

    let mut total = 0usize;
    for xml_name in &xml_names {
        let mut file = zip
            .by_name(xml_name)
            .with_context(|| format!("File not found in zip: {xml_name}"))?;
        let mut xml_data = Vec::new();
        file.read_to_end(&mut xml_data)?;

        let letter = xml_name
            .split('_')
            .next_back()
            .and_then(|s| s.strip_suffix(".xml"))
            .unwrap_or("?");

        pb.set_message(format!("Indexing '{letter}'… ({total} entries so far)"));

        match parse_and_insert(&conn, &xml_data, &pb) {
            Ok(n) => total += n,
            Err(e) => eprintln!("Warning parsing {xml_name}: {e}"),
        }
    }

    // Write version
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('version', ?1)",
        params![DB_VERSION.to_string()],
    )?;

    // Pre-build phonetic rhymes index via Prosodic if available
    let _ = build_rhymes_index(db_path);

    pb.finish_with_message(format!(
        "✓ Dictionary ready — {} entries indexed",
        total.to_string().bold()
    ));

    Ok(())
}

fn human_bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

// ── Lookup ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Entry {
    id: i64,
    #[allow(dead_code)]
    word: String,
    headword: String,
    pos: Option<String>,
    pronunciation: Option<String>,
    etymology: Option<String>,
}

#[derive(Debug)]
struct SenseRow {
    num: Option<String>,
    definition: String,
}

#[derive(Debug)]
struct QuoteRow {
    text: String,
    author: Option<String>,
}

fn lookup(conn: &Connection, word: &str) -> Result<Vec<Entry>> {
    let w = word.to_lowercase();
    let mut stmt = conn.prepare(
        "SELECT id, word, headword, pos, pronunciation, etymology
         FROM entries WHERE word = ?1
         ORDER BY id",
    )?;
    let entries = stmt
        .query_map(params![w], |row| {
            Ok(Entry {
                id: row.get(0)?,
                word: row.get(1)?,
                headword: row.get(2)?,
                pos: row.get(3)?,
                pronunciation: row.get(4)?,
                etymology: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(entries)
}

fn get_senses(conn: &Connection, entry_id: i64) -> Result<Vec<SenseRow>> {
    let mut stmt =
        conn.prepare("SELECT num, definition FROM senses WHERE entry_id = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map(params![entry_id], |row| {
            Ok(SenseRow {
                num: row.get(0)?,
                definition: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn get_quotes(conn: &Connection, entry_id: i64) -> Result<Vec<QuoteRow>> {
    let mut stmt =
        conn.prepare("SELECT text, author FROM quotes WHERE entry_id = ?1 ORDER BY id LIMIT 5")?;
    let rows = stmt
        .query_map(params![entry_id], |row| {
            Ok(QuoteRow {
                text: row.get(0)?,
                author: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn get_synonyms(conn: &Connection, entry_id: i64) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT text FROM synonyms WHERE entry_id = ?1 ORDER BY id LIMIT 3")?;
    let rows = stmt
        .query_map(params![entry_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Candidate word with its edit distance and Jaro-Winkler score.
#[derive(Debug)]
struct FuzzyMatch {
    word: String,
    dist: usize,
    jw: f64,
}

/// Load all unique dictionary words and find the closest matches to `query`.
/// Returns up to `limit` matches, sorted by (edit_distance, -jaro_winkler).
fn fuzzy_lookup(conn: &Connection, query: &str, limit: usize) -> Result<Vec<FuzzyMatch>> {
    let q = query.to_lowercase();

    // Load all unique indexed words (they're short ASCII strings, this is fast)
    let mut stmt = conn.prepare("SELECT DISTINCT word FROM entries")?;
    let words: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    // Score every word
    let mut scored: Vec<FuzzyMatch> = words
        .into_iter()
        .map(|w| {
            let dist = strsim::levenshtein(&q, &w);
            let jw = strsim::jaro_winkler(&q, &w);
            FuzzyMatch { word: w, dist, jw }
        })
        .collect();

    // Sort: primary = edit distance, secondary = jaro-winkler descending
    scored.sort_by(|a, b| a.dist.cmp(&b.dist).then(b.jw.partial_cmp(&a.jw).unwrap()));

    scored.truncate(limit);
    Ok(scored)
}

/// Helper to locate Python binary and prosodic_rhymes.py script.
fn prosodic_tools() -> Option<(PathBuf, PathBuf)> {
    let python_candidates = [
        PathBuf::from(".venv/bin/python"),
        PathBuf::from("/home/vega/Documents/git/local/good-dict-rs/.venv/bin/python"),
    ];
    let python_bin = python_candidates.into_iter().find(|p| p.exists())?;

    let script_candidates = [
        PathBuf::from("prosodic_rhymes.py"),
        PathBuf::from("/home/vega/Documents/git/local/good-dict-rs/prosodic_rhymes.py"),
    ];
    let script_path = script_candidates.into_iter().find(|p| p.exists())?;

    Some((python_bin, script_path))
}

/// Pre-populate the SQLite word_rimes table using Prosodic in ~1 second.
fn build_rhymes_index(db_path: &Path) -> Result<()> {
    if let Some((python_bin, script_path)) = prosodic_tools() {
        let status = Command::new(python_bin)
            .arg(script_path)
            .arg("--build-index")
            .arg(db_path)
            .status();
        if let Ok(st) = status {
            if st.success() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Query Prosodic to calculate the rime for an out-of-vocabulary word, caching it in SQLite.
fn fetch_rime_from_prosodic(word: &str, db_path: &Path) -> Option<(String, i64)> {
    let (python_bin, script_path) = prosodic_tools()?;
    let output = Command::new(python_bin)
        .arg(script_path)
        .arg("--get-rime")
        .arg(word)
        .arg(db_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let val: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let rime = val.get("rime")?.as_str()?.to_string();
    let num_sylls = val.get("num_sylls")?.as_i64()?;
    Some((rime, num_sylls))
}

/// Ensure word_rimes is populated if Prosodic is installed.
fn ensure_rhymes_indexed(conn: &Connection, db_path: &Path) {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM word_rimes", [], |r| r.get(0))
        .unwrap_or(0);
    if count == 0 {
        let _ = build_rhymes_index(db_path);
    }
}

/// Look up rhymes for a word instantly (<1ms) from the SQLite pre-generated index.
/// We invoke the Python helper strictly as a subprocess (either for initial index generation
/// or for OOV words), keeping the Rust binary completely unencumbered by GPL code.
fn lookup_rhymes(conn: &Connection, db_path: &Path, word: &str, limit: usize) -> Vec<String> {
    ensure_rhymes_indexed(conn, db_path);

    let w = word.to_lowercase();
    let rime_info: Option<(String, i64)> = conn
        .query_row(
            "SELECT rime, num_sylls FROM word_rimes WHERE word = ?1",
            params![w],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let (rime, num_sylls) = match rime_info {
        Some((r, n)) => (r, n),
        None => {
            if let Some((r, n)) = fetch_rime_from_prosodic(&w, db_path) {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO word_rimes (word, rime, num_sylls) VALUES (?1, ?2, ?3)",
                    params![w, r, n],
                );
                (r, n)
            } else {
                return Vec::new();
            }
        }
    };

    // 1. Query matching rhymes from SQLite, joined with entries so we return genuine dictionary words
    let mut stmt = match conn.prepare(
        "SELECT DISTINCT r.word
         FROM word_rimes r
         JOIN entries e ON e.word = r.word
         WHERE r.rime = ?1 AND r.word != ?2
         ORDER BY ABS(r.num_sylls - ?3), length(r.word), r.word
         LIMIT ?4",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut results: Vec<String> = stmt
        .query_map(params![rime, w, num_sylls, limit as i64], |row| row.get(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // 2. If entries had fewer than limit results, pull remainder from word_rimes directly
    if results.len() < limit {
        if let Ok(mut stmt_all) = conn.prepare(
            "SELECT word
             FROM word_rimes
             WHERE rime = ?1 AND word != ?2
             ORDER BY ABS(num_sylls - ?3), length(word), word
             LIMIT ?4",
        ) {
            if let Ok(rows) =
                stmt_all.query_map(params![rime, w, num_sylls, limit as i64], |row| row.get(0))
            {
                for r in rows.flatten() {
                    if !results.contains(&r) {
                        results.push(r);
                    }
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }
    }

    results
}

// ── Display ──────────────────────────────────────────────────────────────────

/// Wrap text at a given column width (respecting word boundaries).
fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut lines = Vec::new();
    let mut current = String::from(indent);
    let indent_len = indent.chars().count();

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        let current_len = current.chars().count();
        if current_len > indent_len && current_len + 1 + word_len > width {
            lines.push(current.clone());
            current = format!("{indent}{word}");
        } else if current_len > indent_len {
            current.push(' ');
            current.push_str(word);
        } else {
            current.push_str(word);
        }
    }
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
        .min(100)
}

fn display_entries(
    entries: &[(Entry, Vec<SenseRow>, Vec<QuoteRow>, Vec<String>)],
    show_ety: bool,
    show_quotes: bool,
    show_syns: bool,
    rhymes: &[String],
) {
    let width = term_width();
    let rule: String = "─".repeat(width);

    for (i, (entry, senses, quotes, syns)) in entries.iter().enumerate() {
        if i > 0 {
            println!("{}", rule.dimmed());
        }

        // ── Header ──────────────────────────────────────────────────────────
        let mut header = format!("{}", entry.headword.bold().bright_white());
        if let Some(ref pr) = entry.pronunciation {
            header.push_str(&format!("  {}", pr.bright_black()));
        }
        if let Some(ref pos) = entry.pos {
            header.push_str(&format!("  {}", pos.italic().yellow()));
        }
        println!("\n{header}");
        println!(
            "{}",
            "─".repeat(width.min(header.chars().count() + 4)).dimmed()
        );

        // ── Etymology ───────────────────────────────────────────────────────
        if show_ety {
            if let Some(ref ety) = entry.etymology {
                let t = strip_tags(ety);
                // Remove outer brackets if already present (GCIDE wraps ety in [...])
                let t = t.trim();
                let t = if t.starts_with('[') && t.ends_with(']') {
                    &t[1..t.len() - 1]
                } else {
                    t
                };
                if !t.is_empty() {
                    println!("{}", wrap(&format!("[{t}]"), width, "  ").bright_black());
                }
            }
        }

        // ── Definitions ─────────────────────────────────────────────────────
        if senses.is_empty() {
            // no sense rows — fall back to body
        } else {
            println!();
            for sense in senses {
                let num_prefix = sense
                    .num
                    .as_deref()
                    .map(|n| format!("{n} "))
                    .unwrap_or_default();
                let def_clean = strip_tags(&sense.definition);
                let full = format!("{num_prefix}{def_clean}");
                println!("{}", wrap(&full, width, "  "));
            }
        }

        // ── Quotes ──────────────────────────────────────────────────────────
        if show_quotes && !quotes.is_empty() {
            println!();
            for q in quotes.iter().take(3) {
                let text = strip_tags(&q.text);
                if text.len() > 10 {
                    let quoted = format!("\u{201c}{}\u{201d}", text);
                    println!("{}", wrap(&quoted, width, "     ").italic().bright_cyan());
                    if let Some(ref author) = q.author {
                        println!(
                            "{}",
                            format!("          — {}", strip_tags(author)).bright_black()
                        );
                    }
                }
            }
        }

        // ── Synonyms & Usage ────────────────────────────────────────────────
        if show_syns && !syns.is_empty() {
            let mut syn_words: Vec<String> = Vec::new();
            let mut see_refs: Vec<String> = Vec::new();
            let mut usage_notes: Vec<String> = Vec::new();

            for raw in syns.iter() {
                let t = strip_tags(raw);
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Check if this is a synonym list (starts with "Syn." or "Syn --" etc.)
                let is_syn_header = trimmed.starts_with("Syn.")
                    || trimmed.starts_with("Syn --")
                    || trimmed.starts_with("Syn:");

                if is_syn_header {
                    let after_dash = if let Some(idx) = trimmed.find("--") {
                        trimmed[idx + 2..].trim()
                    } else if let Some(idx) = trimmed.find(':') {
                        trimmed[idx + 1..].trim()
                    } else {
                        trimmed.strip_prefix("Syn.").unwrap_or(trimmed).trim()
                    };

                    // Extract any "See <Target>." cross reference
                    let mut content = after_dash;
                    if let Some(see_idx) = content.find("See ") {
                        let ref_str = content[see_idx..].trim().trim_end_matches('.');
                        if !see_refs.iter().any(|r| r == ref_str) {
                            see_refs.push(ref_str.to_string());
                        }
                        content = content[..see_idx].trim();
                    }

                    // Semicolons or commas can separate words in the list
                    // A period may separate list from subsequent usage text
                    let words_part = content.split('.').next().unwrap_or(content);
                    for item in words_part.split([';', ',']) {
                        let word = item.trim().trim_end_matches('.');
                        if !word.is_empty()
                            && !word.to_lowercase().starts_with("see")
                            && !syn_words.iter().any(|w| w.eq_ignore_ascii_case(word))
                        {
                            syn_words.push(word.to_string());
                        }
                    }

                    // Any remaining text after the first period is usage text
                    if let Some(idx) = content.find('.') {
                        let remaining = content[idx + 1..].trim();
                        if remaining.len() > 15 {
                            usage_notes.push(remaining.to_string());
                        }
                    }
                } else {
                    // Usage prose or notes
                    usage_notes.push(trimmed.to_string());
                }
            }

            if !syn_words.is_empty() || !usage_notes.is_empty() || !see_refs.is_empty() {
                println!();
            }

            if !syn_words.is_empty() {
                let mut line = format!(
                    "  {} {}",
                    "Synonyms:".bold().bright_magenta(),
                    syn_words.join("  ·  ").bright_black()
                );
                if !see_refs.is_empty() {
                    line.push_str(&format!("  {}", see_refs.join(", ").dimmed()));
                }
                println!("{}", line);
            } else if !see_refs.is_empty() {
                println!(
                    "  {} {}",
                    "Synonyms:".bold().bright_magenta(),
                    see_refs.join(", ").dimmed()
                );
            }

            for note in usage_notes.iter().take(2) {
                println!("{}", wrap(note, width, "  ").bright_black().italic());
            }
        }
    }

    // ── Rhymes (via Prosodic) ────────────────────────────────────────────────
    if !rhymes.is_empty() {
        println!();
        println!(
            "  {} {}",
            "Rhymes:".bold().bright_blue(),
            rhymes.join("  ·  ").bright_black()
        );
    }

    println!();
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db_path = db_path()?;
    let needs_build = cli.refresh || !db_path.exists() || {
        let conn = open_db(&db_path)?;
        db_version(&conn) < DB_VERSION
    };

    if needs_build {
        build_database(&db_path)?;
    }

    let conn = open_db(&db_path)?;

    for (word_idx, word) in cli.words.iter().enumerate() {
        if word_idx > 0 {
            println!();
        }

        let entries = lookup(&conn, word)?;

        if entries.is_empty() {
            let candidates = fuzzy_lookup(&conn, word, 6)?;
            let best = candidates.first();

            // Auto-correct if the closest match is within 2 edits
            if best.is_some_and(|m| m.dist <= 2) {
                let best = best.unwrap();
                eprintln!(
                    "  {} {} → {}",
                    "~".bright_yellow(),
                    word.bright_black(),
                    best.word.bold()
                );
                // Re-run lookup with the corrected word
                let corrected = lookup(&conn, &best.word)?;
                let mut display_data = Vec::new();
                for entry in corrected {
                    let senses = get_senses(&conn, entry.id)?;
                    let quotes = if cli.quotes {
                        get_quotes(&conn, entry.id)?
                    } else {
                        vec![]
                    };
                    let syns = if cli.synonyms {
                        get_synonyms(&conn, entry.id)?
                    } else {
                        vec![]
                    };
                    display_data.push((entry, senses, quotes, syns));
                }
                let rhymes = if cli.rhymes {
                    lookup_rhymes(&conn, &db_path, &best.word, 12)
                } else {
                    Vec::new()
                };
                display_entries(
                    &display_data,
                    cli.etymology,
                    cli.quotes,
                    cli.synonyms,
                    &rhymes,
                );
            } else {
                // Show ranked suggestions (dist 3-4 range)
                let suggestions: Vec<&str> = candidates
                    .iter()
                    .filter(|m| m.dist <= 4)
                    .take(5)
                    .map(|m| m.word.as_str())
                    .collect();
                eprintln!("{} {}", "Not found:".bold().red(), word.bright_white());
                if !suggestions.is_empty() {
                    eprintln!("  Did you mean: {}", suggestions.join(", ").bright_black());
                }
            }
            continue;
        }

        let mut display_data = Vec::new();
        // Group entries: each entry row is a separate part-of-speech block
        // but we show all of them together for the same word
        for entry in entries {
            let senses = get_senses(&conn, entry.id)?;
            let quotes = if cli.quotes {
                get_quotes(&conn, entry.id)?
            } else {
                vec![]
            };
            let syns = if cli.synonyms {
                get_synonyms(&conn, entry.id)?
            } else {
                vec![]
            };
            display_data.push((entry, senses, quotes, syns));
        }

        let rhymes = if cli.rhymes {
            lookup_rhymes(&conn, &db_path, word, 12)
        } else {
            Vec::new()
        };
        display_entries(
            &display_data,
            cli.etymology,
            cli.quotes,
            cli.synonyms,
            &rhymes,
        );
    }

    Ok(())
}
