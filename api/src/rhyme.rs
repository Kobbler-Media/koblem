//! Phonetic rhyme checking for drafted lyrics (CMUdict / ARPAbet).
//!
//! LLMs rhyme by spelling, not sound: "good"/"blood" share -ood but are UH1 D vs
//! AH1 D. It survives on the page and collapses the moment it is sung. This finds
//! those pairs so the caller can show them and offer a fix — it never rewrites
//! anything itself. Perfect rhyme is not always wanted (rap leans on slant and
//! internal rhyme), so the decision is the user's.
//!
//! Best-effort by design: words absent from CMUdict are skipped silently rather
//! than guessed at, and only the unambiguous trap is flagged — see [`eye_rhymes`].
//!
//! Self-contained: `resources/cmudict.dict` is vendored (CMU licence alongside it)
//! and parsed here. A phone is a stressed vowel iff it ends in `1` or `2`, which is
//! all the phonology this needs — no dictionary crate.

use std::collections::HashMap;
use std::sync::OnceLock;

const DICT: &str = include_str!("../resources/cmudict.dict");

type Pron = Vec<&'static str>;

/// `word PH1 PH2`, variants as `word(2) PH1`, optional trailing `# comment`.
/// Phones borrow from the embedded dict — parsing copies no strings.
fn dict() -> &'static HashMap<&'static str, Vec<Pron>> {
    static D: OnceLock<HashMap<&'static str, Vec<Pron>>> = OnceLock::new();
    D.get_or_init(|| {
        let mut map: HashMap<&'static str, Vec<Pron>> = HashMap::new();
        for line in DICT.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(head) = parts.next() else { continue };
            let word = head.split('(').next().unwrap_or(head);
            let phones: Pron = parts.collect();
            if !phones.is_empty() {
                map.entry(word).or_default().push(phones);
            }
        }
        map
    })
}

fn prons(word: &str) -> Option<&'static [Pron]> {
    dict().get(word).map(Vec::as_slice)
}

fn is_stressed_vowel(phone: &str) -> bool {
    matches!(phone.as_bytes().last(), Some(b'1' | b'2'))
}

/// Perfect-rhyme tail: from the last stressed vowel to the end. Comparing from the
/// last *syllable* instead would call "nation"/"button" (both -AH0 N) a rhyme.
fn rhyming_part(p: &Pron) -> &[&'static str] {
    match p.iter().rposition(|s| is_stressed_vowel(s)) {
        Some(i) => &p[i..],
        None => &p[..],
    }
}

/// True when ANY pronunciation of `a` rhymes with ANY of `b`. Checking only the
/// first invents false failures on cot/caught (cost = AA1|AO1 S T), homographs
/// (tear = EH1 R | IH1 R) and again = EH1 N | EY1 N.
fn rhymes(a: &[Pron], b: &[Pron]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| rhyming_part(x) == rhyming_part(y)))
}

fn shared_suffix(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x.eq_ignore_ascii_case(y))
        .count()
}

/// Most trailing phones any pronunciation pair shares. Separates an eye-rhyme from
/// a slant rhyme: "good"/"blood" share only [D] and sound nothing alike, while
/// "forest"/"honest" share [AH0 S T] and are a legitimate device. Spelling cannot
/// tell them apart — both merely look like rhymes.
fn shared_phones(a: &[Pron], b: &[Pron]) -> usize {
    let mut best = 0;
    for x in a {
        for y in b {
            let n = x
                .iter()
                .rev()
                .zip(y.iter().rev())
                .take_while(|(s, t)| s == t)
                .count();
            best = best.max(n);
        }
    }
    best
}

/// A pair of line-endings that look like a rhyme but share almost no sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EyeRhyme {
    pub a: String,
    pub b: String,
}

fn last_word(line: &str) -> Option<String> {
    let tail: String = line
        .chars()
        .rev()
        .skip_while(|c| !c.is_ascii_alphabetic() && *c != '\'')
        .take_while(|c| c.is_ascii_alphabetic() || *c == '\'')
        .collect();
    let w: String = tail.chars().rev().collect();
    (!w.is_empty()).then(|| w.to_lowercase())
}

/// Sung phrases, whatever the house format: ACE puts one per line, songgen emits a
/// single ` ; `-separated line whose phrases are period-delimited. Split on all
/// three, drop `[tags]` and `(stage directions)`.
fn lyric_phrases(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('(') && !l.starts_with('#'))
        .flat_map(|l| l.split(';'))
        .flat_map(|seg| seg.split('.'))
        .map(|seg| match seg.rsplit_once(']') {
            Some((_, after)) => after,
            None => seg,
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Line-endings that look like a rhyme but are not one.
///
/// Checks each phrase against the next two, so AABB and ABAB schemes are both
/// covered without having to infer the scheme. Deliberately conservative: it fires
/// only when the spelling agrees, the sound does not, and the pair shares at most
/// two trailing phones — a slant rhyme shares more and is left alone. Empty = clean.
pub fn eye_rhymes(text: &str) -> Vec<EyeRhyme> {
    let words: Vec<String> = lyric_phrases(text)
        .iter()
        .filter_map(|l| last_word(l))
        .collect();
    let mut out: Vec<EyeRhyme> = Vec::new();
    for (i, a) in words.iter().enumerate() {
        for b in words.iter().skip(i + 1).take(2) {
            if a == b {
                continue;
            }
            let (Some(pa), Some(pb)) = (prons(a), prons(b)) else {
                continue;
            };
            if shared_suffix(a, b) >= 2 && !rhymes(pa, pb) && shared_phones(pa, pb) <= 2 {
                let f = EyeRhyme {
                    a: a.clone(),
                    b: b.clone(),
                };
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// Rewrite instruction naming the offenders, for the user-triggered fix. Dash- and
/// comma-free: songgen forbids both in sung output and the model echoes what it is
/// shown.
pub fn fix_instruction(flags: &[EyeRhyme]) -> Option<String> {
    if flags.is_empty() {
        return None;
    }
    let pairs = flags
        .iter()
        .map(|f| format!("\"{}\" with \"{}\"", f.a, f.b))
        .collect::<Vec<_>>()
        .join(" and ");
    Some(format!(
        "These line endings only share letters. They do not rhyme when sung: {pairs}. \
         Rewrite just those lines so they rhyme by sound. Keep everything else the same."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(w: &str) -> &'static [Pron] {
        prons(w).unwrap_or_else(|| panic!("{w} missing from cmudict"))
    }

    #[test]
    fn vendored_dict_parses() {
        assert!(dict().len() > 100_000, "got {} entries", dict().len());
        assert_eq!(p("good")[0], vec!["G", "UH1", "D"]);
    }

    #[test]
    fn variants_and_trailing_comments_parse() {
        // `again(2)` must land under `again`, not `again(2)`
        assert_eq!(p("again").len(), 2);
        assert!(prons("again(2)").is_none());
        // `d'artagnan D AH0 R T AE1 NG Y AH0 N # foreign french`
        let d = p("d'artagnan");
        assert!(!d[0].contains(&"#"), "comment leaked into phones: {d:?}");
        assert!(
            !d[0].contains(&"foreign"),
            "comment leaked into phones: {d:?}"
        );
    }

    #[test]
    fn good_blood_is_an_eye_rhyme_but_good_hood_is_not() {
        assert!(!rhymes(p("good"), p("blood")));
        assert!(rhymes(p("good"), p("hood")));
        assert!(rhymes(p("blood"), p("flood")));
    }

    #[test]
    fn alternate_pronunciations_are_not_false_failures() {
        assert!(rhymes(p("lost"), p("cost"))); // cot/caught: cost(2) K AO1 S T
        assert!(rhymes(p("again"), p("pain"))); // again(2) AH0 G EY1 N
        assert!(rhymes(p("tear"), p("fear"))); // tear(2) T IH1 R
    }

    #[test]
    fn stressed_vowel_rule_beats_last_syllable_rule() {
        // both end AH0 N; a last-syllable rule calls this a rhyme
        assert!(!rhymes(p("nation"), p("button")));
    }

    /// The shared_phones <= 2 cut is what separates "mistake" from "craft".
    /// Pin the measurements it was chosen from so a future tweak has to face them.
    #[test]
    fn shared_phones_threshold_separates_eye_rhyme_from_slant_rhyme() {
        assert_eq!(shared_phones(p("good"), p("blood")), 1); // [D]
        assert_eq!(shared_phones(p("wood"), p("mood")), 1); // [D]
        assert_eq!(shared_phones(p("bush"), p("brush")), 1); // [SH]
        assert_eq!(shared_phones(p("chase"), p("phase")), 0); // S vs Z
        assert_eq!(shared_phones(p("planned"), p("contained")), 2); // [N D]
        assert_eq!(shared_phones(p("forest"), p("honest")), 3); // [AH0 S T] — spared
        assert_eq!(shared_phones(p("nation"), p("station")), 4); // [EY1 SH AH0 N] — a real rhyme
    }

    #[test]
    fn flags_the_pair_in_real_lyrics() {
        let flags = eye_rhymes("He doesn't want to be good\nHe'd trade a soul for blood\n");
        assert_eq!(flags.len(), 1);
        assert_eq!(
            (flags[0].a.as_str(), flags[0].b.as_str()),
            ("good", "blood")
        );
        let fix = fix_instruction(&flags).unwrap();
        assert!(fix.contains("\"good\" with \"blood\""));
        assert!(!fix.contains('—') && !fix.contains('-') && !fix.contains(','));
    }

    #[test]
    fn abab_schemes_are_checked_not_just_couplets() {
        // good/blood are two apart — a couplet-only check misses this entirely
        let flags = eye_rhymes(
            "he will not be good\nunder a pale tree\nhe dreams of blood\nand wants to be free\n",
        );
        assert_eq!(flags.len(), 1, "{flags:?}");
        assert_eq!(
            (flags[0].a.as_str(), flags[0].b.as_str()),
            ("good", "blood")
        );
    }

    #[test]
    fn songgen_single_line_format_is_split_and_checked() {
        let s = "[intro-short] ; [verse] He does not want to be good. \
                 He would trade a soul for blood. ; [outro-short]";
        let flags = eye_rhymes(s);
        assert_eq!(flags.len(), 1, "expected good/blood, got {flags:?}");
    }

    #[test]
    fn slant_rhymes_are_left_alone() {
        // SONGGEN_LYRICS_PROMPT's own example — flagging it would rewrite good lyrics
        let s = "[verse] Trails wind through the forest. Trees stand tall and honest.";
        assert!(eye_rhymes(s).is_empty(), "{:?}", eye_rhymes(s));
    }

    #[test]
    fn clean_lyrics_flag_nothing() {
        assert!(eye_rhymes("a puppet of the wire\nsmothered by the fire\n").is_empty());
    }

    #[test]
    fn unknown_words_are_skipped_not_flagged() {
        assert!(eye_rhymes("a line ending in zzzqqq\nanother ending in xxxqqq\n").is_empty());
    }
}
