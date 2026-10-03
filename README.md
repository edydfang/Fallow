# fallow

Shrink and clean up a large `.mbox` email archive, such as a Gmail Takeout export.

- **`fallow stats`** shows what is taking up space: which senders, which Gmail labels, and how much is attachments.
- **`fallow slim`** writes a **new, smaller mbox**. It can remove mail from senders you list, newsletters, spam or trash, and replace attachments with a short note.

Your original file is never changed.

---

## Install

1. Download the file for your system from [Releases](https://github.com/edydfang/Fallow/releases/latest):

   | System | File |
   |---|---|
   | Windows | `fallow-…-x86_64-pc-windows-msvc.zip` |
   | Mac (M1/M2/M3/M4) | `fallow-…-aarch64-apple-darwin.tar.gz` |
   | Mac (Intel) | `fallow-…-x86_64-apple-darwin.tar.gz` |
   | Linux | `fallow-…-x86_64-unknown-linux-musl.tar.gz` |

2. Unzip it. Inside is a single program, `fallow` (`fallow.exe` on Windows).
3. Open a terminal in that folder: PowerShell on Windows, Terminal on Mac.
   - **Windows:** type `.\fallow.exe` instead of `fallow` in the examples below.
   - **Mac:** the first time, run `xattr -d com.apple.quarantine ./fallow`, then use `./fallow`.

Check that it works:

```
fallow --version
```

Put quotes around any file path that contains spaces, for example `"All mail Including Spam and Trash.mbox"`.

---

## Quick start

These three steps cover the usual workflow.

**1. See where the space goes**

```
fallow stats archive.mbox
```

The terminal shows the totals and the biggest senders. A folder named `mbox_stats_<date>/` is created with spreadsheets. See [What `stats` produces](#what-stats-produces).

**2. Preview a cleanup. Nothing is written yet.**

```
fallow slim archive.mbox --dry-run --senders senders.txt --drop-bulk --strip-attachments
```

The preview prints how many messages would be removed, how big the result would be, and which senders still take the most space.

**3. Run it for real.** Remove `--dry-run` and name the output file with `-o`:

```
fallow slim archive.mbox -o cleaned.mbox --senders senders.txt --drop-bulk --strip-attachments
```

---

## Common tasks

### Remove all mail from certain senders

1. Create a text file, for example `senders.txt`, with the senders to remove:

   ```
   # Lines starting with # are ignored
   news@shop.com
   a@x.com, b@y.com, c@z.com
   "Shop, Inc" <deals@shop.com>
   linkedin.com
   noreply@*
   ```

   - One or more addresses per line, separated by commas, semicolons or spaces.
   - A line copied from a mail client, like `"Name" <address>`, works as is.
   - A bare domain such as `linkedin.com` removes everyone at that domain, including subdomains like `mail.linkedin.com`.
   - `*` is a wildcard: `noreply@*` matches any `noreply` address.
   - Capitalization doesn't matter.

2. Run:

   ```
   fallow slim archive.mbox -o cleaned.mbox --senders senders.txt --dropped-out removed.mbox
   ```

   - `cleaned.mbox` contains everything except those senders' mail.
   - `removed.mbox` contains just the removed mail, so nothing is lost. Together the two files are exactly the original.

The summary at the end lists any line in your file that matched no mail at all. That usually means a typo.

> **Shortcut:** in `received_senders.csv` from `fallow stats`, delete the rows you want to **keep**,
> save, and pass that file directly: `--senders received_senders.csv`.

For one or two senders you can skip the file: `--drop news@shop.com --drop linkedin.com`.

### Remove newsletters, spam and trash

```
fallow slim archive.mbox -o cleaned.mbox --drop-bulk --drop-label Spam --drop-label Trash
```

- `--drop-bulk` removes newsletters and marketing mail, meaning anything with an unsubscribe header.
- `--drop-label` removes mail carrying a Gmail label. This only works for Gmail exports. Other useful labels: `"Category Promotions"`, `"Category Social"`, `"Category Updates"`.

### Shrink attachments

```
fallow slim archive.mbox -o cleaned.mbox --strip-attachments --min-attachment-kb 100
```

Each attachment of at least 100 KB is replaced inside the email by a line like
`[Attachment removed by fallow: "report.pdf" (application/pdf, 1.4 MB)]`. Small ones, such as signature logos, stay.

To **keep the files** while removing them from the mbox, add `--extract-attachments attachments`.
Every attachment is saved once into the `attachments` folder, even if it was sent many times.
An `attachments_index.csv` in that folder records which email each file came from.

### Never remove certain people

Add `--keep boss@work.com`, or `--keep-file vip.txt` with the same file format as `senders.txt`.
These senders are protected from **every** removal rule above.

### Combine everything

Options can be mixed freely:

```
fallow slim archive.mbox -o cleaned.mbox \
    --senders senders.txt --keep-file vip.txt \
    --drop-bulk --drop-label Spam --drop-label Trash \
    --strip-attachments --min-attachment-kb 100 --extract-attachments attachments \
    --dropped-out removed.mbox --dropped-csv removed.csv
```

On Windows PowerShell, put it all on one line, or end each line with a backtick `` ` `` instead of `\`.

---

## Reference

### `fallow slim` options

**Required:** the input mbox, and `-o OUTPUT.mbox` unless you use `--dry-run`.

| What to remove | |
|---|---|
| `--senders FILE` | Remove mail from every sender listed in FILE. `--drop-file` is the same option. |
| `--drop SENDER` | Remove mail from one sender, domain or `*` pattern. Repeat for more. |
| `--drop-bulk` | Remove newsletters and marketing mail. |
| `--drop-label LABEL` | Remove mail with this Gmail label. Repeat for more. |
| `--keep SENDER` / `--keep-file FILE` | Never remove mail from these senders. |

| Attachments | |
|---|---|
| `--strip-attachments` | Replace attachments with a one-line note. |
| `--min-attachment-kb N` | Only handle attachments of at least N KB. The default is all. |
| `--extract-attachments DIR` | Save attachments, with duplicates removed, into DIR. |

| Output and safety | |
|---|---|
| `-o FILE` | Where to write the cleaned mbox. |
| `--dry-run` | Only show what would happen. Nothing is written. |
| `--dropped-out FILE` | Also save all removed mail to this mbox. |
| `--dropped-csv FILE` | A spreadsheet of every removed email: why it was removed, sender, date, subject, size. |

### `fallow stats` options

| | |
|---|---|
| `-o DIR` | Folder for the spreadsheets. The default is `mbox_stats_<date>`. |
| `--me ADDRESS` | Your own address, to separate mail you sent from mail you received. For Gmail exports this is detected automatically. |
| `--by sender` | Show individual senders in the terminal instead of domains. |
| `--top N` | Number of rows to show in the terminal. The default is 25. |
| `--no-csv` | Print the summary only, without writing spreadsheets. |

### What `stats` produces

All files open in Excel. Rows are sorted by size, largest first.

| File | What's in it |
|---|---|
| `received_senders.csv` | Everyone who sent you mail: number of messages, size, attachments, newsletters |
| `received_sender_domains.csv` | The same, grouped by domain |
| `labels.csv` | Size of each Gmail label |
| `largest_messages.csv` | The 1000 biggest emails, with sender, date, subject and attachment names |
| `sent_to.csv`, `sent_to_domains.csv`, `sent_cc.csv`, `sent_bcc.csv` | People you wrote to |
| `received_to.csv`, `received_cc.csv` | Recipients on the mail you received |
| `general.csv` | Overall totals |

---

## Good to know

- The cleaned file is a normal mbox. Thunderbird, Apple Mail, `mbox-to-sqlite`, GYB and similar tools can open it.
- Apart from replaced attachments, every kept email is copied exactly as it was.
- On a Gmail export, stripping attachments alone usually cuts the file to about 5–15% of its size.
- Attachment sizes shown are the real file sizes. Inside the mbox they take about 37% more space because of encoding.
- An email whose whole body is a single attachment, with no text part, is counted but not stripped.
- If an email can't be parsed, it is copied through unchanged, and the run continues.

## Speed

The file is memory-mapped and parsed on all CPU cores, so memory use stays low even for very large files.
These timings are from a 2-core Linux machine with the file in page cache. On a laptop, disk read speed is usually the limit.

| Test file | Earlier Python script | `fallow stats` | `fallow slim` |
|---|---|---|---|
| 3.2 GB, 600k small emails | 143 s | 3.5 s | 4.7 s |
| 2.0 GB, 7k emails with attachments | 31 s | 0.8 s | 2.5 s |

## Development

Build from source with Rust 1.85 or newer (<https://rustup.rs>): `cargo build --release`.
The binary ends up at `target/release/fallow`.

Every push and pull request runs format, lint and test checks on Windows, Linux and macOS.

To publish a release, bump `version` in `Cargo.toml` and push to `main`.
The Release workflow then builds every platform, creates the `vX.Y.Z` tag, and publishes the
downloads with a `SHA256SUMS.txt` file.
