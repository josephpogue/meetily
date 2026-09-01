// assistant/card.rs
//
// The wire format every answer lane writes, and the only thing the panel
// renders:
//
//   LEAD: one sentence
//   - bullet
//   - bullet
//   SOURCE: where it came from
//
// or the single token SKIP, meaning "nothing worth showing". Parsing is
// incremental: the same function runs on every partial text, so a
// half-written card still shows its lead and the bullets finished so far.
//
// Port of v1's CardFormat.swift.

use std::collections::HashSet;

const MAX_BULLETS: usize = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedCard {
    pub lead: String,
    pub bullets: Vec<String>,
    /// Empty string means no source, matching Swift's `nil`.
    pub source: String,
    pub is_skip: bool,
    pub is_empty: bool,
}

/// Whether a partial stream has said enough to rule SKIP in or out. Without
/// this the panel flashes a one-character card on the first token of every
/// skipped trigger, because "S" parses as a perfectly good lead. SKIP is
/// supposed to render nothing at all, so nothing is presented until the text
/// can no longer become SKIP.
pub fn can_decide(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.chars().count() >= 4 {
        return true;
    }
    !"SKIP".starts_with(&t.to_uppercase())
}

pub fn parse(text: &str) -> ParsedCard {
    let trimmed_whole = text.trim();
    if trimmed_whole == "SKIP" || trimmed_whole.starts_with("SKIP\n") || trimmed_whole == "SKIP." {
        return ParsedCard {
            is_skip: true,
            ..Default::default()
        };
    }

    let mut card = ParsedCard::default();
    for raw_line in text.split('\n') {
        let line = trim_horizontal(raw_line);
        if line.is_empty() {
            continue;
        }

        if let Some(body) = strip_prefix_ci(line, "LEAD:") {
            card.lead = body.to_string();
        } else if let Some(body) = strip_prefix_ci(line, "SOURCE:") {
            card.source = body.to_string();
        } else if line.starts_with("- ") || line == "-" {
            let body = trim_horizontal(&line[1..]).to_string();
            if card.bullets.len() < MAX_BULLETS {
                card.bullets.push(body);
            }
            // Over the cap: drop it.
        } else if card.lead.is_empty() && card.bullets.is_empty() && card.source.is_empty() {
            // A lane that forgot the LEAD prefix still gets its first line
            // shown rather than a blank card.
            card.lead = line.to_string();
        } else if let Some(last) = card.bullets.last_mut() {
            // Continuation of the last bullet across a wrapped line.
            last.push(' ');
            last.push_str(line);
        }
    }

    card.is_empty = card.lead.is_empty() && card.bullets.is_empty() && card.source.is_empty();
    card
}

fn trim_horizontal(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\t')
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let mut chars = line.chars();
    for pc in prefix.chars() {
        match chars.next() {
            Some(lc) if lc.to_ascii_uppercase() == pc.to_ascii_uppercase() => continue,
            _ => return None,
        }
    }
    Some(trim_horizontal(chars.as_str()))
}

/// Which bullets of `new` differ from `old` at the same position, or are new
/// past `old`'s length. Drives the "corrected" highlight when the deep pass
/// contradicts the fast pass.
pub fn changed_bullets(old: &ParsedCard, new: &ParsedCard) -> Vec<String> {
    let mut changed = Vec::new();
    for (i, bullet) in new.bullets.iter().enumerate() {
        let unchanged = old
            .bullets
            .get(i)
            .map(|o| similar(o, bullet))
            .unwrap_or(false);
        if !unchanged {
            changed.push(bullet.clone());
        }
    }
    changed
}

/// True when the deep pass reached a different answer than the fast pass.
/// Only the lead decides this: the lead IS the answer, the bullets are
/// support, and two different models almost never phrase support
/// identically. Reserved for the answer itself changing, the one loud moment
/// in the design.
pub fn contradicts(fast: &ParsedCard, deep: &ParsedCard) -> bool {
    if fast.is_empty || fast.lead.is_empty() || deep.lead.is_empty() {
        return false;
    }
    !similar(&fast.lead, &deep.lead)
}

/// Cheap agreement test: is most of the shorter text's substance also in the
/// longer one. Containment rather than overlap, because the deep pass
/// usually says more, and scoring against the longer text would call every
/// fuller answer a contradiction.
fn similar(a: &str, b: &str) -> bool {
    // Numbers decide first. "Which of the four proposals" is answered by a
    // number, so "Proposal 3" against "Proposal 4" is a flat contradiction
    // however similar the surrounding words are. A number set that contains
    // the other is still agreement: naming 3 and 4 where the other named
    // 1, 2, 3 and 4 is saying less, not saying otherwise.
    let na = number_tokens(a);
    let nb = number_tokens(b);
    if !na.is_empty() && !nb.is_empty() && !na.is_subset(&nb) && !nb.is_subset(&na) {
        return false;
    }

    let ka = content_words(a);
    let kb = content_words(b);
    if ka == kb {
        return true;
    }
    if ka.is_empty() || kb.is_empty() {
        return ka.is_empty() && kb.is_empty();
    }
    let overlap = ka.intersection(&kb).count();
    (overlap as f64) / (ka.len().min(kb.len()) as f64) >= 0.5
}

/// Bare one and two digit numbers only, which is what an enumerated option
/// looks like: "Proposal 3". Anything with letters welded on is a
/// measurement rather than an answer and must not veto: "200ms p95" against
/// "Proposal 4" shares no numbers while the two plainly agree. Longer digit
/// runs are years and prices, not answers to "which one" either.
fn number_tokens(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.chars().count() <= 2 && t.chars().all(|c| c.is_numeric()))
        .map(|t| t.to_string())
        .collect()
}

/// Shared with trigger.rs's repeat guard.
pub fn content_words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !STOPWORDS.contains(w))
        .map(stem)
        .collect()
}

/// Crude plural and third-person stripping, load-bearing rather than
/// cosmetic. Without it "Proposal 3 holds" and "Proposals 3 and 4 hold"
/// share no words at all. Words ending in a double s are left alone.
fn stem(word: &str) -> String {
    let char_count = word.chars().count();
    if char_count >= 5 && word.ends_with('s') && !word.ends_with("ss") {
        let mut chars: Vec<char> = word.chars().collect();
        chars.pop();
        chars.into_iter().collect()
    } else {
        word.to_string()
    }
}

const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "your", "yours", "they", "them", "then", "than", "have",
    "does", "will", "would", "could", "should", "which", "what", "when", "where", "into", "onto",
    "over", "under", "about", "actually", "really", "just", "only", "also", "both", "some",
    "more", "most", "much", "very", "here", "there", "these", "those", "still", "been", "being",
    "were", "weren", "wasn", "isn", "aren", "didn", "doesn",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_card() {
        let text = "LEAD: Ship on Tuesday\n- Backend is done\n- QA needs one more day\nSOURCE: standup notes";
        let card = parse(text);
        assert_eq!(card.lead, "Ship on Tuesday");
        assert_eq!(card.bullets, vec!["Backend is done", "QA needs one more day"]);
        assert_eq!(card.source, "standup notes");
        assert!(!card.is_skip);
        assert!(!card.is_empty);
    }

    #[test]
    fn tolerates_missing_source() {
        let text = "LEAD: Ship on Tuesday\n- Backend is done";
        let card = parse(text);
        assert_eq!(card.lead, "Ship on Tuesday");
        assert_eq!(card.source, "");
        assert!(!card.is_skip);
        assert!(!card.is_empty);
    }

    #[test]
    fn skip_sentinel_is_recognized() {
        assert!(parse("SKIP").is_skip);
        assert!(parse("  SKIP  ").is_skip);
        assert!(parse("SKIP.").is_skip);
        assert!(parse("SKIP\nnothing worth showing").is_skip);
        assert!(!parse("SKIPPER: not a match").is_skip);
    }

    #[test]
    fn can_decide_holds_short_prefixes_of_skip() {
        assert!(!can_decide("S"));
        assert!(!can_decide("SKI"));
        assert!(can_decide("SKIP"));
        assert!(parse("SKIP").is_skip);
        assert!(can_decide("LEAD: x"));
    }

    #[test]
    fn contradicts_on_changed_lead_not_reworded_bullet() {
        let fast = parse("LEAD: We ship Proposal 3\n- Backend is done");
        let deep_reworded_bullet =
            parse("LEAD: We ship Proposal 3\n- The backend implementation is complete");
        assert!(!contradicts(&fast, &deep_reworded_bullet));

        let deep_changed_lead = parse("LEAD: We ship Proposal 4 instead\n- Backend is done");
        assert!(contradicts(&fast, &deep_changed_lead));
    }

    #[test]
    fn changed_bullets_reports_new_or_differing_text() {
        let old = parse("LEAD: x\n- Backend is done\n- QA needs one more day");
        let new = parse(
            "LEAD: x\n- Frontend still blocked on API keys\n- QA needs one more day\n- Docs updated",
        );
        let changed = changed_bullets(&old, &new);
        assert_eq!(
            changed,
            vec![
                "Frontend still blocked on API keys".to_string(),
                "Docs updated".to_string(),
            ]
        );
    }
}
