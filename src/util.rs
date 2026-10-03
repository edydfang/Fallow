//! Shared plumbing: mmap, parallel batch driver, progress bar, CSV, formatting.

use crate::mbox::Splitter;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn open_mmap(path: &Path) -> Result<Mmap> {
    let f = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if f.metadata()?.len() == 0 {
        return Err(format!("{} is empty", path.display()).into());
    }
    // SAFETY: read-only mapping; the file must not be truncated while we run.
    let m = unsafe { Mmap::map(&f)? };
    #[cfg(unix)]
    {
        let _ = m.advise(memmap2::Advice::Sequential);
    }
    Ok(m)
}

pub fn progress(total: u64, label: &str) -> ProgressBar {
    let pb = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr_with_hz(5));
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix} [{elapsed_precise}] {wide_bar:.cyan/blue} {binary_bytes}/{binary_total_bytes} \
             {binary_bytes_per_sec} ETA {eta}  {msg}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_prefix(label.to_string());
    pb
}

/// Split `data` into messages and process them in parallel batches.
/// `work` runs on worker threads; `sink` receives each batch's results in file order.
pub fn run_batches<'d, R, W, S>(data: &'d [u8], pb: &ProgressBar, work: W, mut sink: S)
where
    R: Send,
    W: Fn(&'d [u8]) -> R + Sync,
    S: FnMut(Vec<R>),
{
    const BATCH_BYTES: usize = 64 << 20;
    const BATCH_MSGS: usize = 16_384;
    let mut split = Splitter::new(data);
    let mut batch: Vec<(usize, usize)> = Vec::with_capacity(BATCH_MSGS);
    let mut msgs: u64 = 0;
    loop {
        batch.clear();
        let mut bytes = 0usize;
        for r in split.by_ref() {
            bytes += r.1 - r.0;
            batch.push(r);
            if bytes >= BATCH_BYTES || batch.len() >= BATCH_MSGS {
                break;
            }
        }
        if batch.is_empty() {
            break;
        }
        let results: Vec<R> = batch.par_iter().map(|&(s, e)| work(&data[s..e])).collect();
        msgs += results.len() as u64;
        sink(results);
        pb.inc(bytes as u64);
        pb.set_message(format!("{msgs} msgs"));
    }
    pb.finish_with_message(format!("{msgs} msgs"));
}

pub fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn mb(n: u64) -> String {
    format!("{:.3}", n as f64 / 1_048_576.0)
}

/// UTC timestamp like 20261003-024400Z (no chrono dependency).
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// CSV with a UTF-8 BOM so Excel shows non-ASCII (e.g. Chinese) correctly.
pub fn write_csv(
    path: &Path,
    header: &[&str],
    rows: impl IntoIterator<Item = Vec<String>>,
) -> Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(b"\xEF\xBB\xBF")?;
    writeln!(w, "{}", header.join(","))?;
    for r in rows {
        let line: Vec<String> = r.iter().map(|f| csv_field(f)).collect();
        writeln!(w, "{}", line.join(","))?;
    }
    w.flush()?;
    Ok(())
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
