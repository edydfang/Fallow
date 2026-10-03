//! `fallow stats` — gmail-mbox-stats–style reports, plus size/attachment/bulk analysis.

use crate::mime::{self, Headers, domain_of, extract_addrs};
use crate::util::*;
use clap::Args;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Args)]
pub struct StatsArgs {
    /// Path to the .mbox file
    pub mbox: PathBuf,

    /// Your address; mail from it counts as sent by you (repeatable).
    /// If omitted, Gmail's "Sent" label is used to tell sent from received.
    #[arg(long = "me", value_name = "ADDR")]
    pub me: Vec<String>,

    /// Folder for CSV reports [default: mbox_stats_<timestamp>]
    #[arg(long, short)]
    pub out: Option<PathBuf>,

    /// Rows in the terminal tables
    #[arg(long, default_value_t = 25)]
    pub top: usize,

    /// Group the terminal sender table by full address or by domain
    #[arg(long, value_enum, default_value_t = By::Domain)]
    pub by: By,

    /// How many of the largest messages to list in largest_messages.csv
    #[arg(long, default_value_t = 1000)]
    pub largest: usize,

    /// Only print the summary; don't write CSV files
    #[arg(long)]
    pub no_csv: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq)]
pub enum By {
    Sender,
    Domain,
}

struct MsgInfo {
    size: u64,
    from: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    bulk: bool,
    labels: Vec<String>,
    att_count: u32,
    att_bytes: u64,
    att_names: Vec<String>,
    subject: String,
    date: String,
}

const WANTED: &[&str] = &[
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "date",
    "list-unsubscribe",
    "list-id",
    "precedence",
    "x-gmail-labels",
];

fn analyze(msg: &[u8]) -> MsgInfo {
    let start = mime::skip_from_line(msg);
    let (he, _) = mime::split_head_body(&msg[start..]);
    let h: Headers = mime::parse_headers(&msg[start..start + he], WANTED);
    let addrs = |n: &str| h.get(n).map(extract_addrs).unwrap_or_default();
    let mut att_count = 0;
    let mut att_bytes = 0;
    let mut att_names = Vec::new();
    for leaf in mime::leaves(msg) {
        if leaf.is_attachment {
            att_count += 1;
            att_bytes += leaf.decoded_len(msg);
            if let Some(n) = leaf.filename {
                att_names.push(n);
            }
        }
    }
    MsgInfo {
        size: msg.len() as u64,
        from: addrs("from").into_iter().next(),
        to: addrs("to"),
        cc: addrs("cc"),
        bcc: addrs("bcc"),
        bulk: mime::is_bulk(&h),
        labels: mime::gmail_labels(&h),
        att_count,
        att_bytes,
        att_names,
        subject: mime::decode_words(h.get("subject").unwrap_or("")),
        date: h.get("date").unwrap_or("").to_string(),
    }
}

#[derive(Default, Clone)]
struct Agg {
    msgs: u64,
    bytes: u64,
    att: u64,
    att_bytes: u64,
    bulk: u64,
}

impl Agg {
    fn add(&mut self, m: &MsgInfo) {
        self.msgs += 1;
        self.bytes += m.size;
        self.att += u64::from(m.att_count);
        self.att_bytes += m.att_bytes;
        self.bulk += u64::from(m.bulk);
    }
}

type Map = HashMap<String, Agg>;

#[derive(Default)]
struct Stats {
    total: Agg,
    bulk_bytes: u64,
    sent: Agg,
    received: Agg,
    labels: Map,
    in_senders: Map,
    in_sender_domains: Map,
    in_receivers: Map,
    in_cc: Map,
    my_receivers: Map,
    my_receiver_domains: Map,
    my_cc: Map,
    my_bcc: Map,
    detected_me: HashMap<String, u64>,
    has_labels: bool,
    largest: Vec<MsgInfo>,
    largest_cap: usize,
}

fn bump(map: &mut Map, key: &str, m: &MsgInfo) {
    if let Some(a) = map.get_mut(key) {
        a.add(m);
    } else {
        let mut a = Agg::default();
        a.add(m);
        map.insert(key.to_string(), a);
    }
}

impl Stats {
    fn absorb(&mut self, m: MsgInfo, me: &HashSet<String>) {
        self.total.add(&m);
        if m.bulk {
            self.bulk_bytes += m.size;
        }
        if !m.labels.is_empty() {
            self.has_labels = true;
        }
        for l in &m.labels {
            bump(&mut self.labels, l, &m);
        }
        let from_me = match &m.from {
            Some(f) if !me.is_empty() => me.contains(f),
            Some(_) => m.labels.iter().any(|l| l == "Sent"),
            None => false,
        };
        if from_me {
            self.sent.add(&m);
            if me.is_empty() {
                if let Some(f) = &m.from {
                    *self.detected_me.entry(f.clone()).or_default() += 1;
                }
            }
            let mut doms = HashSet::new();
            for r in &m.to {
                bump(&mut self.my_receivers, r, &m);
                doms.insert(domain_of(r).to_string());
            }
            for r in &m.cc {
                bump(&mut self.my_cc, r, &m);
                doms.insert(domain_of(r).to_string());
            }
            for r in &m.bcc {
                bump(&mut self.my_bcc, r, &m);
                doms.insert(domain_of(r).to_string());
            }
            for d in &doms {
                bump(&mut self.my_receiver_domains, d, &m);
            }
        } else {
            self.received.add(&m);
            let s = m.from.as_deref().unwrap_or("(unknown)");
            bump(&mut self.in_senders, s, &m);
            bump(&mut self.in_sender_domains, domain_of(s), &m);
            for r in &m.to {
                bump(&mut self.in_receivers, r, &m);
            }
            for r in &m.cc {
                bump(&mut self.in_cc, r, &m);
            }
        }
        if self.largest_cap > 0 {
            self.largest.push(m);
            if self.largest.len() >= self.largest_cap * 2 {
                self.trim_largest();
            }
        }
    }

    fn trim_largest(&mut self) {
        self.largest
            .sort_unstable_by_key(|m| std::cmp::Reverse(m.size));
        self.largest.truncate(self.largest_cap);
    }
}

fn sorted(map: &Map) -> Vec<(&String, &Agg)> {
    let mut v: Vec<_> = map.iter().collect();
    v.sort_unstable_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(b.1.msgs.cmp(&a.1.msgs)));
    v
}

const AGG_HEADER: [&str; 6] = [
    "messages",
    "size_mb",
    "attachments",
    "attachment_mb",
    "bulk_messages",
    "pct_of_total_size",
];

fn agg_rows(map: &Map, total: u64) -> impl Iterator<Item = Vec<String>> + '_ {
    let t = total.max(1) as f64;
    sorted(map).into_iter().map(move |(k, a)| {
        vec![
            k.clone(),
            a.msgs.to_string(),
            mb(a.bytes),
            a.att.to_string(),
            mb(a.att_bytes),
            a.bulk.to_string(),
            format!("{:.2}", 100.0 * a.bytes as f64 / t),
        ]
    })
}

fn print_table(title: &str, key: &str, map: &Map, total: u64, n: usize) {
    if map.is_empty() {
        return;
    }
    let t = total.max(1) as f64;
    println!("\n{title}\n");
    println!(
        "{:>10} {:>6} {:>8} {:>10} {:>7}  {key}",
        "Size", "%", "Msgs", "Attach", "Bulk"
    );
    for (k, a) in sorted(map).into_iter().take(n) {
        println!(
            "{:>10} {:>6.1} {:>8} {:>10} {:>7}  {}",
            fmt_bytes(a.bytes),
            100.0 * a.bytes as f64 / t,
            a.msgs,
            fmt_bytes(a.att_bytes),
            a.bulk,
            truncate(k, 60)
        );
    }
}

pub fn run(a: StatsArgs) -> Result<()> {
    let data = open_mmap(&a.mbox)?;
    let me: HashSet<String> = a.me.iter().map(|s| s.trim().to_ascii_lowercase()).collect();
    let started = Instant::now();
    let pb = progress(data.len() as u64, "Scanning");
    let mut st = Stats {
        largest_cap: if a.no_csv { 0 } else { a.largest },
        ..Default::default()
    };
    run_batches(
        &data,
        &pb,
        |m| {
            std::panic::catch_unwind(|| analyze(m)).unwrap_or_else(|_| MsgInfo {
                size: m.len() as u64,
                from: None,
                to: vec![],
                cc: vec![],
                bcc: vec![],
                bulk: false,
                labels: vec![],
                att_count: 0,
                att_bytes: 0,
                att_names: vec![],
                subject: "(unparseable)".into(),
                date: String::new(),
            })
        },
        |batch| {
            for m in batch {
                st.absorb(m, &me);
            }
        },
    );
    st.trim_largest();
    let secs = started.elapsed().as_secs_f64();
    let tb = st.total.bytes;
    let pct = |x: u64| 100.0 * x as f64 / tb.max(1) as f64;

    println!();
    println!("File:              {}", a.mbox.display());
    println!("Messages:          {}", st.total.msgs);
    println!("Total size:        {}", fmt_bytes(tb));
    println!(
        "Attachments:       {} files, {} decoded (~{:.1}% of file incl. encoding overhead)",
        st.total.att,
        fmt_bytes(st.total.att_bytes),
        pct(st.total.att_bytes * 4 / 3).min(100.0)
    );
    println!(
        "Bulk/newsletters:  {} msgs, {} ({:.1}%)  [List-Unsubscribe / List-Id / Precedence]",
        st.total.bulk,
        fmt_bytes(st.bulk_bytes),
        pct(st.bulk_bytes)
    );
    println!(
        "Sent by you:       {} msgs, {}",
        st.sent.msgs,
        fmt_bytes(st.sent.bytes)
    );
    println!(
        "Received:          {} msgs, {}",
        st.received.msgs,
        fmt_bytes(st.received.bytes)
    );
    if me.is_empty() {
        if let Some((addr, _)) = st.detected_me.iter().max_by_key(|e| e.1) {
            println!(
                "Your address:      {addr}  (detected from Gmail \"Sent\" label; override with --me)"
            );
        } else {
            println!(
                "Your address:      unknown — pass --me you@example.com for sent/received split"
            );
        }
    }
    println!(
        "Scan time:         {secs:.1}s  ({}/s)",
        fmt_bytes((tb as f64 / secs.max(1e-9)) as u64)
    );

    if st.has_labels {
        print_table(
            "Gmail labels by size (a message can have several):",
            "Label",
            &st.labels,
            tb,
            a.top,
        );
    }
    match a.by {
        By::Domain => print_table(
            "Top received-mail sender domains by size:",
            "Domain",
            &st.in_sender_domains,
            tb,
            a.top,
        ),
        By::Sender => print_table(
            "Top received-mail senders by size:",
            "Sender",
            &st.in_senders,
            tb,
            a.top,
        ),
    }

    if a.no_csv {
        return Ok(());
    }
    let out = a
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("mbox_stats_{}", timestamp())));
    std::fs::create_dir_all(&out)?;
    let j = |name: &str| out.join(name);

    let g = |label: &str, v: String| vec![label.to_string(), v];
    write_csv(
        &j("general.csv"),
        &["metric", "value"],
        vec![
            g("file", a.mbox.display().to_string()),
            g("messages", st.total.msgs.to_string()),
            g("total_mb", mb(tb)),
            g("attachments", st.total.att.to_string()),
            g("attachment_mb_decoded", mb(st.total.att_bytes)),
            g("bulk_messages", st.total.bulk.to_string()),
            g("bulk_mb", mb(st.bulk_bytes)),
            g("sent_messages", st.sent.msgs.to_string()),
            g("sent_mb", mb(st.sent.bytes)),
            g("received_messages", st.received.msgs.to_string()),
            g("received_mb", mb(st.received.bytes)),
            g("unique_received_senders", st.in_senders.len().to_string()),
            g(
                "unique_received_sender_domains",
                st.in_sender_domains.len().to_string(),
            ),
            g(
                "unique_people_you_wrote_to",
                st.my_receivers.len().to_string(),
            ),
            g("scan_seconds", format!("{secs:.1}")),
        ],
    )?;

    let files: Vec<(&str, &str, &Map)> = vec![
        ("received_senders.csv", "sender", &st.in_senders),
        (
            "received_sender_domains.csv",
            "domain",
            &st.in_sender_domains,
        ),
        ("received_to.csv", "receiver", &st.in_receivers),
        ("received_cc.csv", "cc", &st.in_cc),
        ("sent_to.csv", "receiver", &st.my_receivers),
        ("sent_to_domains.csv", "domain", &st.my_receiver_domains),
        ("sent_cc.csv", "cc", &st.my_cc),
        ("sent_bcc.csv", "bcc", &st.my_bcc),
        ("labels.csv", "label", &st.labels),
    ];
    let mut written = vec!["general.csv".to_string()];
    for (name, key, map) in files {
        if map.is_empty() {
            continue;
        }
        let mut header = vec![key];
        header.extend(AGG_HEADER);
        write_csv(&j(name), &header, agg_rows(map, tb))?;
        written.push(name.to_string());
    }
    if !st.largest.is_empty() {
        write_csv(
            &j("largest_messages.csv"),
            &[
                "size_mb",
                "attachment_mb",
                "attachments",
                "from",
                "date",
                "subject",
                "labels",
                "attachment_names",
            ],
            st.largest.iter().map(|m| {
                vec![
                    mb(m.size),
                    mb(m.att_bytes),
                    m.att_count.to_string(),
                    m.from.clone().unwrap_or_default(),
                    m.date.clone(),
                    m.subject.clone(),
                    m.labels.join("; "),
                    m.att_names.join("; "),
                ]
            }),
        )?;
        written.push("largest_messages.csv".into());
    }
    println!(
        "\nCSV reports in {}/: {}",
        out.display(),
        written.join(", ")
    );
    Ok(())
}
