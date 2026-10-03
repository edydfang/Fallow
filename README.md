# fallow

Fast mbox analyzer and slimmer, written in Rust. It does the job of
[gmail-mbox-stats](https://github.com/leodevbro/gmail-mbox-stats) (sender, receiver, CC/BCC
and domain frequency CSVs) and adds size analysis and a `slim` command that writes a smaller mbox.

Speed on a 2-core Linux box with the file in page cache. On a laptop SSD, disk read speed is
usually the limit.

| Test file | Python script | `fallow stats` | `fallow slim` |
|---|---|---|---|
| 3.2 GB, 600k small messages | 143 s | 3.5 s | 4.7 s |
| 2.0 GB, 7k messages with attachments | 31 s | 0.8 s | 2.5 s (with extraction) |

The file is memory-mapped and parsed on all CPU cores. Memory use stays low no matter how big the mbox is.

## Install

Download a prebuilt binary from [Releases](https://github.com/edydfang/Fallow/releases):

| Platform | File |
|---|---|
| Windows (x64 / ARM) | `fallow-vX.Y.Z-x86_64-pc-windows-msvc.zip` / `…-aarch64-pc-windows-msvc.zip` |
| Linux x64 (static, any distro) | `fallow-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon / Intel | `fallow-vX.Y.Z-aarch64-apple-darwin.tar.gz` / `…-x86_64-apple-darwin.tar.gz` |

On macOS, clear the download quarantine once: `xattr -d com.apple.quarantine ./fallow`.

Or build from source with Rust 1.85+ (<https://rustup.rs>): `cargo build --release`.

## Releasing

CI runs format, clippy and tests on Windows, Linux and macOS for every push and PR.
To publish a release, bump `version` in `Cargo.toml` and push to `main`. The Release workflow
sees there's no release for that version yet, builds all five targets, creates the `vX.Y.Z` tag,
and publishes the archives plus `SHA256SUMS.txt` with notes generated from the commit history.
Pushing a matching `v*` tag, or running the workflow manually from the Actions tab, works too.

## 1. Analyze

```
fallow stats "All mail Including Spam and Trash.mbox"
fallow stats archive.mbox --me you@gmail.com --by sender --top 50 -o report
```

The terminal shows totals, attachment and bulk-mail share, Gmail labels by size, and the top
sender domains. CSV files go to `mbox_stats_<timestamp>/` (UTF-8 with BOM, so Excel opens them correctly):

| File | Contents |
|---|---|
| `general.csv` | Totals |
| `received_senders.csv`, `received_sender_domains.csv` | Who sends you the most mail, by size |
| `received_to.csv`, `received_cc.csv` | Recipients of incoming mail |
| `sent_to.csv`, `sent_to_domains.csv`, `sent_cc.csv`, `sent_bcc.csv` | Who you write to |
| `labels.csv` | Gmail labels (Takeout exports), with size per label |
| `largest_messages.csv` | The 1000 biggest messages, with subject, date and attachment names |

Without `--me`, Gmail's `Sent` label decides which mail was sent by you.

## 2. Slim

Do a `--dry-run` first. It prints what would be dropped, plus the largest remaining
sender domains as candidates for more rules:

```
fallow slim archive.mbox --dry-run --drop-bulk --drop-label Spam --drop-label Trash \
    --drop-label "Category Promotions" --drop "noreply@*" --drop linkedin.com
```

Then run it for real:

```
fallow slim archive.mbox -o slim.mbox \
    --drop-bulk --drop-label Spam --drop-label Trash --drop-file junk.txt --keep-file vip.txt \
    --strip-attachments --min-attachment-kb 100 --extract-attachments attachments/
```

| Option | Effect |
|---|---|
| `--drop PATTERN` / `--drop-file FILE` | Drop by sender. `a@b.com` matches one address. `b.com` or `@b.com` matches a domain and its subdomains. `noreply@*` or `*@*.linkedin.com` are globs. A file holds one pattern per line; you can paste the first column of `received_senders.csv`. |
| `--keep PATTERN` / `--keep-file FILE` | Never drop these senders. This overrides every drop rule. |
| `--drop-bulk` | Drop mail with `List-Unsubscribe`, `List-Id` or `Precedence: bulk/list`, i.e. newsletters and marketing. |
| `--drop-label LABEL` | Drop mail with this Gmail label: `Spam`, `Trash`, `Category Promotions`, `Category Social`, `Category Updates`, and so on. |
| `--strip-attachments` | Replace each attachment with a placeholder like `[Attachment removed by fallow: "report.pdf" (application/pdf, 1.4 MB)]`. |
| `--min-attachment-kb N` | Strip or extract only attachments of at least N KB, so small signature images stay. |
| `--extract-attachments DIR` | Save each attachment, decoded and de-duplicated by content hash, as `<hash>_<name>`. An `attachments_index.csv` records which mail each file came from. |

The input file is never modified. Everything in a kept message other than the stripped parts
is copied byte-for-byte, and the output is a normal mbox that Thunderbird, Apple Mail,
`mbox-to-sqlite`, GYB and similar tools can read.

A typical result: a Gmail export shrinks to roughly 5–15% of its size once attachments are stripped.

## Notes

- Messages whose entire body is one attachment (not multipart) are counted, but not stripped.
- Attachment sizes are decoded sizes. On disk, base64 adds about 37%.
- If a single message fails to parse, it is passed through unchanged and does not abort the run.
