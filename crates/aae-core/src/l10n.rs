//! Translations of what AAE shows and says.
//!
//! English stays the source everywhere: the code writes its text in English,
//! and [`tr`] turns finished English text into the user's language on its way
//! out, in the apps and in `aae`. A catalog, such as `translations/fr.po`,
//! pairs each English text with its translation. Text with parts filled in,
//! such as "{device} is ready.", is found as a pattern, and its parts are put
//! in their place in the translation, translated too when they're text AAE
//! knows. Text with no translation stays in English.
//!
//! The MCP server stays in English: AI agents read it.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// The languages with a catalog, by their two-letter code.
static CATALOGS: &[(&str, &str)] = &[(
    "fr",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../translations/fr.po")),
)];

static PARSED: LazyLock<HashMap<&'static str, Catalog>> = LazyLock::new(|| {
    CATALOGS
        .iter()
        .map(|(code, po)| (*code, Catalog::parse(po)))
        .collect()
});

static CURRENT: RwLock<Option<&'static str>> = RwLock::new(None);

/// Uses the language of a tag such as "fr", "fr-FR" or "fr_FR.UTF-8", or
/// English when AAE has no catalog for it. Returns whether it has one.
pub fn set_language(tag: &str) -> bool {
    let code = tag
        .split(['-', '_', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let found = CATALOGS.iter().find(|(c, _)| *c == code).map(|(c, _)| *c);
    if let Ok(mut current) = CURRENT.write() {
        *current = found;
    }
    found.is_some()
}

/// The language `aae` uses: AAE_LANG, then the usual LC_ALL, LC_MESSAGES
/// and LANG.
pub fn language_from_env() -> Option<String> {
    ["AAE_LANG", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty() && value != "C" && value != "POSIX")
}

/// The two-letter code of the language in use, if not English.
pub fn language() -> Option<&'static str> {
    CURRENT.read().ok().and_then(|c| *c)
}

/// The English text in the language in use.
pub fn tr(text: &str) -> String {
    match language().and_then(|code| PARSED.get(code)) {
        Some(catalog) => catalog.translate(text),
        None => text.to_string(),
    }
}

/// As `tr`, for a word with more than one meaning in English, such as
/// "Watch" (a button) and "Watch" (a device): the catalog's entry with this
/// context (msgctxt), or else the text's usual translation.
pub fn tr_in(context: &str, text: &str) -> String {
    let found = language()
        .and_then(|code| PARSED.get(code))
        .and_then(|catalog| catalog.exact.get(&format!("{context}\u{4}{text}")));
    match found {
        Some(translated) => translated.clone(),
        None => tr(text),
    }
}

/// One English text and its translation, as parts.
struct Template {
    english: Vec<Part>,
    translated: Vec<Part>,
    /// The length of its fixed text, to try the most specific first.
    fixed: usize,
    /// Its fixed text ends a sentence or introduces one, as in "Couldn't
    /// start {device}: {why}", so a part may hold whole sentences. A pattern
    /// such as "{} and {}" joins names, not sentences.
    holds_sentences: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    /// A part filled in, by name ("device") or position ("0", "1"…).
    Slot(String),
}

#[derive(Default)]
struct Catalog {
    exact: HashMap<String, String>,
    templates: Vec<Template>,
}

impl Catalog {
    fn parse(po: &str) -> Catalog {
        let mut catalog = Catalog::default();
        for (english, translated) in po_entries(po) {
            // Text is looked up without the spaces around it, which are
            // kept as they are (see translate_whole).
            let (english, translated) = (english.trim().to_string(), translated.trim().to_string());
            // A translation that is the English text adds nothing, and as a
            // pattern it could hide a better one.
            if english.is_empty() || translated.is_empty() || translated == english {
                continue;
            }
            let english_parts = parts(&english);
            if !english_parts.iter().any(|p| matches!(p, Part::Slot(_))) {
                catalog.exact.insert(english, translated);
                continue;
            }
            // Two parts filled in side by side can't be told apart, and a
            // pattern with no words of its own, such as "{kind}: {title}",
            // would match almost any text.
            if english_parts
                .windows(2)
                .any(|w| matches!(w, [Part::Slot(_), Part::Slot(_)]))
                || !english_parts.iter().any(|p| match p {
                    Part::Text(t) => t.chars().any(char::is_alphabetic),
                    Part::Slot(_) => false,
                })
            {
                continue;
            }
            let fixed = english_parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => t.len(),
                    Part::Slot(_) => 0,
                })
                .sum();
            let holds_sentences = english_parts.iter().any(|p| match p {
                Part::Text(t) => t.contains(['.', ':', '?', '!']),
                Part::Slot(_) => false,
            });
            catalog.templates.push(Template {
                english: english_parts,
                translated: parts(&translated),
                fixed,
                holds_sentences,
            });
        }
        catalog.templates.sort_by_key(|t| std::cmp::Reverse(t.fixed));
        catalog
    }

    fn translate(&self, text: &str) -> String {
        match self.translate_any(text, 0) {
            Some(found) => found,
            None => {
                note_missing(text);
                text.to_string()
            }
        }
    }

    /// The text as a whole, or else sentence by sentence and line by line,
    /// as when messages are joined. None if AAE knows none of it.
    fn translate_any(&self, text: &str, depth: u8) -> Option<String> {
        if let Some(found) = self.translate_whole(text, depth) {
            return Some(found);
        }
        let pieces = split_sentences(text);
        let count = pieces.len();
        if count < 2 {
            return None;
        }
        // From the left, the longest run of sentences AAE knows as one
        // text, as a message of two sentences after a label.
        let mut out = String::new();
        let mut any = false;
        let mut start = 0;
        while start < count {
            let mut found = None;
            for end in (start + 1..=count).rev() {
                if start == 0 && end == count {
                    continue; // The whole text, tried above.
                }
                let run: String = pieces[start..end]
                    .iter()
                    .enumerate()
                    .map(|(i, (piece, gap))| {
                        if start + i + 1 == end { piece.to_string() } else { format!("{piece}{gap}") }
                    })
                    .collect();
                if !run.chars().any(char::is_alphabetic) {
                    continue;
                }
                if let Some(translated) = self.translate_whole(&run, depth) {
                    found = Some((translated, end));
                    break;
                }
            }
            match found {
                Some((translated, end)) => {
                    any = true;
                    out.push_str(&translated);
                    out.push_str(pieces[end - 1].1);
                    start = end;
                }
                None => {
                    let (piece, gap) = pieces[start];
                    if depth == 0 {
                        note_missing(piece);
                    }
                    out.push_str(piece);
                    out.push_str(gap);
                    start += 1;
                }
            }
        }
        any.then_some(out)
    }

    /// The whole text translated, keeping the spaces around it.
    fn translate_whole(&self, text: &str, depth: u8) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() || !trimmed.chars().any(char::is_alphabetic) {
            return Some(text.to_string());
        }
        let start = text.len() - text.trim_start().len();
        let end = start + trimmed.len();
        let translated = self
            .translate_trimmed(trimmed, depth)
            .or_else(|| self.translate_list(trimmed, depth))?;
        Some(format!("{}{}{}", &text[..start], translated, &text[end..]))
    }

    /// A list such as "16 KB pages, With Google services, Phone": each item
    /// AAE knows translated, the others kept. None if it knows none.
    fn translate_list(&self, text: &str, depth: u8) -> Option<String> {
        let (body, end) = match text.strip_suffix('.') {
            Some(body) => (body, "."),
            None => (text, ""),
        };
        // Several sentences are translated one by one first (see translate).
        if body.contains(". ") || body.contains('\n') {
            return None;
        }
        for separator in ["; ", ", "] {
            if !body.contains(separator) {
                continue;
            }
            let mut any = false;
            let items: Vec<String> = body
                .split(separator)
                .map(|item| match self.translate_trimmed(item.trim(), depth + 1) {
                    Some(t) => {
                        any = true;
                        t
                    }
                    None => item.to_string(),
                })
                .collect();
            if any {
                let separator = if separator == "; " { "\u{a0}; " } else { separator };
                return Some(items.join(separator) + end);
            }
        }
        None
    }

    fn translate_trimmed(&self, text: &str, depth: u8) -> Option<String> {
        // Exact text first, in each form, then patterns, so "Android
        // versions." is "Android versions" with a period, not "Android {}".
        self.forms(text, &|form| self.exact.get(form).cloned())
            .or_else(|| self.forms(text, &|form| self.pattern(form, depth)))
            .or_else(|| {
                // A value shown with a capital, such as "Full speed" for
                // "full speed".
                let mut chars = text.chars();
                let first = chars.next().filter(|c| c.is_uppercase())?;
                let lower = first.to_lowercase().collect::<String>() + chars.as_str();
                let found = self.forms(&lower, &|form| self.exact.get(form).cloned())?;
                let mut chars = found.chars();
                let first = chars.next()?;
                Some(first.to_uppercase().collect::<String>() + chars.as_str())
            })
            .or_else(|| {
                // A one-word label the same in French, such as "OK:", still
                // takes French spacing.
                let label = text.strip_suffix(':')?;
                (!label.is_empty() && label.chars().all(char::is_alphanumeric))
                    .then(|| format!("{label}\u{a0}:"))
            })
    }

    /// The text as it is; a label such as "Warning:" as its word with French
    /// spacing; and the text with or without a final period, as when a
    /// step's name is said as a sentence.
    fn forms(&self, text: &str, find: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        if let Some(found) = find(text) {
            return Some(found);
        }
        if let Some(label) = text.strip_suffix(':')
            && !label.ends_with([' ', '\u{a0}'])
            && let Some(found) = find(label)
        {
            return Some(format!("{found}\u{a0}:"));
        }
        match text.strip_suffix('.') {
            Some(bare) if !bare.ends_with('.') => {
                find(bare).map(|t| if t.ends_with(['.', '!', '?', '…']) { t } else { t + "." })
            }
            Some(_) => None,
            None => find(&format!("{text}."))
                .map(|t| t.strip_suffix('.').map(str::to_string).unwrap_or(t)),
        }
    }

    fn pattern(&self, text: &str, depth: u8) -> Option<String> {
        for template in &self.templates {
            let Some(values) = fill(&template.english, text) else {
                continue;
            };
            if !template.holds_sentences
                && values
                    .values()
                    .any(|v| [". ", "? ", "! ", "\n"].iter().any(|end| v.contains(end)))
            {
                continue;
            }
            let mut out = String::new();
            for part in &template.translated {
                match part {
                    Part::Text(t) => out.push_str(t),
                    Part::Slot(name) => {
                        let value = values.get(name).map(String::as_str).unwrap_or("");
                        // A part may be a message of its own, such as why
                        // something failed.
                        let value = if depth < 2 {
                            self.translate_any(value, depth + 1)
                                .unwrap_or_else(|| value.to_string())
                        } else {
                            value.to_string()
                        };
                        out.push_str(&value);
                    }
                }
            }
            return Some(out);
        }
        None
    }
}

/// The values of a template's parts in a text, if the text is that template.
fn fill(template: &[Part], text: &str) -> Option<HashMap<String, String>> {
    let mut values = HashMap::new();
    let mut rest = text;
    let mut parts = template.iter().peekable();
    // The last fixed text must end the text.
    let suffix = match template.last() {
        Some(Part::Text(t)) => t.as_str(),
        _ => "",
    };
    while let Some(part) = parts.next() {
        match part {
            Part::Text(t) => {
                if parts.peek().is_none() {
                    // Already checked as the suffix.
                    if rest != t.as_str() {
                        return None;
                    }
                    rest = "";
                } else {
                    rest = rest.strip_prefix(t.as_str())?;
                }
            }
            Part::Slot(name) => {
                let value_len = match parts.peek() {
                    None => rest.len(),
                    Some(Part::Text(next)) => {
                        if parts.clone().nth(1).is_none() {
                            // The next text ends the template.
                            rest.len().checked_sub(suffix.len())
                                .filter(|_| rest.ends_with(suffix))?
                        } else {
                            rest.find(next.as_str())?
                        }
                    }
                    Some(Part::Slot(_)) => return None,
                };
                // Only a part after some fixed text may be empty, as in
                // "It is running{}." with nothing to add.
                if value_len == 0 && rest.len() == text.len() {
                    return None;
                }
                // A part that starts the text is a name or a value, never
                // the end of an earlier sentence: "{} installed." isn't
                // "OK: Android versions. 1 installed."
                let value = &rest[..value_len];
                if rest.len() == text.len()
                    && [". ", "? ", "! ", ": ", "\n"].iter().any(|end| value.contains(end))
                {
                    return None;
                }
                values.insert(name.clone(), value.to_string());
                rest = &rest[value_len..];
            }
        }
    }
    rest.is_empty().then_some(values)
}

/// A text's fixed parts and filled-in parts. `{name}` and `{}` are Rust's,
/// `%@`, `%lld` and `%1$@` Swift's. Unnamed ones are numbered in order.
fn parts(text: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut fixed = String::new();
    let mut position = 0;
    let mut chars = text.char_indices().peekable();
    let push_fixed = |parts: &mut Vec<Part>, fixed: &mut String| {
        if !fixed.is_empty() {
            parts.push(Part::Text(std::mem::take(fixed)));
        }
    };
    while let Some((i, c)) = chars.next() {
        match c {
            '{' if text[i + 1..].starts_with('{') => {
                fixed.push('{');
                chars.next();
            }
            '}' if text[i + 1..].starts_with('}') => {
                fixed.push('}');
                chars.next();
            }
            '{' => {
                let Some(close) = text[i..].find('}') else {
                    fixed.push(c);
                    continue;
                };
                let inside = &text[i + 1..i + close];
                let name = inside.split(':').next().unwrap_or("");
                let ok = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !ok {
                    fixed.push(c);
                    continue;
                }
                push_fixed(&mut parts, &mut fixed);
                let key = if name.is_empty() {
                    position += 1;
                    (position - 1).to_string()
                } else {
                    name.to_string()
                };
                parts.push(Part::Slot(key));
                while let Some((j, _)) = chars.peek() {
                    if *j > i + close {
                        break;
                    }
                    chars.next();
                }
            }
            '%' => {
                let spec = swift_format(&text[i..]);
                match spec {
                    Some((len, index)) => {
                        push_fixed(&mut parts, &mut fixed);
                        let key = match index {
                            Some(n) => (n - 1).to_string(),
                            None => {
                                position += 1;
                                (position - 1).to_string()
                            }
                        };
                        parts.push(Part::Slot(key));
                        while let Some((j, _)) = chars.peek() {
                            if *j >= i + len {
                                break;
                            }
                            chars.next();
                        }
                    }
                    None => {
                        if text[i + 1..].starts_with('%') {
                            chars.next();
                        }
                        fixed.push('%');
                    }
                }
            }
            _ => fixed.push(c),
        }
    }
    push_fixed(&mut parts, &mut fixed);
    parts
}

/// A Swift format such as `%@`, `%lld` or `%1$@` at the start of a text: its
/// length and its explicit position, if any.
fn swift_format(text: &str) -> Option<(usize, Option<usize>)> {
    let rest = &text[1..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (index, after) = match rest[digits.len()..].strip_prefix('$') {
        Some(after) if !digits.is_empty() => (digits.parse().ok(), after),
        _ => (None, rest),
    };
    for spec in ["@", "lld", "llu", "ld", "lu", "d", "u", "f", ".1f", ".2f", ".0f"] {
        if after.starts_with(spec) {
            let len = 1 + (rest.len() - after.len()) + spec.len();
            return Some((len, index));
        }
    }
    None
}

/// Sentences and lines, each with the space after it.
fn split_sentences(text: &str) -> Vec<(&str, &str)> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let end_of_sentence = matches!(bytes[i], b'.' | b'?' | b'!' | b':')
            && bytes.get(i + 1).is_some_and(|b| *b == b' ' || *b == b'\n');
        if end_of_sentence || bytes[i] == b'\n' {
            let piece_end = if bytes[i] == b'\n' { i } else { i + 1 };
            let mut gap_end = piece_end;
            while gap_end < bytes.len() && (bytes[gap_end] == b' ' || bytes[gap_end] == b'\n') {
                gap_end += 1;
            }
            pieces.push((&text[start..piece_end], &text[piece_end..gap_end]));
            start = gap_end;
            i = gap_end;
            continue;
        }
        i += 1;
    }
    if start < text.len() {
        pieces.push((&text[start..], ""));
    }
    pieces
}

/// Notes English text with no translation in the file AAE_L10N_MISSING
/// names, for translators.
fn note_missing(text: &str) {
    static PATH: LazyLock<Option<String>> =
        LazyLock::new(|| std::env::var("AAE_L10N_MISSING").ok().filter(|p| !p.is_empty()));
    let Some(path) = PATH.as_ref() else { return };
    if !text.chars().any(char::is_alphabetic) {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", text.replace('\n', "\\n"));
    }
}

/// The msgid and msgstr of each entry of a gettext PO file. An entry with a
/// msgctxt has "context\u{4}msgid" as its msgid, as gettext does.
fn po_entries(po: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut context: Option<String> = None;
    let mut id = String::new();
    let mut translated = String::new();
    // 0: msgctxt, 1: msgid, 2: msgstr.
    let mut field: Option<u8> = None;
    let mut flush = |context: &mut Option<String>, id: &mut String, translated: &mut String| {
        if !id.is_empty() || !translated.is_empty() {
            let id = std::mem::take(id);
            let id = match context.take() {
                Some(c) => format!("{c}\u{4}{id}"),
                None => id,
            };
            entries.push((id, std::mem::take(translated)));
        }
        *context = None;
    };
    for line in po.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            if line.is_empty() {
                flush(&mut context, &mut id, &mut translated);
                field = None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgctxt ") {
            flush(&mut context, &mut id, &mut translated);
            context = Some(unquote(rest));
            field = Some(0);
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            if field != Some(0) {
                flush(&mut context, &mut id, &mut translated);
            }
            id = unquote(rest);
            field = Some(1);
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            translated = unquote(rest);
            field = Some(2);
        } else if line.starts_with('"') {
            match field {
                Some(0) => {
                    if let Some(c) = context.as_mut() {
                        c.push_str(&unquote(line));
                    }
                }
                Some(1) => id.push_str(&unquote(line)),
                Some(2) => translated.push_str(&unquote(line)),
                _ => {}
            }
        }
    }
    flush(&mut context, &mut id, &mut translated);
    entries
}

fn unquote(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(po: &str) -> Catalog {
        Catalog::parse(po)
    }

    const PO: &str = r#"
msgid ""
msgstr "Content-Type: text/plain; charset=UTF-8\n"

msgid "Cold Boot"
msgstr "Démarrage à froid"

msgid "{device} is ready. {reader} is on."
msgstr "L'appareil {device} est prêt. {reader} est activé."

msgid "Couldn't start {device}: {why}"
msgstr "Impossible de démarrer l'appareil {device} : {why}"

msgid "The emulator isn't running."
msgstr "L'émulateur ne tourne pas."

msgid "%@ of %@"
msgstr "%2$@ sur %1$@"

msgid "Phone"
msgstr "Téléphone"

msgid "16 KB pages"
msgstr "pages de 16 Ko"

msgid " Using NVDA."
msgstr " NVDA est utilisé."

msgid "Installing AAE's helper"
msgstr "Installation de l'app auxiliaire d'AAE"

msgid "Warning"
msgstr "Avertissement"

msgid "full speed"
msgstr "pleine vitesse"

msgid "{} processor threads{memory}. Devices get {} cores."
msgstr "{} processeurs logiques{memory}. Les appareils utilisent {} cœurs."

msgid "It is running{}."
msgstr "L'appareil est en marche{}."

msgid "{} installed."
msgstr "Installées : {}."

msgid "Problem: {}"
msgstr "Problème : {}"

msgid "Not found."
msgstr "Introuvable."

msgid "Android versions"
msgstr "Versions d'Android"

msgid "Android has started."
msgstr "Android a démarré."

msgid "Watch"
msgstr "Montre"

msgctxt "start watching for builds"
msgid "Watch"
msgstr "Surveiller"

msgid "Line one\n"
"line two"
msgstr "Ligne un\n"
"ligne deux"
"#;

    #[test]
    fn exact_text() {
        let c = catalog(PO);
        assert_eq!(c.translate("Cold Boot"), "Démarrage à froid");
        assert_eq!(c.translate("Line one\nline two"), "Ligne un\nligne deux");
        assert_eq!(c.translate("Not in the catalog"), "Not in the catalog");
    }

    #[test]
    fn named_parts_go_in_their_place() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("Pixel 9 is ready. TalkBack is on."),
            "L'appareil Pixel 9 est prêt. TalkBack est activé."
        );
    }

    #[test]
    fn parts_that_are_messages_are_translated_too() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("Couldn't start Test: The emulator isn't running."),
            "Impossible de démarrer l'appareil Test : L'émulateur ne tourne pas."
        );
    }

    #[test]
    fn swift_positions_can_change_order() {
        let c = catalog(PO);
        assert_eq!(c.translate("3 of 5"), "5 sur 3");
    }

    #[test]
    fn joined_messages_are_translated_one_by_one() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("Android has started. Cold Boot"),
            "Android a démarré. Démarrage à froid"
        );
    }

    #[test]
    fn lists_are_translated_item_by_item() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("16 KB pages, Phone. Android has started."),
            "pages de 16 Ko, Téléphone. Android a démarré."
        );
        assert_eq!(
            c.translate("Android 17 (API 37.2), 16 KB pages, Phone"),
            "Android 17 (API 37.2), pages de 16 Ko, Téléphone"
        );
    }

    #[test]
    fn spaces_and_final_periods_dont_hide_a_text() {
        let c = catalog(PO);
        assert_eq!(c.translate("Using NVDA."), "NVDA est utilisé.");
        assert_eq!(
            c.translate("Installing AAE's helper."),
            "Installation de l'app auxiliaire d'AAE."
        );
        assert_eq!(c.translate("Android has started"), "Android a démarré");
    }

    #[test]
    fn labels_and_runs_of_sentences() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("Warning: Android has started. 18 processor threads, 64 GB. Devices get 4 cores."),
            "Avertissement\u{a0}: Android a démarré. 18 processeurs logiques, 64 GB. Les appareils utilisent 4 cœurs."
        );
    }

    #[test]
    fn joining_names_never_takes_sentences() {
        let c = catalog(
            "msgid \"{} and {}\"\nmsgstr \"{} et {}\"\n\nmsgid \"Android has started.\"\nmsgstr \"Android a démarré.\"\n",
        );
        assert_eq!(c.translate("Test and Pixel"), "Test et Pixel");
        assert_eq!(
            c.translate("Up and away. Android has started."),
            "Up et away. Android a démarré."
        );
    }

    #[test]
    fn a_value_shown_with_a_capital() {
        let c = catalog(PO);
        assert_eq!(c.translate("Full speed"), "Pleine vitesse");
    }

    #[test]
    fn a_part_after_fixed_text_may_be_empty() {
        let c = catalog(PO);
        assert_eq!(c.translate("It is running."), "L'appareil est en marche.");
    }

    #[test]
    fn a_leading_part_never_takes_an_earlier_sentence() {
        let c = catalog(PO);
        assert_eq!(
            c.translate("Android versions. 1 installed."),
            "Versions d'Android. Installées : 1."
        );
        assert_eq!(
            c.translate("Problem: Android has started. Not found."),
            "Problème : Android a démarré. Introuvable."
        );
    }

    #[test]
    fn spaces_around_are_kept() {
        let c = catalog(PO);
        assert_eq!(c.translate("  Cold Boot\n"), "  Démarrage à froid\n");
    }

    #[test]
    fn context_tells_meanings_apart() {
        let c = catalog(PO);
        assert_eq!(c.translate("Watch"), "Montre");
        assert_eq!(
            c.exact.get("start watching for builds\u{4}Watch").map(String::as_str),
            Some("Surveiller")
        );
    }

    #[test]
    fn language_tags() {
        assert!(set_language("fr_FR.UTF-8"));
        assert_eq!(language(), Some("fr"));
        assert!(!set_language("de-DE"));
        assert_eq!(language(), None);
        assert_eq!(tr("Cold Boot"), "Cold Boot");
    }

    /// For translators: each line of AAE_L10N_TRY in French, with
    /// `cargo test -p aae-core l10n::tests::try_text -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn try_text() {
        let fr = Catalog::parse(CATALOGS[0].1);
        for line in std::env::var("AAE_L10N_TRY").unwrap_or_default().lines() {
            println!("{line}\n  → {}", fr.translate(line));
        }
    }

    /// Every translation keeps the English text's filled-in parts, no more
    /// and no fewer.
    #[test]
    fn catalogs_keep_every_part() {
        let slots = |text: &str| {
            let mut names: Vec<String> = parts(text)
                .into_iter()
                .filter_map(|p| match p {
                    Part::Slot(name) => Some(name),
                    Part::Text(_) => None,
                })
                .collect();
            names.sort();
            names
        };
        for (code, po) in CATALOGS {
            for (english, translated) in po_entries(po) {
                if english.is_empty() || translated.is_empty() {
                    continue;
                }
                assert_eq!(
                    slots(&english),
                    slots(&translated),
                    "{code}: {english:?} and {translated:?} have different parts"
                );
            }
        }
    }
}
