import { describe, expect, it } from 'vitest';
import {
  applyReplacements,
  countMatches,
  docMatches,
  docMatchesFilter,
  filterDocs,
  findMatches,
  indexOfMatch,
  matchAtOrAfter,
  replaceAllAcross,
  replaceAllIn,
  replaceMatch,
  searchMatches,
  stepMatch,
  type PromptDoc
} from './prompt-search';

const docs: PromptDoc[] = [
  { id: 'a', text: 'Mia walks the quay. mia turns.' },
  { id: 'b', text: 'The captain waits.' },
  { id: 'c', text: 'Mia again, and Mia once more.' }
];

describe('findMatches', () => {
  it('is case-insensitive by default and case-sensitive on request', () => {
    expect(findMatches('Mia and mia', 'mia')).toEqual([
      { from: 0, to: 3 },
      { from: 8, to: 11 }
    ]);
    expect(findMatches('Mia and mia', 'mia', { caseSensitive: true })).toEqual([
      { from: 8, to: 11 }
    ]);
  });

  it('never yields overlapping hits', () => {
    // `aa` in `aaaa` is two hits, not three — the scan resumes past the hit it just took.
    expect(findMatches('aaaa', 'aa')).toEqual([
      { from: 0, to: 2 },
      { from: 2, to: 4 }
    ]);
  });

  /**
   * 🔴 The whole reason this module exists rather than a `RegExp`. Every one of these is a needle an
   * H3 author types and a pattern that would mean something else.
   */
  it('treats regex metacharacters as literal text', () => {
    expect(findMatches('a [Shot 1] b', '[Shot 1]')).toEqual([{ from: 2, to: 10 }]);
    expect(findMatches('At 00:04.500, she turns', '00:04.500')).toEqual([{ from: 3, to: 12 }]);
    expect(findMatches('<Subject 1> waits', '<Subject 1>')).toEqual([{ from: 0, to: 11 }]);
    // The pattern reading of `[Shot 1]` is a character class; the literal reading finds nothing.
    expect(findMatches('S h o t', '[Shot 1]')).toEqual([]);
  });

  /**
   * 🔴 `İ`.toLowerCase() is TWO code units, so a search that measured offsets on the folded string
   * would report every later hit shifted left by one — and those offsets are what a replacement
   * rewrites. The exact scan is what stops the wrong bytes being eaten.
   */
  it('keeps offsets exact when folding changes the length of the text', () => {
    const text = 'İstanbul: mia waits';
    expect(findMatches(text, 'mia')).toEqual([{ from: 10, to: 13 }]);
    expect(text.slice(10, 13)).toBe('mia');
    // The pathological character is still findable, and the hit is exactly as long as the needle.
    expect(findMatches(text, 'İSTANBUL')).toEqual([{ from: 0, to: 8 }]);
    expect(text.slice(0, 8)).toBe('İstanbul');
  });

  describe('whole word', () => {
    const opts = { wholeWord: true };

    it('will not match inside a longer word', () => {
      expect(findMatches('Mia went to Miami with Amia', 'mia', opts)).toEqual([{ from: 0, to: 3 }]);
      // Without the toggle all three are hits — that is the behaviour it exists to narrow.
      expect(countMatches('Mia went to Miami with Amia', 'mia')).toBe(3);
    });

    it('matches at both ends of the text', () => {
      expect(findMatches('mia', 'mia', opts)).toEqual([{ from: 0, to: 3 }]);
      expect(findMatches('a mia', 'mia', opts)).toEqual([{ from: 2, to: 5 }]);
      expect(findMatches('mia b', 'mia', opts)).toEqual([{ from: 0, to: 3 }]);
    });

    it('counts punctuation, whitespace and accented letters correctly', () => {
      expect(findMatches('the quay, at dawn', 'quay', opts)).toEqual([{ from: 4, to: 8 }]);
      expect(findMatches('quay-side', 'quay', opts)).toEqual([{ from: 0, to: 4 }]);
      // 🔴 `\p{L}`, not `\w`: an accented letter is part of a word, so this is NOT a whole-word hit.
      expect(findMatches('Miá', 'mi', opts)).toEqual([]);
      expect(findMatches('mia_2', 'mia', opts)).toEqual([]);
    });

    /**
     * 🔴 The rule that makes the toggle safe to leave ON while editing an H3 block: a needle whose own
     * edge is punctuation needs no boundary there, because it already IS one.
     */
    it('does not disqualify a needle that begins or ends in punctuation', () => {
      expect(findMatches('walks<Subject 1>away', '<Subject 1>', opts)).toEqual([
        { from: 5, to: 16 }
      ]);
      expect(findMatches('[Shot 2] At 00:04.500, she turns', '[Shot 2]', opts)).toEqual([
        { from: 0, to: 8 }
      ]);
      expect(findMatches('a 00:04.500, b', '00:04.500,', opts)).toEqual([{ from: 2, to: 12 }]);
    });

    it('composes with the case fold and with replace all', () => {
      expect(findMatches('MIA and Miami', 'mia', opts)).toEqual([{ from: 0, to: 3 }]);
      expect(replaceAllIn('Mia left Miami', 'mia', 'Nell', opts)).toEqual({
        text: 'Nell left Miami',
        count: 1
      });
    });
  });

  it('finds nothing for an empty needle', () => {
    expect(findMatches('anything', '')).toEqual([]);
    expect(countMatches('anything', '')).toBe(0);
  });
});

describe('searchMatches', () => {
  it('walks the docs in the order they were given', () => {
    expect(searchMatches(docs, 'mia')).toEqual([
      { docId: 'a', from: 0, to: 3 },
      { docId: 'a', from: 20, to: 23 },
      { docId: 'c', from: 0, to: 3 },
      { docId: 'c', from: 15, to: 18 }
    ]);
  });
});

describe('the filter', () => {
  it('keeps everything when empty and only the hits otherwise', () => {
    expect(filterDocs(docs, '').map((d) => d.id)).toEqual(['a', 'b', 'c']);
    expect(filterDocs(docs, 'mia').map((d) => d.id)).toEqual(['a', 'c']);
    expect(filterDocs(docs, 'Mia', { caseSensitive: true }).map((d) => d.id)).toEqual(['a', 'c']);
    expect(docMatchesFilter({ id: 'x', text: 'Mia' }, 'mia', { caseSensitive: true })).toBe(false);
  });
});

describe('the cursor', () => {
  const all = searchMatches(docs, 'mia');

  it('wraps at both ends', () => {
    expect(stepMatch(all, null, 1)).toEqual(all[0]);
    expect(stepMatch(all, null, -1)).toEqual(all[3]);
    expect(stepMatch(all, all[3], 1)).toEqual(all[0]);
    expect(stepMatch(all, all[0], -1)).toEqual(all[3]);
  });

  it('restarts rather than refusing when the cursor has gone stale', () => {
    // The author typed in the block under the cursor: those offsets name no hit any more.
    expect(indexOfMatch(all, { docId: 'a', from: 99, to: 102 })).toBe(-1);
    expect(stepMatch(all, { docId: 'a', from: 99, to: 102 }, 1)).toEqual(all[0]);
  });

  it('has nowhere to go in an empty list', () => {
    expect(stepMatch([], null, 1)).toBeNull();
  });

  it('falls forward to a later doc, then round to the start', () => {
    expect(matchAtOrAfter(docs, all, 'a', 5)).toEqual(all[1]);
    expect(matchAtOrAfter(docs, all, 'a', 40)).toEqual(all[2]);
    expect(matchAtOrAfter(docs, all, 'c', 40)).toEqual(all[0]);
  });
});

describe('replaceMatch', () => {
  it('rewrites the one hit and moves on', () => {
    const at = { docId: 'a', from: 0, to: 3 };
    const r = replaceMatch(docs, at, 'mia', 'Nell');
    expect(r?.text).toBe('Nell walks the quay. mia turns.');
    expect(r?.active).toEqual({ docId: 'a', from: 21, to: 24 });
    // The originals are untouched — the caller stores what it is handed back.
    expect(docs[0].text).toBe('Mia walks the quay. mia turns.');
  });

  it('steps over a replacement that contains the needle', () => {
    const one: PromptDoc[] = [{ id: 'a', text: 'Mia and Mia' }];
    const r = replaceMatch(one, { docId: 'a', from: 0, to: 3 }, 'Mia', 'Mia Chen');
    expect(r?.text).toBe('Mia Chen and Mia');
    // The hit inside what was just written is NOT the next stop; the second `Mia` is.
    expect(r?.active).toEqual({ docId: 'a', from: 13, to: 16 });
  });

  /**
   * 🔴 The guard that keeps a live editor from being corrupted by a stale cursor: the offsets are
   * held by the screen while the author can type into any block on it.
   */
  it('refuses a range that no longer holds the needle', () => {
    expect(replaceMatch(docs, { docId: 'a', from: 4, to: 7 }, 'mia', 'Nell')).toBeNull();
    expect(replaceMatch(docs, { docId: 'zz', from: 0, to: 3 }, 'mia', 'Nell')).toBeNull();
    expect(replaceMatch(docs, { docId: 'a', from: 0, to: 3 }, '', 'Nell')).toBeNull();
  });

  it('honours case sensitivity when checking the range', () => {
    const at = { docId: 'a', from: 20, to: 23 };
    expect(replaceMatch(docs, at, 'Mia', 'Nell', { caseSensitive: true })).toBeNull();
    expect(replaceMatch(docs, at, 'mia', 'Nell', { caseSensitive: true })?.text).toBe(
      'Mia walks the quay. Nell turns.'
    );
  });
});

/**
 * The section scope. `window` is absolute offsets, so everything downstream stays offset arithmetic on
 * the whole text — see the module header for why an ABSENT window and an EMPTY one are opposites.
 */
describe('the window', () => {
  // Offsets: 'head: mia here' is 0..14; 'tail: mia there' starts at 15.
  const text = 'head: mia here\ntail: mia there';
  const head = { from: 0, to: 14 };
  const tail = { from: 15, to: text.length };

  it('restricts the hits to the range', () => {
    expect(docMatches({ id: 'a', text, window: head }, 'mia')).toEqual([{ from: 6, to: 9 }]);
    expect(docMatches({ id: 'a', text, window: tail }, 'mia')).toEqual([{ from: 21, to: 24 }]);
    expect(docMatches({ id: 'a', text }, 'mia').length).toBe(2);
    expect(docMatches({ id: 'a', text, window: null }, 'mia').length).toBe(2);
  });

  it('reports offsets in the WHOLE text, not in the slice', () => {
    const [hit] = docMatches({ id: 'a', text, window: tail }, 'mia');
    expect(text.slice(hit.from, hit.to)).toBe('mia');
  });

  /** A section the block does not have. The opposite of `null`, and the reason it is not `null`. */
  it('matches nothing when empty', () => {
    expect(docMatches({ id: 'a', text, window: { from: 0, to: 0 } }, 'mia')).toEqual([]);
    expect(docMatchesFilter({ id: 'a', text, window: { from: 0, to: 0 } }, 'mia')).toBe(false);
  });

  it('scopes the filter to the same range as the search', () => {
    const docs: PromptDoc[] = [
      { id: 'a', text, window: head },
      { id: 'b', text: 'head: nothing\ntail: mia', window: { from: 0, to: 13 } }
    ];
    expect(filterDocs(docs, 'mia').map((d) => d.id)).toEqual(['a']);
  });

  it('scopes replace all, leaving the rest of the block alone', () => {
    expect(replaceAllAcross([{ id: 'a', text, window: tail }], 'mia', 'Nell')).toEqual({
      changed: [{ id: 'a', text: 'head: mia here\ntail: Nell there' }],
      count: 1
    });
  });

  it('grows with a longer replacement so the last hit in a section is not skipped', () => {
    // Two hits in the window; replacing the first lengthens the text by 5.
    const both = { id: 'a', text: 'mia and mia. outside: mia', window: { from: 0, to: 12 } };
    const r = replaceMatch([both], { docId: 'a', from: 0, to: 3 }, 'mia', 'Mia Chen');
    expect(r?.text).toBe('Mia Chen and mia. outside: mia');
    // The second in-window hit, NOT the one outside it and not nothing.
    expect(r?.active).toEqual({ docId: 'a', from: 13, to: 16 });
  });

  it('refuses a cursor that the window no longer includes', () => {
    // The offsets name a real `mia`, but it is outside the scope now in force.
    expect(
      replaceMatch(
        [{ id: 'a', text, window: head }],
        { docId: 'a', from: 21, to: 24 },
        'mia',
        'Nell'
      )
    ).toBeNull();
  });
});

describe('applyReplacements', () => {
  it('writes the given hits and nothing else', () => {
    expect(applyReplacements('a b a', [{ from: 0, to: 1 }], 'X')).toEqual({
      text: 'X b a',
      count: 1
    });
    expect(applyReplacements('a b a', [], 'X')).toEqual({ text: 'a b a', count: 0 });
  });
});

describe('replace all', () => {
  it('rewrites every hit in one pass without re-scanning its own output', () => {
    expect(replaceAllIn('cat cat', 'cat', 'the cat')).toEqual({
      text: 'the cat the cat',
      count: 2
    });
  });

  it('keeps the surrounding bytes when folding case', () => {
    expect(replaceAllIn('Mia said mia', 'mia', 'Nell')).toEqual({
      text: 'Nell said Nell',
      count: 2
    });
  });

  it('treats `$&` in the replacement as text', () => {
    expect(replaceAllIn('a cat', 'cat', '$& dog').text).toBe('a $& dog');
  });

  it('does nothing for an empty needle', () => {
    expect(replaceAllIn('anything', '', 'x')).toEqual({ text: 'anything', count: 0 });
    expect(replaceAllAcross(docs, '', 'x')).toEqual({ changed: [], count: 0 });
  });

  it('reports only the docs it changed', () => {
    expect(replaceAllAcross(docs, 'mia', 'Nell')).toEqual({
      changed: [
        { id: 'a', text: 'Nell walks the quay. Nell turns.' },
        { id: 'c', text: 'Nell again, and Nell once more.' }
      ],
      count: 4
    });
  });

  it('is scoped to the docs it is handed — which is what the filter decides', () => {
    const shown = filterDocs(docs, 'quay');
    expect(replaceAllAcross(shown, 'mia', 'Nell')).toEqual({
      changed: [{ id: 'a', text: 'Nell walks the quay. Nell turns.' }],
      count: 2
    });
  });
});
