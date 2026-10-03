//! Splitting an mbox buffer into messages.

use memchr::{memchr, memmem};

/// A "From " separator line. Body lines starting with "From " are normally
/// escaped (">From "), but to be safe we also require the usual shape:
/// `From <sender> <date with HH:MM:SS> ... <year>` — or, failing that, a
/// preceding blank line plus a time-looking token.
fn is_separator(line: &[u8], prev_blank: bool) -> bool {
    if !line.starts_with(b"From ") {
        return false;
    }
    let mut rest = &line[5..];
    while let Some((&last, head)) = rest.split_last() {
        if last.is_ascii_whitespace() {
            rest = head;
        } else {
            break;
        }
    }
    if rest.is_empty() || rest[0] == b' ' {
        return false;
    }
    let has_colon = rest.contains(&b':');
    let n = rest.len();
    let ends_with_year = n >= 4 && rest[n - 4..].iter().all(u8::is_ascii_digit);
    (has_colon && ends_with_year) || (prev_blank && has_colon)
}

/// Iterator over (start, end) byte ranges of messages, including the From_ line.
pub struct Splitter<'a> {
    data: &'a [u8],
    pos: usize,
    finder: memmem::Finder<'static>,
}

impl<'a> Splitter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Splitter {
            data,
            pos: 0,
            finder: memmem::Finder::new(b"\nFrom "),
        }
    }
}

impl Iterator for Splitter<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let data = self.data;
        if self.pos >= data.len() {
            return None;
        }
        let start = self.pos;
        let mut search = start + 1;
        loop {
            if search >= data.len() {
                self.pos = data.len();
                return Some((start, data.len()));
            }
            match self.finder.find(&data[search..]) {
                None => {
                    self.pos = data.len();
                    return Some((start, data.len()));
                }
                Some(off) => {
                    let nl = search + off;
                    let ls = nl + 1;
                    let le = memchr(b'\n', &data[ls..]).map_or(data.len(), |x| ls + x);
                    let prev_blank = nl >= 1
                        && (data[nl - 1] == b'\n'
                            || (data[nl - 1] == b'\r' && nl >= 2 && data[nl - 2] == b'\n'));
                    if is_separator(&data[ls..le], prev_blank) {
                        self.pos = ls;
                        return Some((start, ls));
                    }
                    search = ls;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_messages() {
        let d = b"From a@x Mon Jan  1 00:00:00 2024\nSubject: 1\n\nFrom here on\n\nFrom b@y Tue Jan  2 00:00:00 2024\nSubject: 2\n\nbody\n";
        let v: Vec<_> = Splitter::new(d).collect();
        assert_eq!(v.len(), 2);
        assert!(d[v[1].0..].starts_with(b"From b@y"));
    }

    #[test]
    fn gmail_from_line() {
        assert!(is_separator(
            b"From 1781234567890123456@xxx Wed Mar 13 14:22:33 +0000 2024",
            false
        ));
        assert!(!is_separator(b"From the desk of John", true));
    }
}
