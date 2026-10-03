//! `fallow slim` — write a smaller mbox.
//!
//! Drops whole messages by sender pattern, Gmail label, or bulk-mail headers,
//! and replaces attachment parts with a one-line placeholder (optionally saving
//! the decoded files, de-duplicated by content hash). Everything else in each
//! kept message is copied byte-for-byte.

use crate::mime::{self, domain_of, extract_addrs};
use crate::util::*;
use clap::Args;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Args)]
pub struct SlimArgs {
    /// Input .mbox file (never modified)
    pub mbox: PathBuf,

    /// Output .mbox file
    #[arg(long, short, required_unless_present = "dry_run")]
    pub out: Option<PathBuf>,

    /// Drop mail from senders matching PATTERN (repeatable).
    /// Forms: a@b.com | @b.com or b.com (domain + subdomains) | glob with * e.g. noreply@* , *@*.linkedin.com
    #[arg(long = "drop", value_name = "PATTERN")]
    pub drop: Vec<String>,

    /// File listing senders to drop (use - for stdin). Accepts one or more addresses or
    /// patterns per line separated by commas/semicolons/spaces, "Name <addr>" lines,
    /// # comments, or a fallow stats CSV (first column is used)
    #[arg(long, visible_alias = "senders", value_name = "FILE")]
    pub drop_file: Vec<PathBuf>,

    /// Never drop mail from senders matching PATTERN (overrides every drop rule)
    #[arg(long = "keep", value_name = "PATTERN")]
    pub keep: Vec<String>,

    /// File with one --keep pattern per line
    #[arg(long, value_name = "FILE")]
    pub keep_file: Vec<PathBuf>,

    /// Drop newsletters/marketing (List-Unsubscribe, List-Id or Precedence: bulk/list headers)
    #[arg(long)]
    pub drop_bulk: bool,

    /// Drop messages carrying this Gmail label, e.g. "Spam", "Trash", "Category Promotions" (repeatable)
    #[arg(long = "drop-label", value_name = "LABEL")]
    pub drop_label: Vec<String>,

    /// Replace attachments with a short text placeholder
    #[arg(long)]
    pub strip_attachments: bool,

    /// Only strip/extract attachments at least this large (KB, decoded)
    #[arg(long, value_name = "KB", default_value_t = 0)]
    pub min_attachment_kb: u64,

    /// Save attachments (decoded, de-duplicated by content hash) into DIR before stripping
    #[arg(long, value_name = "DIR")]
    pub extract_attachments: Option<PathBuf>,

    /// Also write the dropped messages to this mbox (a safety copy you can review or restore)
    #[arg(long, value_name = "FILE")]
    pub dropped_out: Option<PathBuf>,

    /// Write a CSV listing every dropped message (reason, from, date, subject, size)
    #[arg(long, value_name = "FILE")]
    pub dropped_csv: Option<PathBuf>,

    /// Report what would happen without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

// ------------------------------------------------------------ sender matching

#[derive(Default)]
struct Matcher {
    /// pattern text, in the order given (for reporting)
    patterns: Vec<String>,
    exact: HashMap<String, usize>,
    domains: Vec<(String, usize)>,
    globs: Vec<(String, usize)>,
}

/// Split one line of a sender list into patterns.
fn parse_list_line(line: &str, csv_first_column: bool) -> Vec<String> {
    let line = line.trim().trim_start_matches('\u{feff}');
    if line.is_empty() || line.starts_with('#') {
        return Vec::new();
    }
    if csv_first_column {
        let first = if let Some(rest) = line.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else {
            line.split(',').next().unwrap_or("")
        };
        return vec![first.trim().to_ascii_lowercase()];
    }
    if line.contains('<') {
        return extract_addrs(line);
    }
    line.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|t| {
            t.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .trim_start_matches("mailto:")
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn read_list(path: &PathBuf) -> Result<Vec<String>> {
    let text = if path.as_os_str() == "-" {
        let mut t = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)?;
        t
    } else {
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut lines = text.lines().peekable();
    // A fallow stats CSV starts with a header such as "sender,messages,size_mb,..."
    let header = lines
        .peek()
        .map(|l| l.trim_start_matches('\u{feff}').trim().to_ascii_lowercase())
        .unwrap_or_default();
    let csv = ["sender,", "domain,", "receiver,", "cc,", "bcc,"]
        .iter()
        .any(|h| header.starts_with(h));
    if csv {
        lines.next();
    }
    Ok(lines.flat_map(|l| parse_list_line(l, csv)).collect())
}

impl Matcher {
    fn new(patterns: &[String], files: &[PathBuf]) -> Result<Self> {
        let mut all: Vec<String> = patterns
            .iter()
            .flat_map(|p| parse_list_line(p, false))
            .collect();
        for f in files {
            all.extend(read_list(f)?);
        }
        let mut m = Matcher::default();
        for p in all {
            if m.patterns.contains(&p) {
                continue;
            }
            let i = m.patterns.len();
            if p.contains('*') {
                m.globs.push((p.clone(), i));
            } else if let Some(d) = p.strip_prefix('@') {
                m.domains.push((d.to_string(), i));
            } else if p.contains('@') {
                m.exact.insert(p.clone(), i);
            } else {
                m.domains.push((p.clone(), i));
            }
            m.patterns.push(p);
        }
        Ok(m)
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Index of the first pattern matching `addr`.
    fn find(&self, addr: &str) -> Option<usize> {
        if addr.is_empty() {
            return None;
        }
        if let Some(&i) = self.exact.get(addr) {
            return Some(i);
        }
        let dom = domain_of(addr);
        for (d, i) in &self.domains {
            if dom == d
                || (dom.len() > d.len()
                    && dom.ends_with(d.as_str())
                    && dom.as_bytes()[dom.len() - d.len() - 1] == b'.')
            {
                return Some(*i);
            }
        }
        self.globs
            .iter()
            .find(|(g, _)| glob(g.as_bytes(), addr.as_bytes()))
            .map(|(_, i)| *i)
    }

    fn matches(&self, addr: &str) -> bool {
        self.find(addr).is_some()
    }
}

/// `*`-only glob.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] != b'*' && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------------------------- worker

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Reason {
    Label,
    Bulk,
    Sender,
}

enum Outcome<'a> {
    Dropped {
        reason: Reason,
        /// index of the --drop pattern that matched (sender rule only)
        rule: Option<usize>,
        sender: String,
        data: &'a [u8],
        date: String,
        subject: String,
    },
    Kept {
        data: Cow<'a, [u8]>,
        sender: String,
        stripped: u32,
        stripped_bytes: u64,
        saved: Vec<SavedRow>,
    },
}

struct SavedRow {
    stored_as: String,
    filename: String,
    bytes: u64,
    new_file: bool,
    from: String,
    date: String,
    subject: String,
}

struct Cfg {
    drop: Matcher,
    keep: Matcher,
    drop_bulk: bool,
    drop_labels: Vec<String>,
    strip: bool,
    min_bytes: u64,
    extract: Option<PathBuf>,
    dry_run: bool,
    want_dropped_meta: bool,
    /// content hash → stored file name (true de-duplication across file names)
    seen: Mutex<HashMap<[u8; 32], String>>,
}

const WANTED: &[&str] = &[
    "from",
    "list-unsubscribe",
    "list-id",
    "precedence",
    "x-gmail-labels",
    "date",
    "subject",
    "content-length",
];

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    let s = if s.is_empty() {
        "unnamed".to_string()
    } else {
        s
    };
    // keep the extension when shortening
    if s.chars().count() > 100 {
        let ext: String = s
            .rsplit_once('.')
            .map(|(_, e)| e.chars().take(10).collect())
            .unwrap_or_default();
        let stem: String = s.chars().take(90).collect();
        if ext.is_empty() {
            stem
        } else {
            format!("{stem}.{ext}")
        }
    } else {
        s
    }
}

fn process<'a>(msg: &'a [u8], cfg: &Cfg) -> Outcome<'a> {
    let start = mime::skip_from_line(msg);
    let (he, _) = mime::split_head_body(&msg[start..]);
    let h = mime::parse_headers(&msg[start..start + he], WANTED);
    let sender = h
        .get("from")
        .map(extract_addrs)
        .and_then(|v| v.into_iter().next())
        .unwrap_or_default();
    let protected = !cfg.keep.is_empty() && cfg.keep.matches(&sender);
    if !protected {
        let mut reason = None;
        let mut rule = None;
        if !cfg.drop_labels.is_empty() {
            let labels = mime::gmail_labels(&h);
            if labels
                .iter()
                .any(|l| cfg.drop_labels.iter().any(|d| d.eq_ignore_ascii_case(l)))
            {
                reason = Some(Reason::Label);
            }
        }
        if reason.is_none() && cfg.drop_bulk && mime::is_bulk(&h) {
            reason = Some(Reason::Bulk);
        }
        if reason.is_none() && !cfg.drop.is_empty() {
            if let Some(i) = cfg.drop.find(&sender) {
                reason = Some(Reason::Sender);
                rule = Some(i);
            }
        }
        if let Some(reason) = reason {
            let (date, subject) = if cfg.want_dropped_meta {
                (
                    h.get("date").unwrap_or("").to_string(),
                    mime::decode_words(h.get("subject").unwrap_or("")),
                )
            } else {
                (String::new(), String::new())
            };
            return Outcome::Dropped {
                reason,
                rule,
                sender,
                data: msg,
                date,
                subject,
            };
        }
    }

    if !cfg.strip && cfg.extract.is_none() {
        return Outcome::Kept {
            data: Cow::Borrowed(msg),
            sender,
            stripped: 0,
            stripped_bytes: 0,
            saved: vec![],
        };
    }

    let nl: &[u8] = if memchr::memmem::find(&msg[..msg.len().min(4096)], b"\r\n").is_some() {
        b"\r\n"
    } else {
        b"\n"
    };
    let nls = std::str::from_utf8(nl).unwrap();
    let mut repl: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let mut saved = Vec::new();
    let mut stripped_bytes = 0u64;

    for leaf in mime::leaves(msg) {
        if !leaf.is_attachment || leaf.top_level {
            continue;
        }
        let dsize = leaf.decoded_len(msg);
        if dsize < cfg.min_bytes {
            continue;
        }
        let name = leaf
            .filename
            .clone()
            .unwrap_or_else(|| match leaf.ctype.as_str() {
                "message/rfc822" => "forwarded.eml".into(),
                ct => format!(
                    "unnamed.{}",
                    ct.rsplit('/')
                        .next()
                        .unwrap_or("bin")
                        .split('+')
                        .next()
                        .unwrap_or("bin")
                ),
            });
        let mut saved_as = String::new();
        if let Some(dir) = &cfg.extract {
            let bytes = leaf.decode(msg);
            let hash = blake3::hash(&bytes);
            let fresh = format!("{}_{}", &hash.to_hex().as_str()[..16], sanitize(&name));
            let (stored, first) = {
                let mut seen = cfg.seen.lock().unwrap();
                match seen.get(hash.as_bytes()) {
                    Some(s) => (s.clone(), false),
                    None => {
                        seen.insert(*hash.as_bytes(), fresh.clone());
                        (fresh, true)
                    }
                }
            };
            let mut new_file = false;
            if first && !cfg.dry_run {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join(&stored))
                {
                    Ok(mut f) => {
                        let _ = f.write_all(&bytes);
                        new_file = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => eprintln!("warning: could not save {stored}: {e}"),
                }
            }
            saved.push(SavedRow {
                stored_as: stored.clone(),
                filename: name.clone(),
                bytes: bytes.len() as u64,
                new_file,
                from: sender.clone(),
                date: h.get("date").unwrap_or("").to_string(),
                subject: mime::decode_words(h.get("subject").unwrap_or("")),
            });
            saved_as = format!(", saved as {stored}");
        }
        if cfg.strip {
            let clean_name = name.replace(['\r', '\n'], " ");
            let text = format!(
                "Content-Type: text/plain; charset=utf-8{nls}Content-Transfer-Encoding: 8bit{nls}\
                 Content-Disposition: inline{nls}X-Fallow-Stripped: {}{nls}{nls}\
                 [Attachment removed by fallow: \"{clean_name}\" ({}, {}){saved_as}]",
                leaf.ctype,
                leaf.ctype,
                fmt_bytes(dsize)
            );
            stripped_bytes += leaf.end as u64 - leaf.start as u64;
            repl.push((leaf.start, leaf.end, text.into_bytes()));
        }
    }

    if repl.is_empty() {
        return Outcome::Kept {
            data: Cow::Borrowed(msg),
            sender,
            stripped: 0,
            stripped_bytes: 0,
            saved,
        };
    }

    // A Content-Length header (mboxcl format) would now be wrong: remove it.
    if h.has("content-length") {
        let head = &msg[start..start + he];
        let mut off = 0;
        for line in head.split_inclusive(|&c| c == b'\n') {
            if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
                repl.push((start + off, start + off + line.len(), Vec::new()));
                break;
            }
            off += line.len();
        }
    }

    repl.sort_unstable_by_key(|r| r.0);
    let stripped = repl.iter().filter(|r| !r.2.is_empty()).count() as u32;
    let mut out = Vec::with_capacity(msg.len() / 4);
    let mut pos = 0;
    for (s, e, bytes) in repl {
        if s < pos {
            continue; // overlapping (shouldn't happen)
        }
        out.extend_from_slice(&msg[pos..s]);
        out.extend_from_slice(&bytes);
        pos = e;
    }
    out.extend_from_slice(&msg[pos..]);
    Outcome::Kept {
        data: Cow::Owned(out),
        sender,
        stripped,
        stripped_bytes,
        saved,
    }
}

// ------------------------------------------------------------------ writer

/// Buffered mbox writer that keeps messages separated by a line break.
struct MboxWriter {
    w: BufWriter<File>,
    last_nl: bool,
    err: Option<std::io::Error>,
}

impl MboxWriter {
    fn new(f: File) -> Self {
        MboxWriter {
            w: BufWriter::with_capacity(8 << 20, f),
            last_nl: true,
            err: None,
        }
    }

    fn write(&mut self, data: &[u8]) {
        if self.err.is_some() || data.is_empty() {
            return;
        }
        let r = (|| {
            if !self.last_nl {
                self.w.write_all(b"\n")?;
            }
            self.w.write_all(data)
        })();
        if let Err(e) = r {
            self.err = Some(e);
        }
        self.last_nl = data.ends_with(b"\n");
    }

    fn finish(mut self) -> std::io::Result<()> {
        if let Some(e) = self.err.take() {
            return Err(e);
        }
        self.w.flush()
    }
}

// --------------------------------------------------------------------- run

pub fn run(a: SlimArgs) -> Result<()> {
    if !a.drop_bulk
        && a.drop.is_empty()
        && a.drop_file.is_empty()
        && a.drop_label.is_empty()
        && !a.strip_attachments
        && a.extract_attachments.is_none()
    {
        return Err("nothing to do: pass at least one of --drop, --drop-file, --drop-bulk, --drop-label, --strip-attachments, --extract-attachments".into());
    }
    if let Some(o) = &a.out {
        if let (Ok(x), Ok(y)) = (std::fs::canonicalize(o), std::fs::canonicalize(&a.mbox)) {
            if x == y {
                return Err("output must be a different file from the input".into());
            }
        }
    }
    if a.out.is_some() && a.out == a.dropped_out {
        return Err("--dropped-out must be a different file from --out".into());
    }
    let cfg = Cfg {
        drop: Matcher::new(&a.drop, &a.drop_file)?,
        keep: Matcher::new(&a.keep, &a.keep_file)?,
        drop_bulk: a.drop_bulk,
        drop_labels: a.drop_label.clone(),
        strip: a.strip_attachments,
        min_bytes: a.min_attachment_kb * 1024,
        extract: a.extract_attachments.clone(),
        dry_run: a.dry_run,
        want_dropped_meta: a.dropped_csv.is_some(),
        seen: Mutex::new(HashMap::new()),
    };
    if let (Some(dir), false) = (&cfg.extract, a.dry_run) {
        std::fs::create_dir_all(dir)?;
    }

    let data = open_mmap(&a.mbox)?;
    let open = |p: &Option<PathBuf>| -> Result<Option<MboxWriter>> {
        Ok(match (p, a.dry_run) {
            (Some(o), false) => {
                Some(MboxWriter::new(File::create(o).map_err(|e| {
                    format!("cannot create {}: {e}", o.display())
                })?))
            }
            _ => None,
        })
    };
    let mut writer = open(&a.out)?;
    let mut dropped_writer = open(&a.dropped_out)?;
    let mut rule_hits: Vec<(u64, u64)> = vec![(0, 0); cfg.drop.patterns.len()];
    let mut dropped_rows: Vec<Vec<String>> = Vec::new();

    let started = Instant::now();
    let pb = progress(
        data.len() as u64,
        if a.dry_run { "Dry run " } else { "Slimming" },
    );

    let (mut kept, mut kept_bytes_out, mut stripped, mut stripped_bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut dropped: HashMap<Reason, (u64, u64)> = HashMap::new();
    let mut dropped_senders: HashMap<String, (u64, u64)> = HashMap::new();
    let mut kept_domains: HashMap<String, (u64, u64)> = HashMap::new();
    let mut saved_rows: Vec<SavedRow> = Vec::new();

    run_batches(
        &data,
        &pb,
        |m| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process(m, &cfg))).unwrap_or(
                Outcome::Kept {
                    data: Cow::Borrowed(m),
                    sender: String::new(),
                    stripped: 0,
                    stripped_bytes: 0,
                    saved: vec![],
                },
            )
        },
        |batch| {
            for o in batch {
                match o {
                    Outcome::Dropped {
                        reason,
                        rule,
                        sender,
                        data,
                        date,
                        subject,
                    } => {
                        let size = data.len() as u64;
                        if let Some(i) = rule {
                            rule_hits[i].0 += 1;
                            rule_hits[i].1 += size;
                        }
                        if let Some(w) = dropped_writer.as_mut() {
                            w.write(data);
                        }
                        if a.dropped_csv.is_some() {
                            dropped_rows.push(vec![
                                match reason {
                                    Reason::Label => "label".into(),
                                    Reason::Bulk => "bulk".into(),
                                    Reason::Sender => "sender".into(),
                                },
                                rule.map(|i| cfg.drop.patterns[i].clone())
                                    .unwrap_or_default(),
                                sender.clone(),
                                date,
                                subject,
                                format!("{:.1}", size as f64 / 1024.0),
                            ]);
                        }
                        let e = dropped.entry(reason).or_default();
                        e.0 += 1;
                        e.1 += size;
                        let e = dropped_senders.entry(sender).or_default();
                        e.0 += 1;
                        e.1 += size;
                    }
                    Outcome::Kept {
                        data,
                        sender,
                        stripped: s,
                        stripped_bytes: sb,
                        saved,
                    } => {
                        kept += 1;
                        stripped += u64::from(s);
                        stripped_bytes += sb;
                        kept_bytes_out += data.len() as u64;
                        let e = kept_domains
                            .entry(domain_of(&sender).to_string())
                            .or_default();
                        e.0 += 1;
                        e.1 += data.len() as u64;
                        saved_rows.extend(saved);
                        if let Some(w) = writer.as_mut() {
                            w.write(&data);
                        }
                    }
                }
            }
        },
    );
    if let Some(w) = writer {
        w.finish()
            .map_err(|e| format!("writing output failed: {e}"))?;
    }
    if let Some(w) = dropped_writer {
        w.finish()
            .map_err(|e| format!("writing --dropped-out failed: {e}"))?;
    }
    let secs = started.elapsed().as_secs_f64();

    let in_bytes = data.len() as u64;
    let total_msgs = kept + dropped.values().map(|v| v.0).sum::<u64>();
    let pct = |x: u64| 100.0 * x as f64 / in_bytes.max(1) as f64;
    println!();
    println!(
        "Input:             {}  ({} msgs, {})",
        a.mbox.display(),
        total_msgs,
        fmt_bytes(in_bytes)
    );
    for (r, label) in [
        (Reason::Label, "label"),
        (Reason::Bulk, "bulk mail"),
        (Reason::Sender, "sender rule"),
    ] {
        if let Some((n, b)) = dropped.get(&r) {
            println!(
                "Dropped ({label:<11}) {n} msgs, {} ({:.1}%)",
                fmt_bytes(*b),
                pct(*b)
            );
        }
    }
    if cfg.strip {
        println!(
            "Attachments:       {stripped} stripped, {} removed",
            fmt_bytes(stripped_bytes)
        );
    }
    if cfg.extract.is_some() {
        let unique: HashSet<&str> = saved_rows.iter().map(|r| r.stored_as.as_str()).collect();
        let ubytes: u64 = {
            let mut seen = HashSet::new();
            saved_rows
                .iter()
                .filter(|r| seen.insert(r.stored_as.as_str()))
                .map(|r| r.bytes)
                .sum()
        };
        println!(
            "Extracted:         {} attachments → {} unique files, {}{}",
            saved_rows.len(),
            unique.len(),
            fmt_bytes(ubytes),
            if a.dry_run {
                " (dry run: nothing written)"
            } else {
                ""
            }
        );
    }
    println!(
        "Output:            {} msgs, {} ({:.1}% of input){}",
        kept,
        fmt_bytes(kept_bytes_out),
        pct(kept_bytes_out),
        match (&a.out, a.dry_run) {
            (Some(o), false) => format!("  → {}", o.display()),
            _ => "  (dry run: nothing written)".into(),
        }
    );
    println!(
        "Time:              {secs:.1}s  ({}/s)",
        fmt_bytes((in_bytes as f64 / secs.max(1e-9)) as u64)
    );

    if let (Some(o), false) = (&a.dropped_out, a.dry_run) {
        println!("Dropped mail:      saved to {}", o.display());
    }
    if !cfg.drop.is_empty() {
        let unmatched: Vec<&String> = cfg
            .drop
            .patterns
            .iter()
            .zip(&rule_hits)
            .filter(|(_, h)| h.0 == 0)
            .map(|(p, _)| p)
            .collect();
        println!(
            "Sender list:       {} patterns, {} matched mail, {} matched nothing",
            cfg.drop.patterns.len(),
            cfg.drop.patterns.len() - unmatched.len(),
            unmatched.len()
        );
        if !unmatched.is_empty() {
            println!(
                "\nPatterns that matched no message (typo? already handled by another rule?):"
            );
            for p in unmatched.iter().take(50) {
                println!("  {p}");
            }
            if unmatched.len() > 50 {
                println!("  … and {} more", unmatched.len() - 50);
            }
        }
    }

    let top = |m: &HashMap<String, (u64, u64)>, title: &str| {
        if m.is_empty() {
            return;
        }
        let mut v: Vec<_> = m.iter().collect();
        v.sort_unstable_by_key(|e| std::cmp::Reverse(e.1.1));
        println!("\n{title}");
        for (k, (n, b)) in v.into_iter().take(15) {
            let name = if k.is_empty() {
                "(unknown)".to_string()
            } else {
                truncate(k, 60)
            };
            println!("{:>10} {:>8}  {name}", fmt_bytes(*b), n);
        }
    };
    top(&dropped_senders, "Largest dropped senders:");
    top(
        &kept_domains,
        "Largest remaining sender domains (output size) — candidates for more --drop rules:",
    );

    if let (Some(path), false) = (&a.dropped_csv, a.dry_run) {
        write_csv(
            path,
            &[
                "reason",
                "matched_pattern",
                "from",
                "date",
                "subject",
                "size_mb",
            ],
            dropped_rows,
        )?;
        println!("\nDropped-message list: {}", path.display());
    }

    if let (Some(dir), false) = (&cfg.extract, a.dry_run) {
        let idx = dir.join("attachments_index.csv");
        write_csv(
            &idx,
            &[
                "stored_as",
                "original_name",
                "size_mb",
                "first_seen",
                "from",
                "date",
                "subject",
            ],
            saved_rows.iter().map(|r| {
                vec![
                    r.stored_as.clone(),
                    r.filename.clone(),
                    mb(r.bytes),
                    if r.new_file {
                        "yes".into()
                    } else {
                        "duplicate".into()
                    },
                    r.from.clone(),
                    r.date.clone(),
                    r.subject.clone(),
                ]
            }),
        )?;
        println!("\nAttachment index: {}", idx.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob(b"noreply@*", b"noreply@x.com"));
        assert!(glob(b"*@*.linkedin.com", b"a@e.linkedin.com"));
        assert!(!glob(b"*@*.linkedin.com", b"a@linkedin.com"));
    }

    #[test]
    fn list_lines() {
        assert_eq!(
            parse_list_line("a@x.com, B@y.com; c@z.com", false),
            vec!["a@x.com", "b@y.com", "c@z.com"]
        );
        assert_eq!(
            parse_list_line("\"Shop, Inc\" <news@shop.com>", false),
            vec!["news@shop.com"]
        );
        assert_eq!(
            parse_list_line("mailto:x@y.com\tlinkedin.com", false),
            vec!["x@y.com", "linkedin.com"]
        );
        assert!(parse_list_line("# comment", false).is_empty());
        assert_eq!(
            parse_list_line("news@shop.com,771,216.4", true),
            vec!["news@shop.com"]
        );
    }

    #[test]
    fn matcher_rule_index() {
        let m = Matcher::new(&["a@x.com b@y.com".into(), "@z.org".into()], &[]).unwrap();
        assert_eq!(m.patterns.len(), 3);
        assert_eq!(m.find("b@y.com"), Some(1));
        assert_eq!(m.find("q@sub.z.org"), Some(2));
        assert_eq!(m.find(""), None);
    }

    #[test]
    fn matcher() {
        let m = Matcher::new(
            &["@linkedin.com".into(), "x@y.com".into(), "news*@*".into()],
            &[],
        )
        .unwrap();
        assert!(m.matches("a@linkedin.com"));
        assert!(m.matches("a@e.linkedin.com"));
        assert!(!m.matches("a@notlinkedin.com"));
        assert!(m.matches("x@y.com"));
        assert!(m.matches("newsletter@shop.com"));
    }
}
