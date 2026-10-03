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
use std::io::{BufRead, BufReader, BufWriter, Write};
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

    /// File with one --drop pattern per line (# comments allowed)
    #[arg(long, value_name = "FILE")]
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

    /// Report what would happen without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

// ------------------------------------------------------------ sender matching

#[derive(Default)]
struct Matcher {
    exact: HashSet<String>,
    domains: Vec<String>,
    globs: Vec<String>,
}

impl Matcher {
    fn new(patterns: &[String], files: &[PathBuf]) -> Result<Self> {
        let mut m = Matcher::default();
        let mut all: Vec<String> = patterns.to_vec();
        for f in files {
            let r = BufReader::new(
                File::open(f).map_err(|e| format!("cannot open {}: {e}", f.display()))?,
            );
            for line in r.lines() {
                all.push(line?);
            }
        }
        for p in all {
            // allow pasting the first column of a stats CSV: "addr,123,..."
            let p = p
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('\u{feff}')
                .to_ascii_lowercase();
            if p.is_empty() || p.starts_with('#') || p == "sender" || p == "domain" {
                continue;
            }
            if p.contains('*') {
                m.globs.push(p);
            } else if let Some(d) = p.strip_prefix('@') {
                m.domains.push(d.to_string());
            } else if p.contains('@') {
                m.exact.insert(p);
            } else {
                m.domains.push(p);
            }
        }
        Ok(m)
    }

    fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.domains.is_empty() && self.globs.is_empty()
    }

    fn matches(&self, addr: &str) -> bool {
        if self.exact.contains(addr) {
            return true;
        }
        let dom = domain_of(addr);
        if self.domains.iter().any(|d| {
            dom == d
                || (dom.len() > d.len()
                    && dom.ends_with(d.as_str())
                    && dom.as_bytes()[dom.len() - d.len() - 1] == b'.')
        }) {
            return true;
        }
        self.globs
            .iter()
            .any(|g| glob(g.as_bytes(), addr.as_bytes()))
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
        sender: String,
        size: u64,
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
    let size = msg.len() as u64;

    let protected = !cfg.keep.is_empty() && cfg.keep.matches(&sender);
    if !protected {
        if !cfg.drop_labels.is_empty() {
            let labels = mime::gmail_labels(&h);
            if labels
                .iter()
                .any(|l| cfg.drop_labels.iter().any(|d| d.eq_ignore_ascii_case(l)))
            {
                return Outcome::Dropped {
                    reason: Reason::Label,
                    sender,
                    size,
                };
            }
        }
        if cfg.drop_bulk && mime::is_bulk(&h) {
            return Outcome::Dropped {
                reason: Reason::Bulk,
                sender,
                size,
            };
        }
        if !cfg.drop.is_empty() && cfg.drop.matches(&sender) {
            return Outcome::Dropped {
                reason: Reason::Sender,
                sender,
                size,
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
    let cfg = Cfg {
        drop: Matcher::new(&a.drop, &a.drop_file)?,
        keep: Matcher::new(&a.keep, &a.keep_file)?,
        drop_bulk: a.drop_bulk,
        drop_labels: a.drop_label.clone(),
        strip: a.strip_attachments,
        min_bytes: a.min_attachment_kb * 1024,
        extract: a.extract_attachments.clone(),
        dry_run: a.dry_run,
        seen: Mutex::new(HashMap::new()),
    };
    if let (Some(dir), false) = (&cfg.extract, a.dry_run) {
        std::fs::create_dir_all(dir)?;
    }

    let data = open_mmap(&a.mbox)?;
    let mut writer: Option<BufWriter<File>> = match (&a.out, a.dry_run) {
        (Some(o), false) => Some(BufWriter::with_capacity(8 << 20, File::create(o)?)),
        _ => None,
    };

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
    let mut last_nl = true;
    let mut io_err: Option<std::io::Error> = None;

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
                        sender,
                        size,
                    } => {
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
                            if io_err.is_none() {
                                let r = (|| {
                                    if !last_nl {
                                        w.write_all(b"\n")?;
                                    }
                                    w.write_all(&data)
                                })();
                                if let Err(e) = r {
                                    io_err = Some(e);
                                }
                            }
                            last_nl = data.ends_with(b"\n");
                        }
                    }
                }
            }
        },
    );
    if let Some(e) = io_err {
        return Err(format!("write failed: {e}").into());
    }
    if let Some(mut w) = writer {
        w.flush()?;
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
