//! Turning a user's rant into MiniMax Music 3's two inputs: a structured CAPTION and tagged LYRICS.
//!
//! # Where the rules come from
//!
//! MiniMax publishes an official agent skill for exactly this task —
//! `skills/music-caption-rewriter/` in <https://github.com/MiniMax-AI/MiniMax-Music3>. It is
//! `SKILL.md` (the method), `references/genre-router.md` (an 18-family routing table plus an alias
//! list), 18 family indexes of compact style cards, and **1000 complete caption exemplars**. The
//! output contract, the precedence rules and the validation checklist below are that skill's, not
//! invented here.
//!
//! # What of it koblem can actually run, and what was dropped
//!
//! The skill's method is *progressive disclosure*: build a private brief → read the router → read
//! one or two family indexes → select up to three templates by role (Foundation / Modifier /
//! Arrangement) → synthesise. That is written for an agent with a filesystem and several turns.
//! koblem's music path is a **single** `chat_completion_json` call against the resident 9B
//! (`routes::music`'s `suggest` is the shape), with no tool loop and no retrieval step.
//!
//! So this is the hybrid:
//!
//! * **The router is executed in Rust, deterministically** ([`route_family`]), from the same family
//!   map and alias table the skill's router file publishes. A keyword table is what that file
//!   actually specifies ("Normalize explicit genre names and cultural-market terms with the aliases
//!   below"), so running it here costs nothing and spends no LLM round trip on a decision a lookup
//!   makes.
//! * **One exemplar per family is vendored** (`music3_captions/*.txt`, 18 files, ~68 KB) and the
//!   routed one is inlined as a REGISTER reference. The remaining 982 templates are dropped: they
//!   are 5.6 MB, they cannot be searched without the retrieval loop this path does not have, and a
//!   second exemplar of the same family teaches the 9B nothing the first did not.
//! * **The family indexes are dropped entirely.** Their job is to let an agent *choose* among cards;
//!   with the choice already made in Rust they are dead weight in a system prompt.
//!
//! 🔴 The exemplar is the part most likely to be misused, and the skill says so: *"Do not copy
//! sentences, distinctive phrases, or a template's complete structure."* A 9B handed a worked
//! example will lift its BPM, its key, its instruments and its story unless told not to, in the
//! imperative, next to the example. [`REGISTER_RULES`] is that instruction and it is load-bearing.
//!
//! # Why the reply is delimited, not JSON
//!
//! `routes::music` learned this on the acestep path and wrote it down: *"full lyrics (newlines,
//! quotes, [tags]) break JSON escaping too often — the model double-encodes or mis-escapes and the
//! whole reply lands in `caption`."* Music 3's caption is 250–450 words of prose AND its lyrics
//! carry bracket tags, so it is the same failure with more surface. The model therefore emits
//! `###CAPTION### … ###LYRICS###`, which is impossible to mis-escape, and the ROUTE serialises the
//! parsed halves into the strict `{"caption", "lyrics"}` JSON the UI consumes. Strict JSON on the
//! wire, delimiters on the model's side.
//!
//! # What must NOT be done to the text on the way out
//!
//! The model preprocesses its own inputs (`comfy/ldm/minimax_music/prompt.py`): `clean_caption()`
//! strips markdown and rewrites `<|foo bar|>` into `foo is bar`; `normalize_lyrics()` lowercases
//! every `[Tag]`, turns ` ^ ` into a newline and prepends `[start]`. So koblem lowercases nothing,
//! strips nothing and prepends nothing — the caption and lyrics go over the wire as written. The
//! only thing this module does is stop the 9B emitting markdown headings that would be stripped
//! anyway, because a heading the model deletes is a heading the user edited for nothing.

/// The three top-level headings, in order. The model was trained on this skeleton; it is not a
/// presentation choice.
///
/// Test-only: the headings ship inside [`CAPTION_RULES`] as literal text, and this is what the
/// contract tests assert that text still contains.
#[cfg(test)]
pub const CAPTION_HEADINGS: [&str; 3] = ["Global Metadata", "Vocal Details", "Arrangement"];

/// The caption contract — SKILL.md's "Output Contract" and "Validate Before Returning", distilled.
///
/// The sub-headings are the corpus's own (`Basic Attributes:`, `Global Emotional Progression:`,
/// …). They are offered rather than mandated: every vendored exemplar uses them, so the 9B
/// reproduces them from the example, and naming them here stops it inventing a fourth section.
const CAPTION_RULES: &str = "\
The CAPTION is the model's main input. Write it as EXACTLY three top-level sections, in this \
order, with these exact names on their own lines:\n\
\n\
Global Metadata\n\
  - genre and subgenres, and the cultural or market style when the user implies one\n\
  - tempo, key and scale — see the fabrication rule below\n\
  - the emotional progression across the song (how it starts, deepens, resolves)\n\
  - the listening scenario or imagery it belongs to\n\
  - the production profile: soundstage, frequency character, dynamics, era of production\n\
Vocal Details\n\
  - lead configuration, timbre, register and delivery\n\
  - harmony and backing vocals, and restrained vocal effects\n\
  - for an INSTRUMENTAL piece: say it is instrumental and name the instrument carrying the \
lead melodic role\n\
Arrangement\n\
  - a section-by-section TIMELINE: for each section say what enters, exits, changes or \
intensifies\n\
  - primary and secondary instrument lifecycles, groove development, transitions\n\
  - embellishments, textures and spatial effects, where they matter\n\
\n\
Aim for roughly 250-450 words in total. Prefer concrete musical changes over decorative prose: an \
energy arc a player could follow, not a stack of equipment nouns or production adjectives.\n\
\n\
NEVER FABRICATE. Give an exact BPM or key ONLY when the user stated one or it is strongly implied; \
otherwise use a range or a qualitative tempo (\"laid-back\", \"driving\") and leave the key out. \
Never invent a precise vocal gender, a melodic interval or a named production technique when a \
broader description is honest.\n\
\n\
NEVER REVERSE THE USER. An explicit instrumental request stays instrumental — do not add vocals. \
An explicitly named vocal gender, required instrument, tempo limit or prohibited element survives \
into the caption unchanged. When two of the user's own instructions conflict, prefer the more \
specific and later one; otherwise make the smallest musically coherent compromise.\n\
\n\
Do NOT include: a song title, a track ID, any reasoning about your choices, or any line of the \
lyrics. Do NOT use markdown — no #, no **bold**, no bullet characters. Plain lines only.";

/// Lyric rules. The bracket tags are the executable half; everything else is mood.
///
/// 🔴 The second sentence is the whole reason this section is short. ComfyUI's own template note
/// says it outright: *"Tags are the only executable structural instructions; the lyric text itself
/// only conveys mood."* A writer that believes the words drive the arrangement will over-write the
/// lyrics and under-write the caption, which is the opposite of what helps this model.
///
/// # The instrumental clause, and what it is (and is not) based on
///
/// Run against an explicitly instrumental rant ("no singing, no voices, no choir"), the LLM got the
/// caption right — *"The piece is strictly instrumental"*, no lyric words at all — but returned a
/// tag sheet of `[Intro] [Verse] [Pre-Chorus] [Chorus] [Instrumental] [Outro]` with nothing under
/// any of it. Since the tags are the ONLY part the model executes as structure, a wordless sheet
/// carrying *vocal-section* markers is a mixed signal.
///
/// ⚠️ **It is NOT measured that this makes the model sing.** Nobody rendered it. The rule below is
/// the conservative reading — a vocal-section tag on a piece the user said has no voices is at best
/// meaningless and at worst an instruction to vocalise, and naming the four safe tags costs
/// nothing. If someone does render the comparison and it turns out `[Verse]` on a wordless sheet is
/// inert, this can be relaxed to a preference; until then it is stated as a rule because the
/// downside is asymmetric.
const LYRIC_RULES: &str = "\
The LYRICS carry the song's structure through SECTION TAGS. The tags are the only part the model \
executes as structure; the words themselves only convey mood, so put the musical intent in the \
CAPTION and let the lyrics be lyrics.\n\
\n\
- Use these tags, each alone on its own line, where that section begins: [Intro] [Verse] \
[Pre-Chorus] [Chorus] [Post-Chorus] [Bridge] [Instrumental] [Solo] [Outro]. Write them in this \
capitalisation; the model normalises them itself.\n\
- [Instrumental] and [Solo] sections carry no words — leave them empty.\n\
- Blank line between sections.\n\
- Parentheses are backing vocals actually sung underneath, e.g. \"we rise (we rise)\".\n\
- Keep lines singable: roughly 6-10 syllables, one breath each.\n\
- Rhyme by SOUND, not spelling. A natural near-rhyme beats a forced one that bends a word.\n\
- Stay on one core image and develop it. Avoid stacked buzzwords like \"neon skies, electric \
hearts, endless dreams\".\n\
- Match the caption: same mood, same energy arc, same sections.\n\
- No title, no markdown, no commentary.\n\
\n\
INSTRUMENTAL PIECES. If the request is instrumental — no singing, no voices — write ONLY these \
tags and no words at all: [Intro], [Instrumental], [Solo], [Outro]. Do NOT use [Verse], \
[Pre-Chorus], [Chorus], [Post-Chorus] or [Bridge] on an instrumental sheet. Those are \
vocal-section markers, and putting them on a piece with no words tells the model to build \
sung sections for a song the user said has no voices. Structure an instrumental as \
[Intro] then repeated [Instrumental] and [Solo] sections then [Outro].";

/// How the inlined exemplar may be used. Sits immediately before the example in the system prompt.
///
/// 🔴 Without this the 9B lifts the exemplar's BPM, key, instruments and emotional story wholesale
/// and returns a caption for a song nobody asked for — which reads as plausible output and is the
/// hardest failure here to notice.
const REGISTER_RULES: &str = "\
Below is ONE caption from the model's own reference set, in the style family closest to this \
request. It is a REGISTER REFERENCE ONLY — it shows how specific and how structured a good caption \
is.\n\
\n\
Do NOT reuse its words, sentences, BPM, key, instruments, vocalist, emotional story or section \
order. Do NOT copy its structure wholesale. Write a NEW caption for the user's song. If the user's \
request contradicts anything in the example, the user wins every time.";

// ── the genre router ──────────────────────────────────────────────────────────────────────────

/// One style family from the official router, its vendored exemplar, and the cues that select it.
struct Family {
    /// The router's own family slug.
    id: &'static str,
    /// The exemplar inlined when this family wins.
    exemplar: &'static str,
    /// Positive cues: the router's "Family map" column, plus a modest set of INSTRUMENTATION and
    /// context cues. Normalised to lowercase space-separated words; a multi-word cue outranks a
    /// single-word one — see [`route_family`].
    ///
    /// 🔴 The instrumentation half exists because SKILL.md says routing should use *"genre, groove,
    /// instrumentation, and cultural context"* and this table originally used genre words only —
    /// so a rant that names no genre ("just me and a guitar in a room") fell through to the pop
    /// fallback. It is deliberately MODEST, because the blast radius was measured and is small: fed
    /// the wrong family's exemplar, the LLM still wrote the right genre from the user's own words
    /// (an acoustic rant given the pop-ballad exemplar still produced "Acoustic Folk, Indie
    /// Singer-Songwriter", no drums, no synths). The exemplar supplies stylistic REGISTER; the
    /// caption rules and the rant dominate. A mis-route costs a little register fidelity, not a
    /// wrong-genre song — so this is worth a cue list, not a classifier.
    ///
    /// ⚠️ Ambiguous instruments are entered as QUALIFIED phrases, never bare, so length-squared
    /// scoring resolves them: bare `guitar` reads acoustic, while `distorted guitar` /
    /// `electric guitar` / `guitar riff` outscore it 4-to-1 for rock.
    cues: &'static [&'static str],
}

/// The 18 families, in the router's own order.
///
/// 🔴 Order is the tie-break, so it matters: it runs specific → general, and `general-pop-ballad`
/// is last because the router calls it "fallback when no more specific genre family is supported".
const FAMILIES: &[Family] = &[
    Family {
        id: "east-asian-modern",
        exemplar: include_str!("music3_captions/east-asian-modern.txt"),
        cues: &[
            "mandopop",
            "c pop",
            "cpop",
            "cantopop",
            "j pop",
            "jpop",
            "k pop",
            "kpop",
            "chinese pop",
            "taiwanese pop",
            "korean pop",
            "japanese pop",
        ],
    },
    Family {
        id: "east-asian-ballad-heritage",
        exemplar: include_str!("music3_captions/east-asian-ballad-heritage.txt"),
        cues: &[
            "mandopop ballad",
            "c pop ballad",
            "cantopop ballad",
            "j pop ballad",
            "guofeng",
            "guofeng pop",
            "chinese ballad",
        ],
    },
    Family {
        id: "modern-rnb-neo-soul",
        exemplar: include_str!("music3_captions/modern-rnb-neo-soul.txt"),
        cues: &[
            "r and b",
            "rnb",
            "r n b",
            "contemporary r b",
            "alternative r b",
            "neo soul",
            "trap soul",
            "atmospheric r b",
            // instrumentation
            "rhodes",
            "wurlitzer",
        ],
    },
    Family {
        id: "soul-blues-gospel",
        exemplar: include_str!("music3_captions/soul-blues-gospel.txt"),
        cues: &[
            "soul",
            "blues",
            "blues rock",
            "gospel",
            "worship",
            "soul blues",
            "motown",
            // instrumentation
            "hammond",
            "hammond organ",
            "harmonica",
            "horn section",
            "gospel choir",
        ],
    },
    Family {
        id: "cinematic-pop-ballad",
        exemplar: include_str!("music3_captions/cinematic-pop-ballad.txt"),
        cues: &[
            "cinematic pop",
            "cinematic ballad",
            "orchestral pop",
            "soundtrack ballad",
            // Context. A song FOR a film is a song; `film score` (4) outscores bare `film` (1)
            // and keeps score-led writing on the orchestral family where it belongs.
            "film",
            "movie",
            "end credits",
            "closing credits",
            "final scene",
        ],
    },
    Family {
        id: "cinematic-orchestral-epic",
        exemplar: include_str!("music3_captions/cinematic-orchestral-epic.txt"),
        cues: &[
            "film score",
            "orchestral",
            "orchestra",
            "trailer",
            "epic choral",
            "choir",
            "symphonic soundtrack",
            "symphony",
            "contemporary classical",
            "score",
            // Instrumentation. 🔴 `strings` is deliberately NOT bare: a piano ballad with "a bit
            // of string swell" is a ballad, not a score, and a bare cue would drag it here.
            "string section",
            "sweeping strings",
            "orchestral strings",
            "timpani",
            "french horn",
        ],
    },
    Family {
        id: "electronic-synth-ambient-pop",
        exemplar: include_str!("music3_captions/electronic-synth-ambient-pop.txt"),
        cues: &[
            "synth pop",
            "synthpop",
            "electropop",
            "electro pop",
            "dream pop",
            "ambient pop",
            "ambient",
            "darkwave",
            "retrowave",
            "synthwave",
            "downtempo",
            "shoegaze",
            "chillwave",
            // instrumentation
            "synth",
            "synths",
            "synthesizer",
            "arpeggiator",
            "analog synth",
            "drum machine",
        ],
    },
    Family {
        id: "jazz-swing-big-band",
        exemplar: include_str!("music3_captions/jazz-swing-big-band.txt"),
        cues: &[
            "jazz",
            "vocal jazz",
            "jazz ballad",
            "big band",
            "swing",
            "bossa nova",
            "lounge jazz",
            "bebop",
            // instrumentation
            "upright bass",
            "double bass",
            "walking bass",
            "saxophone",
            "muted trumpet",
            "brass section",
        ],
    },
    Family {
        id: "traditional-vocal-stage",
        exemplar: include_str!("music3_captions/traditional-vocal-stage.txt"),
        cues: &[
            "traditional pop",
            "crooner",
            "doo wop",
            "a cappella",
            "acappella",
            "musical theatre",
            "musical theater",
            "show tune",
            "cabaret",
            "barbershop",
        ],
    },
    Family {
        id: "hip-hop-rap",
        exemplar: include_str!("music3_captions/hip-hop-rap.txt"),
        cues: &[
            "hip hop",
            "hiphop",
            "rap",
            "trap",
            "drill",
            "lo fi hip hop",
            "lofi hip hop",
            "conscious rap",
            "melodic rap",
            "boom bap",
            "chillhop",
            "lo fi",
            "lofi",
            // instrumentation
            "808",
            "808s",
            "turntable",
            "turntables",
            "scratching",
            "vinyl crackle",
        ],
    },
    Family {
        id: "metal-heavy-rock",
        exemplar: include_str!("music3_captions/metal-heavy-rock.txt"),
        cues: &[
            "metal",
            "metalcore",
            "power metal",
            "symphonic metal",
            "nu metal",
            "hard rock",
            "post hardcore",
            "hardcore",
            "djent",
            "thrash",
            "doom",
            // instrumentation / technique
            "double kick",
            "blast beat",
            "palm muted",
            "breakdown riff",
            "growled",
        ],
    },
    Family {
        id: "pop-alternative-rock",
        exemplar: include_str!("music3_captions/pop-alternative-rock.txt"),
        cues: &[
            "pop rock",
            "alternative rock",
            "alt rock",
            "indie rock",
            "arena rock",
            "j rock",
            "punk",
            "pop punk",
            "post grunge",
            "grunge",
            "garage rock",
            "emo",
            "rock",
            // 🔴 Qualified guitar phrases, so they outscore the bare `guitar` that reads acoustic.
            "electric guitar",
            "distorted guitar",
            "guitar riff",
            "power chord",
            "fuzz",
        ],
    },
    Family {
        id: "contemporary-folk-acoustic",
        exemplar: include_str!("music3_captions/contemporary-folk-acoustic.txt"),
        cues: &[
            "indie folk",
            "contemporary folk",
            "folk pop",
            "singer songwriter",
            "acoustic pop",
            "acoustic",
            "fingerpicked",
            "fingerpicking",
            // 🔴 Bare `guitar` reads acoustic ON PURPOSE — "just me and a guitar in a room" is the
            // case this whole cue pass exists for. Any rock qualifier above beats it 4-to-1.
            "guitar",
            "acoustic guitar",
            "nylon string",
            "steel string",
            "mandolin",
            "ukulele",
        ],
    },
    Family {
        id: "roots-traditional-global",
        exemplar: include_str!("music3_captions/roots-traditional-global.txt"),
        cues: &[
            "traditional folk",
            "celtic",
            "chinese traditional",
            "folk blues",
            "reggae",
            "ska",
            "maritime",
            "sea shanty",
            "shanty",
            "world music",
            "afrobeat",
            "flamenco",
            "klezmer",
            // instrumentation
            "bagpipes",
            "tin whistle",
            "sitar",
            "koto",
            "erhu",
            "guzheng",
            "celtic fiddle",
        ],
    },
    Family {
        id: "dance-pop-disco-funk",
        exemplar: include_str!("music3_captions/dance-pop-disco-funk.txt"),
        cues: &[
            "dance pop",
            "nu disco",
            "disco",
            "funk pop",
            "funk",
            "groove pop",
            "boogie",
            // instrumentation
            "slap bass",
            "clavinet",
            "wah guitar",
            "string stabs",
        ],
    },
    Family {
        id: "club-edm-house-trance",
        exemplar: include_str!("music3_captions/club-edm-house-trance.txt"),
        cues: &[
            "edm",
            "house",
            "deep house",
            "tech house",
            "trance",
            "hardstyle",
            "dubstep",
            "techno",
            "drum and bass",
            "dnb",
            "festival",
            "big room",
            "rave",
            // instrumentation / production
            "supersaw",
            "sidechain",
            "four on the floor",
            "the drop",
            "build and drop",
        ],
    },
    Family {
        id: "country-americana",
        exemplar: include_str!("music3_captions/country-americana.txt"),
        cues: &[
            "country",
            "americana",
            "bluegrass",
            "country rock",
            "country pop",
            "rockabilly",
            "honky tonk",
            "banjo",
            // instrumentation. `fiddle` lands here rather than roots: it is the commoner reading,
            // and roots carries the qualified `celtic fiddle` to win when that is meant.
            "pedal steel",
            "fiddle",
            "dobro",
            "slide guitar",
            "telecaster",
        ],
    },
    // 🔴 Last, and the fallback. The router: "Use only as fallback when no more specific genre
    // family is supported."
    Family {
        id: "general-pop-ballad",
        exemplar: include_str!("music3_captions/general-pop-ballad.txt"),
        cues: &["pop", "contemporary pop", "pop ballad", "top 40", "chart"],
    },
];

/// Non-Latin cues that cannot be word-split, matched as raw substrings. From the router's
/// "Common aliases" table.
const CJK_ALIASES: &[(&str, &str)] = &[
    ("华语流行", "east-asian-modern"),
    ("国语流行", "east-asian-modern"),
    ("粤语流行", "east-asian-modern"),
    ("国风流行", "east-asian-ballad-heritage"),
    ("国风", "east-asian-ballad-heritage"),
    ("氛围", "modern-rnb-neo-soul"),
    ("另类节奏布鲁斯", "modern-rnb-neo-soul"),
    ("朋克流行", "pop-alternative-rock"),
    ("后摇感", "pop-alternative-rock"),
    ("复古电子", "electronic-synth-ambient-pop"),
];

/// Words the router explicitly demotes: *"Treat `ballad`, `emotional`, `epic`, `modern`, `dark`,
/// and `cinematic` as modifiers unless the input supplies stronger genre, groove, or
/// instrumentation evidence."*
///
/// They are removed from the text BEFORE scoring, so "an epic emotional song" routes on nothing and
/// lands in the fallback family rather than being dragged into `cinematic-orchestral-epic` by a
/// word that describes intensity, not genre.
const DEMOTED_MODIFIERS: &[&str] = &[
    " ballad ",
    " emotional ",
    " epic ",
    " modern ",
    " dark ",
    " cinematic ",
];

/// The routed family: its slug and the exemplar to inline.
pub struct Routed {
    pub family: &'static str,
    pub exemplar: &'static str,
}

/// Normalise for cue matching: lowercase, every separator to a space, collapsed, space-padded so a
/// single-word cue can be matched with word boundaries via plain `contains`.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    let mut last_space = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    if !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

/// Route a brief to one style family.
///
/// Scoring is **by cue length, squared**, and that is the whole trick: "hip hop" is two words and
/// scores 4, while the bare "pop" inside it is one word and scores 1. Without it every compound
/// genre containing the substring `pop` — synth-pop, dance-pop, hip-hop, folk-pop — would route to
/// the generic pop family, which is the single most likely way a keyword router gets this wrong.
///
/// Ties break toward the EARLIER family in [`FAMILIES`], which runs specific → general.
pub fn route_family(brief: &str, caption_hint: Option<&str>) -> Routed {
    let joined = match caption_hint.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("{brief} {c}"),
        None => brief.to_string(),
    };
    let chosen = best_family(&joined).unwrap_or_else(fallback_family);
    Routed {
        family: chosen.id,
        exemplar: chosen.exemplar,
    }
}

/// The general-pop fallback, as the router names it: *"use only as fallback when no more specific
/// genre family is supported"*.
fn fallback_family() -> &'static Family {
    FAMILIES
        .last()
        .expect("FAMILIES is a non-empty const table")
}

/// The scoring core. `None` means **nothing in this text says anything about genre** — a distinction
/// [`route_family`] throws away (it falls back) but [`route_rework`] depends on.
fn best_family(text: &str) -> Option<&'static Family> {
    // A CJK alias is an explicit market term and settles the route on its own.
    let lowered = text.to_lowercase();
    for (alias, family) in CJK_ALIASES {
        if lowered.contains(alias)
            && let Some(f) = FAMILIES.iter().find(|f| f.id == *family)
        {
            return Some(f);
        }
    }

    let mut normalised = normalise(text);
    for m in DEMOTED_MODIFIERS {
        while let Some(at) = normalised.find(m) {
            // Replace with a single space, keeping the padding invariant intact.
            normalised.replace_range(at..at + m.len(), " ");
        }
    }

    let mut best: Option<(&Family, usize)> = None;
    for family in FAMILIES {
        let mut score = 0usize;
        for cue in family.cues {
            let padded = format!(" {cue} ");
            if normalised.contains(&padded) {
                let words = cue.split(' ').filter(|w| !w.is_empty()).count();
                score += words * words;
            }
        }
        if score > 0 && best.map(|(_, b)| score > b).unwrap_or(true) {
            best = Some((family, score));
        }
    }
    best.map(|(f, _)| f)
}

/// Where a rework landed, and whether the instruction actually moved it.
pub struct ReworkRoute {
    pub family: &'static str,
    /// `Some` **only when the instruction changed the family.** See [`route_rework`].
    pub exemplar: Option<&'static str>,
    pub changed: bool,
}

/// Route a rework: the INSTRUCTION decides, and the existing caption only holds the line.
///
/// 🔴 This is not `route_family(instruction, Some(caption))`, and the difference is the whole point.
/// That function CONCATENATES its two arguments and scores the result — so a 300-word caption full
/// of genre words would outvote a four-word instruction every time, and "make it more country"
/// against an indie-folk caption would score folk and stay folk. The intent ("a genre instruction
/// can move families; a structural one must not") needs the instruction scored **on its own**:
///
///   * the instruction names a genre → that family wins, however the caption reads;
///   * it names none ("bigger chorus", "less mopey") → the caption's own family stands.
///
/// The exemplar is returned **only on a change**, and that is a preservation decision rather than a
/// token one. A rework's register is already established by the caption in front of it; inlining a
/// foreign exemplar would pull the rewrite toward that exemplar's style, which is precisely the
/// "quietly regenerated from scratch" failure this endpoint exists to avoid. When the user HAS
/// asked to change genre there is no established register to protect, so the new family's exemplar
/// earns its place.
pub fn route_rework(instruction: &str, caption: &str) -> ReworkRoute {
    let current = best_family(caption);
    match best_family(instruction) {
        // The instruction has genre evidence of its own.
        Some(instructed) => {
            let changed = current.map(|c| c.id != instructed.id).unwrap_or(true);
            ReworkRoute {
                family: instructed.id,
                exemplar: changed.then_some(instructed.exemplar),
                changed,
            }
        }
        // Purely structural or tonal — keep the caption where it is, and hand over no exemplar.
        None => ReworkRoute {
            family: current.unwrap_or_else(fallback_family).id,
            exemplar: None,
            changed: false,
        },
    }
}

// ── system prompts ────────────────────────────────────────────────────────────────────────────

/// The full expander: rant → caption + lyrics, in one pass.
///
/// `duration_hint` is the ceiling the user set. It is passed as guidance for how many sections to
/// write, NOT as a length to hit: the AR stage decides the real length and will end a song early,
/// so asking the writer to pad to a target produces a caption describing a song the model then
/// declines to make.
pub fn system_prompt(routed: &Routed, duration_hint: Option<f64>) -> String {
    format!(
        "You turn the user's description of a song into the two inputs MiniMax Music 3 needs: a \
         structured CAPTION describing the music, and the song's LYRICS.\n\
         \n\
         Capture what the user asked for. Add only what they asked for or clearly implied; where \
         they said nothing, choose something musically coherent and conservative rather than \
         elaborate.\n\
         \n\
         {CAPTION_RULES}\n\
         \n\
         {LYRIC_RULES}\n\
         {}\
         \n\
         {REGISTER_RULES}\n\
         \n\
         --- REGISTER REFERENCE ({}) ---\n\
         {}\n\
         --- END REFERENCE ---\n\
         \n\
         Before you answer, check: all three caption sections present and named exactly; every \
         explicit user constraint preserved; an instrumental request still instrumental; no vocal \
         gender contradicted; no BPM or key invented; no sentence taken from the reference; no \
         title, no lyric line inside the caption.\n\
         \n\
         Output EXACTLY this layout and nothing else — no JSON, no markdown, no commentary:\n\
         ###CAPTION###\n\
         <the three-section caption>\n\
         ###LYRICS###\n\
         <the song lyrics with their section tags>",
        duration_clause(duration_hint),
        routed.family,
        routed.exemplar.trim_end(),
    )
}

/// Caption-only variant, for a user who writes their own lyrics.
pub fn caption_only_prompt(routed: &Routed, duration_hint: Option<f64>) -> String {
    format!(
        "You turn the user's description of a song into a structured CAPTION for MiniMax Music 3 — \
         a description of the music itself. Do not write lyrics.\n\
         \n\
         Capture what the user asked for. Add only what they asked for or clearly implied.\n\
         \n\
         {CAPTION_RULES}\n\
         {}\
         \n\
         {REGISTER_RULES}\n\
         \n\
         --- REGISTER REFERENCE ({}) ---\n\
         {}\n\
         --- END REFERENCE ---\n\
         \n\
         Output EXACTLY this and nothing else — no JSON, no markdown, no commentary:\n\
         ###CAPTION###\n\
         <the three-section caption>",
        duration_clause(duration_hint),
        routed.family,
        routed.exemplar.trim_end(),
    )
}

/// Lyrics-only: the user already has a caption and wants words to fit it.
pub fn lyrics_only_prompt(caption: &str, duration_hint: Option<f64>) -> String {
    format!(
        "You write song lyrics for MiniMax Music 3, to fit a caption the user already has. Match \
         its style, mood, energy arc and vocal description.\n\
         \n\
         {LYRIC_RULES}\n\
         {}\
         \n\
         The caption you are writing to:\n\
         ---\n\
         {}\n\
         ---\n\
         \n\
         Output ONLY the lyrics and their section tags — no caption, no title, no commentary.",
        duration_clause(duration_hint),
        caption.trim(),
    )
}

/// 🔴 The rule that makes Rework safe to use: it is an EDIT, not a regeneration.
///
/// The failure this guards is not the model refusing — it is the model quietly writing a new song.
/// The user asks for a bigger chorus, gets back a caption and lyrics that are entirely different,
/// and loses verses they had already accepted. That is worse than the feature not existing, because
/// it destroys approved work silently and the user only notices later.
///
/// So the instruction is stated three ways — what to change, what to leave, and how to resolve a
/// conflict — because a single "keep the rest the same" is exactly the sort of clause a model
/// honours in spirit while rewriting every sentence.
const PRESERVATION_RULES: &str = "\
YOU ARE EDITING, NOT REWRITING. You are given a caption and lyrics the user has already accepted, \
plus one instruction. Apply the instruction and change NOTHING ELSE.\n\
\n\
- Anything the instruction does not mention comes back materially unchanged. Same sections, same \
order, same instruments, same vocal description, same story, same words. Reproduce them; do not \
re-word them because you would have phrased it differently.\n\
- Change only what the instruction actually reaches. \"Make the chorus hit harder\" touches the \
chorus and whatever leads into it — not the verses, not the genre, not the vocalist.\n\
- Do not renumber, reorder, add or drop sections unless the instruction asks for it.\n\
- Keep the caption's three sections and all the caption rules above. A rework is still a valid \
caption.\n\
- If the instruction CONTRADICTS something already in the caption or lyrics, the instruction \
wins — it is the user's newer and more specific word. Change that thing, and only that thing.\n\
- But never drop an existing constraint the instruction is silent about. If the caption says \
\"no drums\" and the instruction is \"bigger chorus\", the reworked caption still says no drums.\n\
- If the instruction is unclear or you cannot see what it refers to, make the smallest coherent \
change rather than a broad one. A too-small edit is recoverable; a rewrite is not.";

/// Section-count guidance from the duration ceiling.
///
/// 🔴 Phrased as a section budget, never as "make it N seconds long". The model ends the song when
/// the AR stage decides it is over; a writer told to hit a duration pads, and padding a caption
/// makes it vaguer, which is the one thing that reliably degrades this model's output.
fn duration_clause(seconds: Option<f64>) -> String {
    let Some(s) = seconds.filter(|s| s.is_finite() && *s > 0.0) else {
        return String::new();
    };
    let shape = if s < 60.0 {
        "a short piece: an intro, one verse and one chorus is usually the whole song"
    } else if s < 150.0 {
        "a standard single: intro, two verses, a chorus that returns, and a short outro"
    } else if s < 260.0 {
        "a full-length song: room for a bridge, an instrumental section and a final chorus"
    } else {
        "a long piece: room for extended instrumental sections and a developed bridge"
    };
    format!(
        "\nThe user has capped this song at about {:.0} seconds, so plan {shape}. This is a \
         CEILING and the model may end the song sooner — plan the SECTIONS to fit it, never pad \
         the caption or the lyrics to reach a length.\n",
        s
    )
}

/// What a rework is allowed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReworkScope {
    /// Caption and lyrics together — the default, because most feedback ("less mopey") lands on
    /// both and splitting it would leave them describing different songs.
    #[default]
    Both,
    /// Caption only. The lyrics are handed over as read-only context so the caption keeps matching
    /// them, and come back untouched.
    Caption,
    /// Lyrics only, with the caption as read-only context. This is the one that protects an
    /// approved caption from a "just fix the second verse".
    Lyrics,
}

/// The rework prompt: existing caption + lyrics + one instruction.
///
/// `exemplar` is `Some` only when [`route_rework`] found the instruction asking for a different
/// style family — see there for why a rework normally carries none.
pub fn rework_system_prompt(
    scope: ReworkScope,
    exemplar: Option<&str>,
    duration_hint: Option<f64>,
) -> String {
    let mut p = String::from(
        "You revise an existing MiniMax Music 3 caption and lyric sheet to apply one piece of user \
         feedback.\n\n",
    );
    p.push_str(PRESERVATION_RULES);
    p.push('\n');

    // Only the rules for what is actually being rewritten. Handing over the caption contract on a
    // lyrics-only rework invites the model to "improve" a caption it was told to leave alone.
    match scope {
        ReworkScope::Both => {
            p.push_str(&format!("\n{CAPTION_RULES}\n\n{LYRIC_RULES}\n"));
        }
        ReworkScope::Caption => {
            p.push_str(&format!(
                "\n{CAPTION_RULES}\n\nThe lyrics are shown to you as CONTEXT ONLY. Do not rewrite \
                 them and do not return them — the caption must simply stay consistent with them.\n"
            ));
        }
        ReworkScope::Lyrics => {
            p.push_str(&format!(
                "\n{LYRIC_RULES}\n\nThe caption is shown to you as CONTEXT ONLY. Do not rewrite it \
                 and do not return it — the lyrics must simply stay consistent with it.\n"
            ));
        }
    }

    p.push_str(&duration_clause(duration_hint));

    if let Some(exemplar) = exemplar {
        // Reached only on a requested genre CHANGE, where there is no established register to
        // protect — see `route_rework`.
        p.push_str(&format!(
            "\nThe user has asked to move this toward a different style. {REGISTER_RULES}\n\n\
             --- REGISTER REFERENCE ---\n{}\n--- END REFERENCE ---\n",
            exemplar.trim_end()
        ));
    }

    p.push_str(match scope {
        ReworkScope::Both => {
            "\nOutput EXACTLY this layout and nothing else — no JSON, no markdown, no commentary, \
             no explanation of what you changed:\n\
             ###CAPTION###\n<the full revised caption>\n\
             ###LYRICS###\n<the full revised lyrics>"
        }
        ReworkScope::Caption => {
            "\nOutput EXACTLY this and nothing else — no JSON, no markdown, no commentary, no \
             explanation of what you changed:\n\
             ###CAPTION###\n<the full revised caption>"
        }
        ReworkScope::Lyrics => {
            "\nOutput ONLY the full revised lyrics with their section tags — no caption, no \
             markdown, no commentary, no explanation of what you changed."
        }
    });
    // 🔴 "Full" is repeated in every arm on purpose. Asked to revise, a model will happily return
    // only the section it touched; that reads as success and silently truncates the song.
    p
}

/// The user turn for a rework: the material, then the instruction last.
///
/// Instruction LAST is deliberate — it is the thing to act on, and burying it above several hundred
/// words of caption invites the model to treat it as one more piece of context.
pub fn rework_user_turn(caption: &str, lyrics: &str, instruction: &str) -> String {
    let lyrics = lyrics.trim();
    format!(
        "CURRENT CAPTION:\n{}\n\nCURRENT LYRICS:\n{}\n\nAPPLY THIS CHANGE:\n{}",
        caption.trim(),
        if lyrics.is_empty() {
            "(instrumental — no lyrics)"
        } else {
            lyrics
        },
        instruction.trim(),
    )
}

// ── reply parsing ─────────────────────────────────────────────────────────────────────────────

/// What the expander produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Suggestion {
    pub caption: String,
    pub lyrics: Option<String>,
    /// The family the router picked, echoed so the UI can show it and a bad route is visible
    /// rather than inferred from a caption that feels off.
    pub family: String,
}

/// Split a `###CAPTION### … ###LYRICS### …` reply.
///
/// Tolerant by design: a 9B occasionally drops the opening marker, or wraps the whole thing in a
/// code fence. Neither is worth a 502 when the content is right there. What it does NOT do is
/// guess: with no LYRICS marker the whole reply is the caption and `lyrics` is `None`, because
/// silently splitting prose at a blank line would put half a caption in the lyrics box.
pub fn parse_suggestion(content: &str, family: &str) -> Suggestion {
    const CAP: &str = "###CAPTION###";
    const LYR: &str = "###LYRICS###";
    let content = strip_code_fence(content);
    match content.find(LYR) {
        Some(li) => {
            let caption = content[..li].replacen(CAP, "", 1).trim().to_string();
            let lyrics = content[li + LYR.len()..].trim().to_string();
            Suggestion {
                caption: clean_caption_text(&caption),
                lyrics: (!lyrics.is_empty()).then(|| lyrics.to_string()),
                family: family.to_string(),
            }
        }
        None => Suggestion {
            caption: clean_caption_text(&content.replacen(CAP, "", 1)),
            lyrics: None,
            family: family.to_string(),
        },
    }
}

/// Drop a wrapping ```…``` fence if the model added one.
fn strip_code_fence(content: &str) -> &str {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Skip an optional language tag on the opening fence line.
    let body = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => return trimmed,
    };
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// Strip markdown the model added despite being told not to.
///
/// 🔴 This exists because of what happens downstream, not for tidiness: the model's own
/// `clean_caption()` strips `#`, `**`, `*` and bullets before tokenising. So a `## Global Metadata`
/// that survives koblem reaches the user's edit box, gets edited, and is then deleted by the engine
/// anyway — the user tuned something with no effect. Removing it here keeps what the user sees and
/// what the model reads the same text.
///
/// Deliberately narrow: leading heading markers and bullet markers, and inline emphasis. It does
/// not touch brackets, parentheses or punctuation, all of which are meaningful here.
fn clean_caption_text(caption: &str) -> String {
    let mut out = String::with_capacity(caption.len());
    for line in caption.lines() {
        let mut s = line.trim_end();
        // A heading marker: up to three spaces, then #'s, then a space.
        let lead = s.len() - s.trim_start().len();
        if lead <= 3 {
            let t = s.trim_start();
            if let Some(rest) = t.strip_prefix('#') {
                let rest = rest.trim_start_matches('#');
                if let Some(after) = rest.strip_prefix(' ') {
                    s = after;
                }
            } else if let Some(after) = t
                .strip_prefix("- ")
                .or_else(|| t.strip_prefix("* "))
                .or_else(|| t.strip_prefix("+ "))
            {
                s = after;
            }
        }
        out.push_str(&s.replace("**", ""));
        out.push('\n');
    }
    // Collapse the blank-line runs the model's own cleaner would collapse anyway.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

/// A cheap pre-flight on the tokenised-prompt ceiling.
///
/// Returns the estimated token count when it is over [`crate::music3::MAX_PROMPT_TOKENS`], so the
/// caller can refuse before the gate wait rather than after it. See
/// [`crate::music3::CHARS_PER_TOKEN_ESTIMATE`] for why this is deliberately generous.
pub fn over_prompt_budget(caption: &str, lyrics: &str) -> Option<usize> {
    let chars = caption.chars().count() + lyrics.chars().count();
    let estimate = chars / crate::music3::CHARS_PER_TOKEN_ESTIMATE;
    (estimate > crate::music3::MAX_PROMPT_TOKENS).then_some(estimate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 The failure a naive keyword router makes: every compound genre contains "pop", so a bare
    /// substring match sends synth-pop, dance-pop and hip-hop all to the generic pop family. The
    /// length-squared score is what prevents it, and this is the test that would catch its removal.
    #[test]
    fn a_compound_genre_beats_the_bare_pop_inside_it() {
        assert_eq!(
            route_family("dreamy synth-pop with a drum machine", None).family,
            "electronic-synth-ambient-pop"
        );
        assert_eq!(
            route_family("lo-fi hip hop beats to study to", None).family,
            "hip-hop-rap"
        );
        assert_eq!(
            route_family("a disco dance-pop banger", None).family,
            "dance-pop-disco-funk"
        );
        // …and a genuinely generic pop request still reaches the generic family.
        assert_eq!(
            route_family("a catchy pop song about summer", None).family,
            "general-pop-ballad"
        );
    }

    /// 🔴 Instrumentation and context route when no genre word is present. Both of these fell
    /// through to the pop fallback before the cue pass, and the second is the subtler miss:
    /// `emotional` and `cinematic` are DEMOTED modifiers, so a film brief carried no signal at all.
    #[test]
    fn instrumentation_and_context_route_a_genre_free_rant() {
        assert_eq!(
            route_family("just me and a guitar in a room, quiet, sad, honest", None).family,
            "contemporary-folk-acoustic"
        );
        assert_eq!(
            route_family("big emotional song for the end of a film", None).family,
            "cinematic-pop-ballad"
        );
        assert_eq!(
            route_family("pedal steel and a fiddle, dusty and slow", None).family,
            "country-americana"
        );
        assert_eq!(
            route_family("hammond organ, horn section, raw and live", None).family,
            "soul-blues-gospel"
        );
    }

    /// The ambiguous instruments, resolved by length-squared rather than by ordering luck. Each
    /// pair is a cue that would route the wrong way if the qualified phrase were dropped.
    #[test]
    fn a_qualified_instrument_beats_the_bare_one() {
        // Bare `guitar` reads acoustic; any rock qualifier outscores it 4-to-1.
        assert_eq!(
            route_family("layers of distorted guitar and pounding drums", None).family,
            "pop-alternative-rock"
        );
        // A song FOR a film is a song; score-led writing stays orchestral.
        assert_eq!(
            route_family("a sweeping film score with timpani", None).family,
            "cinematic-orchestral-epic"
        );
        // `celtic fiddle` (4) beats country's bare `fiddle` (1).
        assert_eq!(
            route_family("celtic fiddle and tin whistle", None).family,
            "roots-traditional-global"
        );
        // 🔴 The regression this guards: a piano ballad with "string swell" must NOT become a film
        // score. That is why `strings` is only ever entered as a qualified phrase.
        assert_eq!(
            route_family(
                "a slow breakup song, female voice, just piano and a bit of string swell",
                None
            )
            .family,
            "general-pop-ballad"
        );
    }

    /// The router demotes intensity words. "Epic emotional cinematic" describes how a song FEELS
    /// and says nothing about what it IS — routing it to the orchestral-epic family on the strength
    /// of the word "epic" would hand the writer a film-score exemplar for what might be a pop song.
    #[test]
    fn modifiers_alone_fall_through_to_the_fallback_family() {
        assert_eq!(
            route_family("something epic and emotional, really dark and modern", None).family,
            "general-pop-ballad"
        );
        // But real orchestral evidence routes there properly.
        assert_eq!(
            route_family("an epic trailer cue with a full orchestra and choir", None).family,
            "cinematic-orchestral-epic"
        );
        // An empty brief must still return a usable exemplar rather than panicking.
        let empty = route_family("", None);
        assert_eq!(empty.family, "general-pop-ballad");
        assert!(!empty.exemplar.is_empty());
    }

    /// Every family must have a non-empty vendored exemplar carrying the three headings — an
    /// exemplar that lost its skeleton would teach the 9B the wrong shape, silently.
    #[test]
    fn every_vendored_exemplar_carries_the_three_headings() {
        for f in FAMILIES {
            for heading in CAPTION_HEADINGS {
                assert!(
                    f.exemplar.contains(heading),
                    "{} is missing the {heading:?} section",
                    f.id
                );
            }
            assert!(f.exemplar.len() > 500, "{} exemplar looks truncated", f.id);
        }
        assert_eq!(FAMILIES.len(), 18);
    }

    /// The delimiter format exists because JSON escaping loses lyrics. So the parse must survive
    /// exactly what a 9B does to a delimited reply: a code fence, a dropped opening marker, and
    /// markdown it was told not to write.
    #[test]
    fn the_reply_parse_survives_a_fenced_markdown_answer() {
        let reply = "```\n###CAPTION###\n## Global Metadata\n- **Indie folk**, 92 BPM.\n\n\n\
                     Vocal Details\nSoft male lead.\n\nArrangement\nGuitar throughout.\n\
                     ###LYRICS###\n[Verse]\nA line (a line)\n\n[Chorus]\nAnother\n```";
        let s = parse_suggestion(reply, "contemporary-folk-acoustic");
        // The heading marker and the bold are gone — the engine would have stripped them anyway,
        // and leaving them in means the user edits text the model then deletes.
        assert!(s.caption.starts_with("Global Metadata"), "{:?}", s.caption);
        assert!(s.caption.contains("Indie folk, 92 BPM."), "{:?}", s.caption);
        assert!(!s.caption.contains('#'), "{:?}", s.caption);
        assert!(!s.caption.contains("**"), "{:?}", s.caption);
        // Blank-line runs collapsed, as the model's own cleaner does.
        assert!(!s.caption.contains("\n\n\n"), "{:?}", s.caption);
        // 🔴 Lyrics are NOT markdown-cleaned: their brackets and parentheses are the executable
        // half, and "(a line)" is a backing vocal, not emphasis.
        let lyrics = s.lyrics.expect("lyrics present");
        assert!(lyrics.starts_with("[Verse]"), "{lyrics:?}");
        assert!(lyrics.contains("(a line)"), "{lyrics:?}");
        assert_eq!(s.family, "contemporary-folk-acoustic");
    }

    /// No LYRICS marker ⇒ it is all caption. Splitting prose at a blank line to manufacture lyrics
    /// would put half a caption in the lyrics box, which renders as sung text.
    #[test]
    fn a_reply_without_the_lyrics_marker_is_all_caption() {
        let s = parse_suggestion(
            "###CAPTION###\nGlobal Metadata\nJazz.",
            "jazz-swing-big-band",
        );
        assert_eq!(s.lyrics, None);
        assert!(s.caption.contains("Jazz."));
    }

    /// The duration clause must never ask for a length — only for a section budget.
    #[test]
    fn the_duration_clause_asks_for_sections_not_seconds() {
        let c = duration_clause(Some(180.0));
        assert!(c.contains("CEILING"), "{c}");
        assert!(c.contains("never pad"), "{c}");
        assert!(duration_clause(None).is_empty());
        assert!(duration_clause(Some(f64::NAN)).is_empty());
    }

    /// The pre-flight is generous on purpose: it must not refuse a request the tokeniser would
    /// have taken. A normal song is nowhere near the cap; only an essay trips it.
    #[test]
    fn the_prompt_budget_check_passes_a_normal_song() {
        let caption = "Global Metadata\n".repeat(40);
        assert_eq!(over_prompt_budget(&caption, "[Verse]\nwords\n"), None);
        let huge = "x".repeat(crate::music3::MAX_PROMPT_TOKENS * 4 + 400);
        assert!(over_prompt_budget(&huge, "").is_some());
    }

    /// 🔴 A rework routes on the INSTRUCTION, scored alone — not on instruction+caption joined.
    /// Joined, a 300-word caption's genre words outvote a four-word instruction every time, so
    /// "make it more country" against a folk caption would score folk and never move.
    #[test]
    fn a_rework_routes_on_the_instruction_not_the_caption() {
        let folk = "Global Metadata\nIndie folk, acoustic guitar, fingerpicked, singer songwriter.";

        // Genre instruction MOVES the family, and only then is an exemplar handed over.
        let moved = route_rework("make it more country", folk);
        assert_eq!(moved.family, "country-americana");
        assert!(moved.changed);
        assert!(
            moved.exemplar.is_some(),
            "a genre change needs the new register"
        );

        // Structural instruction: no genre evidence, so the caption's family stands.
        let stayed = route_rework("make the chorus hit harder", folk);
        assert_eq!(stayed.family, "contemporary-folk-acoustic");
        assert!(!stayed.changed);
        // 🔴 No exemplar on an ordinary rework: the caption already sets the register, and a
        // foreign example is exactly what would pull an edit into a rewrite.
        assert!(stayed.exemplar.is_none());

        // Naming the family it is ALREADY in is not a change either.
        let same = route_rework("more acoustic guitar", folk);
        assert_eq!(same.family, "contemporary-folk-acoustic");
        assert!(!same.changed);
        assert!(same.exemplar.is_none());

        // The joined form really would have gone the other way — this is the bug being prevented.
        assert_eq!(
            route_family("make it more country", Some(folk)).family,
            "contemporary-folk-acoustic"
        );
    }

    /// The preservation clause is the feature. A refactor that drops it leaves an endpoint that
    /// still looks like it works and silently destroys accepted lyrics.
    #[test]
    fn the_rework_prompt_forbids_rewriting_what_was_not_asked_about() {
        let p = rework_system_prompt(ReworkScope::Both, None, Some(120.0));
        assert!(p.contains("EDITING, NOT REWRITING"), "{p}");
        assert!(p.contains("materially unchanged"), "{p}");
        assert!(p.contains("the instruction wins"), "{p}");
        assert!(p.contains("never drop an existing constraint"), "{p}");
        // Both halves are in scope, so both rule sets and both markers must be present.
        assert!(
            p.contains("###CAPTION###") && p.contains("###LYRICS###"),
            "{p}"
        );
        for heading in CAPTION_HEADINGS {
            assert!(p.contains(heading), "missing {heading}");
        }
        // Instrumental sheets keep the tag rule added for /suggest.
        assert!(p.contains("[Solo]"), "instrumental tag rule missing");

        // Scoped reworks must not invite edits to the half that is off-limits.
        let caption_only = rework_system_prompt(ReworkScope::Caption, None, None);
        assert!(caption_only.contains("CONTEXT ONLY"), "{caption_only}");
        assert!(!caption_only.contains("###LYRICS###"), "{caption_only}");

        let lyrics_only = rework_system_prompt(ReworkScope::Lyrics, None, None);
        assert!(lyrics_only.contains("CONTEXT ONLY"), "{lyrics_only}");
        assert!(!lyrics_only.contains("###CAPTION###"), "{lyrics_only}");
        // The caption contract must NOT be handed over when the caption is off-limits, or the
        // model starts "improving" a caption it was told to leave alone.
        assert!(!lyrics_only.contains("Global Metadata"), "{lyrics_only}");

        // An exemplar appears only when one was routed in (i.e. a genre change).
        let moved = rework_system_prompt(ReworkScope::Both, Some("EXEMPLAR TEXT"), None);
        assert!(moved.contains("EXEMPLAR TEXT"), "{moved}");
        assert!(moved.contains("Do NOT reuse its words"), "copy ban missing");
    }

    /// The instruction goes LAST, and an instrumental sheet says so rather than showing a blank.
    #[test]
    fn the_rework_user_turn_puts_the_instruction_last() {
        let t = rework_user_turn("CAP", "[Verse]\nwords", "less mopey");
        assert!(t.find("CAP").unwrap() < t.find("less mopey").unwrap());
        assert!(t.contains("APPLY THIS CHANGE:\nless mopey"), "{t}");

        // Empty lyrics are labelled, not blank: a bare gap reads as "you forgot to send them" and
        // invites the model to write some for an instrumental piece.
        let instrumental = rework_user_turn("CAP", "   ", "add a solo");
        assert!(
            instrumental.contains("(instrumental — no lyrics)"),
            "{instrumental}"
        );
    }

    /// The whole prompt has to actually carry the contract. A refactor that drops the exemplar or
    /// the copy-ban leaves a prompt that still looks right and produces the reference song.
    #[test]
    fn the_system_prompt_carries_the_contract_and_the_copy_ban() {
        let routed = route_family("a heavy metalcore track", None);
        assert_eq!(routed.family, "metal-heavy-rock");
        let p = system_prompt(&routed, Some(200.0));
        for heading in CAPTION_HEADINGS {
            assert!(p.contains(heading), "missing {heading}");
        }
        assert!(p.contains("REGISTER REFERENCE"), "exemplar not framed");
        assert!(p.contains("Do NOT reuse its words"), "copy ban missing");
        assert!(p.contains("NEVER FABRICATE"), "fabrication rule missing");
        assert!(p.contains("###LYRICS###"), "output layout missing");
        assert!(
            p.contains(routed.exemplar.trim_end()),
            "exemplar not inlined"
        );

        // The caption-only variant must not ask for lyrics at all.
        let c = caption_only_prompt(&routed, None);
        assert!(!c.contains("###LYRICS###"), "caption-only asked for lyrics");
    }
}
