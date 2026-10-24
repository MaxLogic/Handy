//! Remove formatting cues an LLM echoed at the start of a dictated output line.
//! Only cues also present in the source dictation are eligible.
const CUES: &[(&str, usize)] = &[
    ("new paragraph", 2),
    ("nowy akapit", 2),
    ("new line", 1),
    ("line break", 1),
    ("nowa linia", 1),
];

fn command_prefix(text: &str) -> Option<(usize, &str)> {
    let text = text.trim_start_matches([' ', '\t']);
    for &(cue, breaks) in CUES {
        let Some(prefix) = text.get(..cue.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(cue) {
            continue;
        }
        let rest = &text[cue.len()..];
        if !rest.starts_with([',', '.', ':', ';', '!', '?']) {
            continue;
        }
        let rest = rest
            .trim_start_matches([',', '.', ':', ';', '!', '?'])
            .trim_start_matches([' ', '\t']);
        if !rest.trim().is_empty() {
            return Some((breaks, rest));
        }
    }
    None
}

fn source_commands(transcript: &str) -> Vec<usize> {
    let source = transcript.to_lowercase();
    let mut commands = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        let after_letter = source[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let between_letters = matches!(ch, '\'' | '\u{2019}')
            && after_letter
            && source[index + ch.len_utf8()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        // A trailing possessive apostrophe is ambiguous with a closing quote.
        // Prefer preserving quoted text over consuming a possible command.
        let possessive = matches!(ch, '\'' | '\u{2019}')
            && source[..index].ends_with('s')
            && source[index + ch.len_utf8()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        if let Some(closing) = quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == closing && !between_letters && !possessive {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '`' => quote = Some(ch),
            '\'' if !after_letter => quote = Some(ch),
            '\u{201c}' => quote = Some('\u{201d}'),
            '\u{2018}' => quote = Some('\u{2019}'),
            _ => {}
        }
        if quote.is_some() || ch.is_whitespace() {
            continue;
        }
        let before = source[..index].trim_end_matches([' ', '\t', '\r']);
        let boundary = before.is_empty()
            || before
                .trim_end_matches(['"', '\'', '`', '\u{201d}', '\u{2019}'])
                .ends_with(['.', ',', ':', ';', '!', '?', '\n']);
        if boundary {
            if let Some((breaks, _)) = command_prefix(&source[index..]) {
                commands.push(breaks);
            }
        }
    }
    commands
}

/// Repair an echoed, delimited cue only when source dictation contains a
/// corresponding standalone command. Each source command can repair one echo.
pub(crate) fn consume_echoed_cues(transcript: &str, output: &str) -> String {
    let commands = source_commands(transcript);
    if commands.is_empty() {
        return output.to_owned();
    }
    let mut next_command = 0;
    let mut result = String::with_capacity(output.len());
    for line in output.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let Some((breaks, rest)) = command_prefix(body) else {
            result.push_str(line);
            continue;
        };
        let Some(offset) = commands[next_command..]
            .iter()
            .position(|&source_breaks| source_breaks == breaks)
        else {
            result.push_str(line);
            continue;
        };
        // Earlier commands may already have been consumed by the model.
        next_command += offset + 1;
        result.truncate(result.trim_end_matches([' ', '\t', '\r', '\n']).len());
        if !result.is_empty() {
            for _ in 0..breaks {
                result.push('\n');
            }
        }
        result.push_str(rest);
        result.push_str(&line[body.len()..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::consume_echoed_cues;

    #[test]
    fn preserves_multiline_quoted_cues_and_resumes_after_the_quote() {
        let curly = "Say \u{2018}Don\u{2019}t change it.\nNew paragraph, as a term, means a boundary.\u{2019}";
        assert_eq!(consume_echoed_cues(curly, curly), curly);
        for literal in [
            "Say \u{2018}The users\u{2019} choice.\nNew paragraph, as a term, means a boundary.\u{2019}",
            "Say 'The users' choice.\nNew paragraph, as a term, means a boundary.'",
        ] {
            assert_eq!(consume_echoed_cues(literal, literal), literal);
        }
        for quote in ['"', '\'', '`'] {
            let literal = format!(
                "Say {quote}First sentence.\nNew paragraph, as a term, means a boundary.{quote}"
            );
            assert_eq!(consume_echoed_cues(&literal, &literal), literal);
        }
        assert_eq!(
            consume_echoed_cues(
                "Say \"don't change it.\nNew paragraph, as a term.\" New line, continue.",
                "Quoted phrase.\nNew line, continue."
            ),
            "Quoted phrase.\ncontinue."
        );
        assert_eq!(
            consume_echoed_cues(
                "It's the users' choice. New paragraph, continue.",
                "Choice.\nNew paragraph, continue."
            ),
            "Choice.\n\ncontinue."
        );
    }

    #[test]
    fn consumes_each_source_command_at_most_once_and_preserves_quoted_mentions() {
        assert_eq!(
            consume_echoed_cues(
                "Intro.  New paragraph, next.",
                "Intro.\nNew paragraph, next.\nNew paragraph, literal."
            ),
            "Intro.\n\nnext.\nNew paragraph, literal."
        );
        let quoted = "New paragraph, as a term, means a paragraph boundary.";
        assert_eq!(
            consume_echoed_cues("Say \"New paragraph, as a term\".", quoted),
            quoted
        );
    }

    #[test]
    fn consumes_echoed_commands_in_the_reported_translation() {
        let source = "Ok, sprawdźmy jak to działa. New line, to powinno być od nowej linii. New paragraph, to powinno być teraz na następnym akapicie. No i chyba tyle.";
        let echoed = "Ok, let's check how it works.\nNew line, that should be on a new line.\nNew paragraph, that should now be on the next paragraph.\nAnd that's about it.";
        assert_eq!(
            consume_echoed_cues(source, echoed),
            "Ok, let's check how it works.\nthat should be on a new line.\n\nthat should now be on the next paragraph.\nAnd that's about it."
        );
    }

    #[test]
    fn consumes_polish_and_english_cues_without_doubling_existing_breaks() {
        for (source, output, expected) in [
            (
                "Pierwsze. Line break, drugie.",
                "First.\nLine break, second.",
                "First.\nsecond.",
            ),
            (
                "Pierwsze. Nowa linia, drugie.",
                "Pierwsze.\nNowa linia, drugie.",
                "Pierwsze.\ndrugie.",
            ),
            (
                "Pierwsze. Nowy akapit, drugie.",
                "First.\n\nNew paragraph, second.",
                "First.\n\nsecond.",
            ),
        ] {
            assert_eq!(consume_echoed_cues(source, output), expected);
        }
    }

    #[test]
    fn preserves_text_without_a_matching_source_cue_or_without_a_command_delimiter() {
        for (source, output) in [
            ("Discuss typography.", "New paragraph, by the author."),
            (
                "New paragraph formatting",
                "New paragraph formatting is useful.",
            ),
            ("Say new line.", "The words new line, in a sentence."),
            ("New paragraph.", "Already formatted.\n\nSecond paragraph."),
        ] {
            assert_eq!(consume_echoed_cues(source, output), output);
        }
    }
}
