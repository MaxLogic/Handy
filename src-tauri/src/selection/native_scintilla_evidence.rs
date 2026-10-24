//! Pure decisions for the narrowly admitted native Scintilla adapter.

pub(super) const MAX_BYTES: usize = 524_288;
const MAX_SCALARS: usize = 131_072;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Unsupported,
    Disabled,
    ReadOnly,
    MultipleSelections,
    InvalidPosition,
    TooLarge,
    InvalidText,
    Changed,
    Timeout,
    Identity,
    ReadFailed,
}

pub(super) fn tested_version(ms: u32, ls: u32) -> bool {
    ms == (8 << 16 | 9) && ls == (8 << 16 | 1)
}

/// A successful scalar query may legitimately return zero. Transport success
/// and the whole operation's deadline are independent of the message result.
pub(super) fn scalar_result(
    delivered: bool,
    within_deadline: bool,
    value: usize,
) -> Result<isize, Error> {
    if !delivered || !within_deadline {
        return Err(Error::Timeout);
    }
    Ok(value as isize)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    pub document: usize,
    pub length: isize,
    pub codepage: isize,
    pub read_only: bool,
    pub selections: isize,
    pub mode: isize,
    pub main: isize,
    pub anchor: isize,
    pub caret: isize,
    pub anchor_virtual: isize,
    pub caret_virtual: isize,
    pub start: isize,
    pub end: isize,
}

impl Metadata {
    pub(super) fn range(&self) -> Result<(usize, usize), Error> {
        if self.document == 0 || self.codepage != 65001 || self.mode != 0 {
            return Err(Error::Unsupported);
        }
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if self.selections != 1 || self.main != 0 {
            return Err(Error::MultipleSelections);
        }
        if self.anchor_virtual != 0 || self.caret_virtual != 0 {
            return Err(Error::Unsupported);
        }
        if self.length < 0
            || self.anchor < 0
            || self.caret < 0
            || self.anchor > self.length
            || self.caret > self.length
            || self.start != self.anchor.min(self.caret)
            || self.end != self.anchor.max(self.caret)
        {
            return Err(Error::InvalidPosition);
        }
        let start = usize::try_from(self.start).map_err(|_| Error::InvalidPosition)?;
        let end = usize::try_from(self.end).map_err(|_| Error::InvalidPosition)?;
        let count = end.checked_sub(start).ok_or(Error::InvalidPosition)?;
        if count > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        Ok((start, count))
    }
}

pub(super) fn decode(bytes: Vec<u8>, expected: usize) -> Result<String, Error> {
    if expected > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    if bytes.len() != expected {
        return Err(Error::ReadFailed);
    }
    let text = String::from_utf8(bytes).map_err(|_| Error::InvalidText)?;
    if text.contains('\0') {
        return Err(Error::InvalidText);
    }
    if text.chars().count() > MAX_SCALARS {
        return Err(Error::TooLarge);
    }
    Ok(text)
}

pub(super) fn compare(
    original: &Metadata,
    original_text: &str,
    live: &Metadata,
    live_text: &str,
) -> Result<(), Error> {
    live.range()?;
    if live != original || live_text != original_text {
        return Err(Error::Changed);
    }
    Ok(())
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\u{a0}', " ")
}

/// `observed` is exactly [captured start, fresh caret), never a suffix sample.
pub(super) fn verify(
    original: &Metadata,
    live: &Metadata,
    observed: &str,
    replacement: &str,
) -> bool {
    original.range().is_ok()
        && live.range().is_ok()
        && original.document == live.document
        && live.anchor == live.caret
        && live.caret > original.start
        && usize::try_from(live.caret - original.start).ok() == Some(observed.len())
        && !replacement.is_empty()
        && normalize(observed) == normalize(replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_and_scalar_transport_are_not_inferred_from_values() {
        assert!(tested_version(8 << 16 | 9, 8 << 16 | 1));
        assert!(!tested_version(8 << 16 | 9, 8 << 16));
        assert!(!tested_version(9 << 16, 8 << 16 | 1));
        assert_eq!(scalar_result(true, true, 0), Ok(0));
        assert_eq!(scalar_result(false, true, 0), Err(Error::Timeout));
        assert_eq!(scalar_result(true, false, 12), Err(Error::Timeout));
    }
    fn selected(text: &str) -> Metadata {
        Metadata {
            document: 42,
            length: text.len() as isize,
            codepage: 65001,
            read_only: false,
            selections: 1,
            mode: 0,
            main: 0,
            anchor: 0,
            caret: text.len() as isize,
            anchor_virtual: 0,
            caret_virtual: 0,
            start: 0,
            end: text.len() as isize,
        }
    }
    #[test]
    fn preserves_unicode_multiline_and_selected_whitespace() {
        for text in ["Zażółć gęślą jaźń 😀\r\nDrugi wiersz", " \t\r\n ", ""] {
            let metadata = selected(text);
            let (_, count) = metadata.range().unwrap();
            assert_eq!(decode(text.as_bytes().to_vec(), count).unwrap(), text);
        }
    }
    #[test]
    fn refuses_nonstream_multicaret_readonly_virtual_and_invalid_ranges() {
        let m = selected("abc");
        for change in [0, 1, 2, 3, 4, 5, 6, 7, 8] {
            let mut bad = m.clone();
            match change {
                0 => bad.mode = 1,
                1 => bad.selections = 2,
                2 => bad.read_only = true,
                3 => bad.caret_virtual = 1,
                4 => bad.anchor_virtual = 1,
                5 => bad.codepage = 0,
                6 => bad.anchor = -1,
                7 => bad.end = 4,
                _ => bad.caret = isize::MAX,
            }
            assert!(bad.range().is_err(), "case {change}");
        }
    }
    #[test]
    fn strict_decoding_limits_and_partial_read() {
        assert_eq!(decode(vec![255], 1), Err(Error::InvalidText));
        assert_eq!(decode(vec![0], 1), Err(Error::InvalidText));
        assert_eq!(decode(vec![97], 2), Err(Error::ReadFailed));
        assert_eq!(decode(vec![], MAX_BYTES + 1), Err(Error::TooLarge));
        assert!(decode("😀".repeat(MAX_SCALARS).into_bytes(), MAX_BYTES).is_ok());
        assert_eq!(
            decode(vec![97; MAX_SCALARS + 1], MAX_SCALARS + 1),
            Err(Error::TooLarge)
        );
    }
    #[test]
    fn metadata_and_text_changes_revoke() {
        let m = selected("abc");
        assert_eq!(compare(&m, "abc", &m, "abc"), Ok(()));
        for change in [0, 1, 2, 3] {
            let mut live = m.clone();
            match change {
                0 => live.document += 1,
                1 => live.length += 1,
                2 => {
                    live.anchor = 1;
                    live.start = 1
                }
                _ => {
                    live.caret = 2;
                    live.end = 2
                }
            }
            assert!(compare(&m, "abc", &live, "abc").is_err());
        }
        assert_eq!(compare(&m, "abc", &m, "abd"), Err(Error::Changed));
    }
    #[test]
    fn verifies_exact_replacement_span_and_forward_caret() {
        let original = selected("old");
        let mut live = selected("Zażółć\r\n");
        live.anchor = live.caret;
        live.start = live.caret;
        assert!(verify(&original, &live, "Zażółć\r\n", "Zażółć\n"));
        assert!(!verify(&original, &live, "Zażółć\r\n", "żółć\n"));
        assert!(!verify(&original, &live, "wrong", "Zażółć\n"));
        live.document += 1;
        assert!(!verify(&original, &live, "Zażółć\r\n", "Zażółć\n"));
        let empty = selected("");
        assert!(!verify(&empty, &empty, "", "text"));
    }
}
