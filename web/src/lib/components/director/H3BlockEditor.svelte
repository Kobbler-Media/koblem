<script lang="ts">
  import { untrack } from 'svelte';
  import { Annotation, Compartment, StateEffect, type Range } from '@codemirror/state';
  import {
    Decoration,
    EditorView,
    ViewPlugin,
    WidgetType,
    keymap,
    placeholder as cmPlaceholder
  } from '@codemirror/view';
  import type { DecorationSet, ViewUpdate } from '@codemirror/view';
  import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
  import { parseH3Block, sectionLabel, type H3SectionName } from '#lib/director/h3-block.ts';
  import { docMatches, type TextWindow } from '#lib/director/prompt-search.ts';
  import {
    h3Decorations,
    h3SectionAt,
    h3SectionBadges,
    h3SectionSpans,
    h3Tokens,
    type H3SectionSpan,
    type H3Token,
    type H3TokenCard
  } from '#lib/director/h3-highlight.ts';

  /**
   * The prompt block, as an editor for the schema it holds.
   *
   * ## 🔴 One layer. The text you see IS the text you are editing.
   *
   * This was a highlight OVERLAY — a transparent `<textarea>` over a `<pre>` of coloured spans,
   * scrolled together — and it failed in the field exactly where an overlay does: in
   * `detailed_description`, the one section made of long wrapped paragraphs. Two layers wrapping
   * independently only agree while every line is short. The moment the two disagree about which
   * column a line breaks at, the caret (drawn by the invisible textarea) stops lining up with the
   * glyphs (drawn by the visible `<pre>`), and typing appears to land in a random place. There is no
   * set of CSS metrics that makes this safe, because the two elements are laid out by different
   * parts of the engine — a textarea's content is laid out by the form-control implementation.
   *
   * So there is no second layer any more. CodeMirror renders the text once, as contenteditable, and
   * the colours are **decorations over that same text** — ranges in the same document, painted on
   * the same glyphs. Nothing can drift, because there is no second thing to drift from.
   *
   * ## What that buys beyond the fix
   *
   * A real document model: undo/redo that understands the edits, IME and composition handled,
   * selection preserved across re-renders, and `posAtCoords` for exact hit-testing (the overlay had
   * to hit-test with `elementsFromPoint` against span rects).
   *
   * ## 🔴 The decorations are built from the EDITOR's own document, never from the `parsed` prop
   *
   * The composer parses on every keystroke too, and passing that parse in would be one keystroke
   * BEHIND this document — the parent has not re-rendered yet when the update listener runs. Stale
   * ranges are not merely mis-coloured: a token range past the end of the document throws inside
   * CodeMirror's range set. So this parses its own doc, once per rebuild, shared between the colours
   * and the hover hit-test list.
   */
  let {
    value,
    docId,
    placeholder = '',
    rows = 22,
    minHeight = 280,
    grow = false,
    search = null,
    resolvedLabels,
    onclaim,
    onshot,
    onsection,
    oninput,
    onblur,
    onlookup,
    ontoken
  }: {
    value: string;
    /**
     * A find-and-replace pass running over this block — see `H3PromptsScreen`, which searches every
     * block in the project at once and hands each editor the same query.
     *
     * 🔴 Painted, never SELECTED. Marking the active hit with `EditorView.selection` would look
     * right and would move the caret in whichever editor holds it — and the caret is what reports
     * `onshot`/`onsection` and drives the dimming of every other section, so walking a search
     * through fourteen blocks would rewrite the composer's lens and re-dim each one on the way past.
     * A decoration paints the same glyphs and touches no state.
     *
     * `null` (the default) is off: nothing extra is painted and nothing scrolls.
     */
    search?: {
      query: string;
      caseSensitive: boolean;
      wholeWord: boolean;
      /**
       * Scope the highlight to one section's body, the way the buttons are scoped — `null` for the
       * whole block, an empty range for a section this block has not got.
       *
       * 🔴 The RULES come down; the ranges do not. Painting hits the buttons would decline to touch
       * is the one way this decoration can lie, so the same `docMatches` decides both — but it is run
       * here, on the editor's own document, for the reason at the top of this file. A window measured
       * one keystroke ago can only mis-scope a hit for a frame; a RANGE measured one keystroke ago can
       * be past the end of the document, which throws inside CodeMirror's range set.
       */
      window: TextWindow | null;
      /** The one hit the buttons act on, when it is in THIS block. Scrolled to; painted brighter. */
      active: { from: number; to: number } | null;
    } | null;
    /**
     * Floor for the editor's height, in px. The default is a full composer's; a screen that stacks
     * one editor per shot passes something smaller so a fourteen-shot project is scrollable.
     *
     * Ignored under {@link grow}, where the height is the document's.
     */
    minHeight?: number;
    /**
     * Size to the DOCUMENT and never scroll internally. `rows`/`minHeight` stop applying.
     *
     * 🔴 This exists for the phone's Prompts list, and it is a touch-scrolling fix rather than a look:
     * a column of fourteen boxes that each scroll their own content is a nested-scroll trap — a drag
     * anywhere over a block moves the block instead of the list, so the page can only be scrolled by
     * catching the thin gaps between cards. Grown, there is exactly one scroller (the list) and the
     * page behaves like a page.
     */
    grow?: boolean;
    /**
     * Every `<Label N>` the composer can answer for. Anything else in the block is marked as
     * naming nothing.
     *
     * ⚠️ Absent and empty mean opposite things: `undefined` is "not told", which marks nothing, and
     * an empty set is "nothing resolves", which marks every label. Collapsing them would paint a
     * whole block red the moment a caller forgot the prop.
     */
    resolvedLabels?: ReadonlySet<string>;
    /**
     * The `[Shot N]` the caret moved into. Fires on a change only, and never with `null` — see the
     * effect for why leaving the body is not a deselection.
     */
    onshot?: (shot: number) => void;
    /**
     * The section the caret moved into. Same discipline as {@link onshot}: on a change only, and
     * never with `null`.
     */
    onsection?: (name: string) => void;
    /**
     * Which document this is — the generation's id.
     *
     * 🔴 Not decoration: a change here empties the undo history, which is what keeps a Ctrl+Z from
     * pulling another render unit's prompt into this one. Compared by VALUE in the sync effect, and
     * never used as an effect dependency — the packer hands down a fresh `doc` object on every
     * keystroke, so the signal behind it invalidates constantly while the string never changes.
     */
    docId: string;
    placeholder?: string;
    rows?: number;
    /**
     * Hand the caller a way to write at the caret, on focus — so a reference pill can insert its
     * ordinal into whichever prose field was last used.
     *
     * ⚠️ A capability, not the element. The old signature passed the `HTMLTextAreaElement` and every
     * caller reached for `selectionStart`; there is no textarea here, and the caret lives in
     * CodeMirror's state rather than on a DOM node.
     */
    onclaim?: (target: { insert: (label: string) => void }) => void;
    oninput: (next: string) => void;
    onblur?: () => void;
    /** What to show when the cursor rests on a token, or `null` for no card. */
    onlookup?: (token: H3Token) => H3TokenCard | null;
    /** The token under the cursor, as it changes. Drives the timeline's marker. */
    ontoken?: (token: H3Token | null) => void;
  } = $props();

  // ── Where the caret is, mirrored out of CodeMirror for the status line ───────
  let caret = $state(0);
  let caretLine = $state(1);
  let focused = $state(false);
  let sections = $state<H3SectionSpan[]>([]);
  /** The tokens of the CURRENT document, cached for hit-testing and for the status line. */
  let docTokens: H3Token[] = [];

  const activeSection = $derived(focused ? h3SectionAt(sections, caret) : null);

  /**
   * The `[Shot N]` the caret is inside, or `null`.
   *
   * 🔴 Reported by the owner as the thing the section name alone could not answer: inside
   * `detailed_description` you are positioned relative to the section AND to a shot, and the section
   * is the less useful of the two — it is the same answer for the whole body.
   */
  const activeShot = $derived.by(() => {
    const main = activeSection?.name;
    if (main !== 'detailed_description' && main !== 'integrated_multimodal_description')
      return null;
    let n: number | null = null;
    for (const t of docTokens) {
      if (t.start > caret) break;
      if (t.kind === 'shot') n = t.shot ?? null;
    }
    return n;
  });

  /**
   * Tell the composer which `[Shot N]` the caret is in, so its panels can follow.
   *
   * ⚠️ Fires only on a CHANGE, and never with `null`. Moving the caret out of the body — into the
   * soundscape, or out of the editor entirely — is not a request to deselect the beat somebody was
   * just editing; it is how you go and read something else and come back.
   */
  let reportedShot: number | null = null;
  $effect(() => {
    const n = activeShot;
    if (n === null || n === untrack(() => reportedShot)) return;
    untrack(() => (reportedShot = n));
    onshot?.(n);
  });

  /**
   * The same for the SECTION, which is what points the composer's lens at the editor for the bytes
   * the caret is standing in — including the five sections that hold no `[Shot N]` at all.
   *
   * ⚠️ Held to the same two rules, and the second one is the one that matters here: `null` is what
   * both a blur and a caret between two sections produce, so reporting it would throw the lens off
   * the panel somebody was typing into the moment they clicked away from the block.
   */
  let reportedSection: string | null = null;
  $effect(() => {
    const name = activeSection?.name ?? null;
    if (name === null || name === untrack(() => reportedSection)) return;
    untrack(() => (reportedSection = name));
    onsection?.(name);
  });

  // ── The decorations ─────────────────────────────────────────────────────────

  const MARKS: Record<string, Decoration> = {};
  /** One `Decoration.mark` per class, reused — CodeMirror compares them by identity. */
  const markFor = (cls: string) => (MARKS[cls] ??= Decoration.mark({ class: cls }));

  /**
   * What each section is for, in the fewest words that still say it.
   *
   * Presentation, so it lives here rather than beside the measurements in `h3-highlight`: that
   * module reports the offset and the size and this one decides the wording.
   */
  const SECTION_GLOSS: Record<H3SectionName, string> = {
    subject_definitions: 'who and what',
    summary: 'what this is',
    retention_analysis: 'what survives',
    detailed_description: 'the shot itself',
    integrated_multimodal_description: 'the shot itself',
    overall_soundscape: 'what it sounds like',
    non_diegetic_music: 'the score'
  };

  /**
   * The badge hanging off a header line.
   *
   * 🔴 **A widget, so it is not in the block.** The gloss and the character count are things to know
   * ABOUT a section, not text the model should read — written into the document they would eat the
   * 7,000-character budget and be sent. As a widget they are not in `doc.toString()`, so they are
   * never selected, copied or compiled.
   *
   * Zero width, so there is nothing for the caret to step into and `atomicRanges` would be
   * machinery guarding a case that cannot arise.
   */
  class BadgeWidget extends WidgetType {
    label: string;
    empty: boolean;
    // Plain fields rather than constructor parameter properties: Svelte's script is not run through
    // a TypeScript preprocessor, so that syntax does not survive the compiler.
    constructor(label: string, empty: boolean) {
      super();
      this.label = label;
      this.empty = empty;
    }
    // CodeMirror reuses a widget whose `eq` says it is unchanged, which is what keeps the badge from
    // being torn down and rebuilt on every keystroke that does not alter it.
    eq(other: BadgeWidget) {
      return other.label === this.label && other.empty === this.empty;
    }
    toDOM() {
      const el = document.createElement('span');
      el.className = `h3-badge${this.empty ? ' h3-badge-empty' : ''}`;
      el.textContent = this.label;
      el.setAttribute('aria-hidden', 'true');
      return el;
    }
    ignoreEvent() {
      return true;
    }
  }

  const BADGES: Record<string, Decoration> = {};
  const badgeFor = (label: string, empty: boolean) =>
    (BADGES[`${label}|${empty}`] ??= Decoration.widget({
      widget: new BadgeWidget(label, empty),
      // After the colon rather than before whatever follows it, so an inline FL2VA header reads
      // `name:` badge `[Shot 1]` and never splits the body's first token.
      side: 1
    }));

  /** Marks a transaction as an echo of the parent's own value rather than something the author did. */
  const fromParent = Annotation.define<boolean>();
  /** Asks the highlighter to rebuild when something outside the document changed what it paints. */
  const repaint = StateEffect.define<null>();
  /** Holds `history()` so it can be reconfigured — i.e. emptied — when the document changes. */
  const historyBox = new Compartment();
  /**
   * Holds the height rules, so {@link grow} can be flipped without rebuilding the view.
   *
   * In a compartment rather than read once at construction because the prop is derived from the
   * viewport on the phone shell, and a rotation that changed it while a view was alive would
   * otherwise leave an editor sized by rules nobody is applying any more.
   */
  const heightBox = new Compartment();
  /** Which generation the view currently holds, compared by VALUE. See the sync effect. */
  let shownDocId: string | null = null;
  /** Which height mode the view currently holds. Set at creation; compared by the effect below. */
  let shownGrow: boolean | null = null;

  function build(view: EditorView): DecorationSet {
    const text = view.state.doc.toString();
    // One parse, shared by the colours and the hit-test list. Both need it, this runs on every
    // keystroke, and `parseH3Block` is the expensive half of the pass.
    const parsed = parseH3Block(text);
    // Published for the status line and the hover hit-test, from the same read of the document that
    // produced the colours — so the three can never disagree about which document they describe.
    docTokens = h3Tokens(text, parsed);
    // `h3Decorations` owns the range rules (sorted, non-empty, non-overlapping, in bounds) and its
    // tests own the proof — a violation of any of them throws, on a keystroke.
    const caretAt = view.hasFocus ? view.state.selection.main.head : null;
    const ranges: Range<Decoration>[] = h3Decorations(text, caretAt, parsed, resolvedLabels).map(
      (d) => markFor(d.cls).range(d.from, d.to)
    );
    for (const b of h3SectionBadges(text, parsed)) {
      const size = b.empty ? 'not written' : `${b.chars.toLocaleString()} ch`;
      ranges.push(badgeFor(`${SECTION_GLOSS[b.name]} · ${size}`, b.empty).range(b.at));
    }
    // The search pass, on top of the syntax colours rather than instead of them — mark decorations
    // are allowed to overlap, and a hit that swallowed the `<Subject 1>` under it would hide which
    // construct is about to be rewritten. Measured against THIS document, like everything else here,
    // so a query arriving one keystroke early cannot produce an out-of-range mark.
    const find = search;
    if (find?.query) {
      const hits = docMatches({ id: docId, text, window: find.window }, find.query, {
        caseSensitive: find.caseSensitive,
        wholeWord: find.wholeWord
      });
      for (const m of hits) {
        const live = find.active && m.from === find.active.from && m.to === find.active.to;
        ranges.push(markFor(live ? 'h3-find h3-find-on' : 'h3-find').range(m.from, m.to));
      }
    }
    // 🔴 `Decoration.set(…, true)` rather than a `RangeSetBuilder`: the badges are POINT decorations
    // that land inside header ranges the marks already cover, so the two streams interleave and no
    // single pass produces them in the ascending order a builder demands. Sorting is CodeMirror's
    // job here — it knows how a widget's side orders against a mark starting at the same offset.
    return Decoration.set(ranges, true);
  }

  /**
   * 🔴 A `ViewPlugin` rather than a `StateField`, because focus is one of the three inputs.
   *
   * Focus is a property of the VIEW, not of the document, so a state field would need it pushed in
   * as an effect — and the only place to notice it changing is the update listener, which is
   * explicitly not allowed to dispatch back into the view it is reporting on. A view plugin simply
   * reads `view.hasFocus` at the moment it rebuilds, and `focusChanged` is already one of the flags
   * on the update it is handed.
   *
   * The selection is a dependency for the same reason the document is: moving the caret across a
   * section boundary is what changes which half of the block is dimmed.
   */
  const highlighter = ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = build(view);
      }
      update(u: ViewUpdate) {
        // 🔴 `repaint` is the fourth input and it is not derivable from the update. Taking a
        // reference out of scope changes which labels resolve WITHOUT touching the document, the
        // selection or focus — so without an explicit signal the block would keep painting a label
        // as live until the next keystroke happened to rebuild it.
        const forced = u.transactions.some((tr) => tr.effects.some((e) => e.is(repaint)));
        if (u.docChanged || u.selectionSet || u.focusChanged || forced)
          this.decorations = build(u.view);
      }
    },
    { decorations: (v) => v.decorations }
  );

  // ── The view ────────────────────────────────────────────────────────────────

  let host = $state<HTMLDivElement | null>(null);
  let view: EditorView | null = null;

  /**
   * The look. Declared as a CodeMirror theme rather than in this component's own scoped stylesheet,
   * because the DOM is CodeMirror's — Svelte's scoping attribute is never applied to it, so a scoped
   * rule would simply not match. The `var(--…)` references resolve against the app's own cascade at
   * the element, which is inside `.dw`.
   *
   * ⚠️ Do not write a literal style tag in a comment in this block: Svelte's parser reads it as an
   * element opening inside the script and reports the script as never closed.
   */
  const theme = EditorView.theme({
    '&': {
      fontFamily: 'var(--mono)',
      fontSize: '11px',
      color: 'var(--ink)',
      backgroundColor: 'var(--bg-3, var(--bg-2))',
      height: '100%'
    },
    '&.cm-focused': { outline: 'none' },
    '.cm-scroller': { fontFamily: 'var(--mono)', lineHeight: '1.55', overflow: 'auto' },
    '.cm-content': { padding: '8px 10px', caretColor: 'var(--accent)' },
    '.cm-line': { padding: '0' },
    '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--accent)', borderLeftWidth: '2px' },
    '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': {
      backgroundColor: 'color-mix(in srgb, var(--accent) 30%, transparent)'
    },
    '.cm-placeholder': { color: 'var(--muted)' },

    // ── The palette. Four colours and a weight: this is a schema with five constructs in it, and a
    //    rainbow would be as unreadable as the wall of grey it replaces. Prose keeps the ordinary
    //    ink colour, because it is the writing and it is what the author is reading.
    '.h3-header': { color: 'var(--accent)', fontWeight: '700' },
    '.h3-foreign': { color: 'var(--warning)', fontWeight: '700' },
    // 🔴 Two channels in ONE hue, told apart by form rather than by a second colour. What RENDERS —
    // the header, the `[Shot N]` tag, the transition verb — is accent TEXT. What the model is
    // POINTED AT — every `<Label N>` — is accent on a filled chip. The labels were `#c792ea`, a
    // purple belonging to no token in the workspace, which made the one construct the author has to
    // scan for the one construct that did not look like the app.
    '.h3-shot': { color: 'var(--accent)', fontWeight: '700' },
    '.h3-time': { color: 'var(--success, #6bbf6b)', fontWeight: '600' },
    '.h3-label': {
      color: 'var(--accent)',
      fontWeight: '600',
      backgroundColor: 'color-mix(in srgb, var(--accent) 16%, transparent)',
      borderRadius: '3px',
      padding: '0 2px'
    },
    // A label naming nothing. It is the DESIGNED outcome of taking a reference out of scope, not a
    // rare mistake, so it is marked where the author is already looking rather than only in a card
    // they would have to go and open.
    '.h3-dead': {
      color: 'var(--danger)',
      textDecoration: 'underline wavy var(--danger)',
      textUnderlineOffset: '3px'
    },
    '.h3-marker, .h3-tasktypes': { color: 'var(--warning)' },
    '.h3-scenetrans': { color: 'var(--muted)', fontStyle: 'italic' },
    // Everything outside the section the caret is in. Legible, but plainly not where you are.
    '.h3-dim': { opacity: '0.42' },
    // The badge on a header line. Set in the UI face rather than the block's mono, and a size down,
    // so it reads as something ABOUT the line rather than as more of the line.
    '.h3-badge': {
      fontFamily: 'var(--sans, system-ui)',
      fontSize: '9.5px',
      fontWeight: '500',
      letterSpacing: '0.09em',
      textTransform: 'uppercase',
      color: 'var(--muted)',
      marginLeft: '0.6em',
      userSelect: 'none',
      pointerEvents: 'none'
    },
    // An unwritten section is the normal look of a fresh shot, so it is set apart by style rather
    // than by a colour that would read as a fault.
    '.h3-badge-empty': { fontStyle: 'italic', opacity: '0.75' },
    // ── Find and replace. Warning-amber, which is the one hue in this palette that belongs to no
    //    construct in the schema: a hit is the only mark here that is about the AUTHOR's question
    //    rather than about what the model reads, so it must not be mistakable for either.
    '.h3-find': {
      backgroundColor: 'color-mix(in srgb, var(--warning) 26%, transparent)',
      borderRadius: '2px'
    },
    // The one the buttons act on. Filled and outlined rather than a second colour — "this is the one
    // Replace will rewrite" is a question of WHICH, and an outline answers which.
    '.h3-find-on': {
      backgroundColor: 'color-mix(in srgb, var(--warning) 62%, transparent)',
      outline: '1px solid var(--warning)',
      color: 'var(--ink)'
    },
    // The tokens that answer on hover — the pointer is the only affordance a decoration has.
    '.h3-shot:hover, .h3-time:hover, .h3-label:hover, .h3-header:hover, .h3-foreign:hover, .h3-marker:hover':
      { textDecoration: 'underline dotted', textUnderlineOffset: '3px' }
  });

  /**
   * Height under {@link grow}: the content decides, and the scroller stops being one.
   *
   * ⚠️ `overflow: visible` on `.cm-scroller` is the load-bearing half. Without it the scroller keeps
   * its own overflow context and swallows a touch drag even when it has nothing left to scroll.
   */
  const growTheme = EditorView.theme({
    '&': { height: 'auto' },
    '.cm-scroller': { overflow: 'visible' }
  });

  /**
   * Build the view, ONCE.
   *
   * 🔴 **`host` is the ONLY tracked dependency, and everything else is read inside `untrack`.** A
   * `$effect` subscribes to every signal it reads, and re-running this one tears down the view and
   * builds a new one — losing focus, caret, undo history and scroll position.
   *
   * ⚠️ **A prop that looks constant is not necessarily a constant signal, and that is the trap this
   * hit twice.** `value` is the obvious one. The subtle one was `docId`: it is a stable STRING, but
   * it is read off `gen.doc`, and the packer derives a fresh `doc` OBJECT on every keystroke — so the
   * signal behind the prop invalidates even though the value never changes, and Svelte re-runs the
   * effect on identity rather than on equality. Tracking it here rebuilt the editor on every
   * character typed, which is what "the prompt scrolls to the top as I type" was. Switching document
   * is handled in the sync effect below, by value, where a same-string re-render is free.
   */
  $effect(() => {
    const el = host;
    if (!el) return;
    const v = untrack(
      () =>
        new EditorView({
          parent: el,
          doc: value,
          extensions: [
            // In a compartment so switching render unit can RESET it — see the sync effect.
            historyBox.of(history()),
            heightBox.of(grow ? growTheme : []),
            keymap.of([...defaultKeymap, ...historyKeymap]),
            EditorView.lineWrapping,
            highlighter,
            theme,
            cmPlaceholder(placeholder),
            EditorView.contentAttributes.of({ 'aria-label': 'Prompt block', spellcheck: 'false' }),
            EditorView.updateListener.of((u) => {
              if (u.docChanged) {
                // 🔴 An echo of an EXTERNAL rewrite is not an author edit, and reporting it as one
                // has a visible cost: `oninput` marks sections touched, which is what un-gates the
                // amber notes. Without this, drafting a prompt or editing a beat body in the panels
                // below would light up warnings about text the author has not looked at yet — the
                // exact behaviour the touched-gating exists to prevent.
                if (!u.transactions.some((t) => t.annotation(fromParent))) {
                  oninput(u.state.doc.toString());
                }
                sections = h3SectionSpans(parseH3Block(u.state.doc.toString()));
              }
              if (u.docChanged || u.selectionSet) {
                caret = u.state.selection.main.head;
                caretLine = u.state.doc.lineAt(caret).number;
                if (coarse) reportCaretToken(caret);
              }
              // ⚠️ Focus is reported here rather than through `domEventHandlers`: `focus`/`blur` do
              // not bubble, and this is the flag CodeMirror already computes. Nothing is dispatched
              // back into the view from inside its own update — the highlighter reads `hasFocus`
              // itself, which is why it is a view plugin.
              if (u.focusChanged) {
                focused = u.view.hasFocus;
                if (u.view.hasFocus) onclaim?.({ insert });
                else onblur?.();
              }
            }),
            EditorView.domEventHandlers({
              // 🔴 The pointer that is actually in use decides which report runs — not a media
              // query, because a hybrid machine is both and a phone with a mouse attached is one.
              // ⚠️ `touch` alone, not `!== 'mouse'`. A desktop stylus reports `pen`, hovers
              // exactly like a mouse, and latched this to coarse for the rest of the session —
              // killing the hover card until a mouse pointerdown happened to land in the editor.
              pointerdown: (e) => {
                const wasCoarse = coarse;
                coarse = e.pointerType === 'touch';
                if (coarse && !wasCoarse) clearHover();
                return false;
              },
              mousemove: (e) => {
                if (!coarse) onMove(e);
                return false;
              },
              mouseleave: () => {
                if (!coarse) clearHover();
                return false;
              }
            })
          ]
        })
    );
    view = v;
    untrack(() => {
      shownDocId = docId;
      // Recorded here so the reconfigure effect below is a no-op on mount rather than dispatching a
      // transaction that changes nothing.
      shownGrow = grow;
      sections = h3SectionSpans(parseH3Block(value));
    });
    return () => {
      v.destroy();
      view = null;
    };
  });

  /**
   * Take an external rewrite of the block — an LLM draft, a field edit in the panels below, or a
   * switch to a different render unit entirely.
   *
   * The common case is a no-op: the author's own keystroke has already gone out through `oninput`
   * and come back identical, so the guard catches it and the selection is never touched. Both
   * dependencies are re-read on every keystroke (the packer hands down fresh objects), which is
   * exactly why the comparisons here are by VALUE — the work is a string compare, not a rebuild.
   *
   * 🔴 **Switching document resets the undo history, and that is a corruption guard.** CodeMirror's
   * history knows nothing about which prompt it belongs to, so without the reset one Ctrl+Z after
   * moving to another render unit would pull the PREVIOUS one's block into this one — and the
   * resulting `oninput` would store it. Reconfiguring the compartment is what clears it; the caret
   * goes to the top because it is a different document, not a moved cursor.
   */
  $effect(() => {
    const next = value;
    const id = docId;
    const v = view;
    if (!v) return;
    const switched = untrack(() => shownDocId) !== id;
    if (!switched && v.state.doc.toString() === next) return;
    untrack(() => (shownDocId = id));
    v.dispatch({
      changes: { from: 0, to: v.state.doc.length, insert: next },
      annotations: fromParent.of(true),
      ...(switched ? { selection: { anchor: 0 }, effects: historyBox.reconfigure(history()) } : {})
    });
  });

  /**
   * Repaint when the set of labels that resolve changes under a document that did not.
   *
   * Scoping a reference in or out rewrites no bytes, so nothing the highlighter watches has moved —
   * but every `<Picture N>` after it now means something different.
   *
   * ⚠️ Compared by VALUE, like the `value`/`docId` effect above and for the same reason: the
   * composer derives a fresh Set on every keystroke, so tracking identity would dispatch a
   * transaction per character and rebuild the decorations twice for each one.
   */
  let shownLabelKey: string | null = null;
  $effect(() => {
    const labels = resolvedLabels;
    const v = view;
    if (!v || !labels) return;
    const key = [...labels].sort().join('|');
    if (untrack(() => shownLabelKey) === key) return;
    untrack(() => (shownLabelKey = key));
    v.dispatch({ effects: repaint.of(null) });
  });

  /** Flip the height rules if `grow` changes under a live view. Compared by value; usually a no-op. */
  $effect(() => {
    const g = grow;
    const v = view;
    if (!v || untrack(() => shownGrow) === g) return;
    untrack(() => (shownGrow = g));
    v.dispatch({ effects: heightBox.reconfigure(g ? growTheme : []) });
  });

  /**
   * Repaint when the search moves, and bring the active hit on screen.
   *
   * ⚠️ Compared by VALUE for the same reason `resolvedLabels` is: the screen above derives a fresh
   * `search` object on every keystroke in every block, so tracking identity would dispatch a
   * transaction into all fourteen editors per character typed into one of them.
   *
   * 🔴 The scroll is clamped against the CURRENT document length. The parent's `value` effect and
   * this one are separate effects with no ordering guarantee, so a replacement that lengthens a
   * block can arrive here before the text does — and `scrollIntoView` past the end of the document
   * throws inside CodeMirror rather than scrolling to the bottom.
   */
  let shownSearchKey: string | null = null;
  $effect(() => {
    const find = search;
    const v = view;
    if (!v) return;
    const key = find
      ? [
          find.query,
          find.caseSensitive,
          find.wholeWord,
          find.window?.from ?? -1,
          find.window?.to ?? -1,
          find.active?.from ?? -1,
          find.active?.to ?? -1
        ].join('|')
      : '';
    if (untrack(() => shownSearchKey) === key) return;
    untrack(() => (shownSearchKey = key));
    const at = find?.active?.from;
    const scrollable = at !== undefined && at <= v.state.doc.length;
    v.dispatch({
      effects: [
        repaint.of(null),
        ...(scrollable ? [EditorView.scrollIntoView(at, { y: 'center' })] : [])
      ]
    });
  });

  /**
   * Write a label at the caret. The composer's reference pills call this through {@link onclaim}.
   *
   * Spaces only where one is missing: `<Picture 1>` fused to the previous word is a label the
   * conditioner does not match, and a space before a full stop is just untidy.
   */
  function insert(label: string) {
    if (!view) return;
    const { from, to } = view.state.selection.main;
    const before = view.state.doc.sliceString(Math.max(0, from - 1), from);
    const after = view.state.doc.sliceString(to, Math.min(view.state.doc.length, to + 1));
    const lead = before && !/\s/.test(before) ? ' ' : '';
    const trail = after && !/^[\s.,;:!?)]/.test(after) ? ' ' : '';
    view.dispatch({
      changes: { from, to, insert: lead + label + trail },
      selection: { anchor: from + lead.length + label.length }
    });
    view.focus();
  }

  /** Put the caret at a section's body and scroll it into view. */
  function jumpTo(pos: number) {
    if (!view) return;
    view.focus();
    view.dispatch({
      selection: { anchor: pos },
      effects: EditorView.scrollIntoView(pos, { y: 'center' })
    });
  }

  // ── The hover card ──────────────────────────────────────────────────────────
  let hover = $state<{ card: H3TokenCard; x: number; y: number } | null>(null);
  let hovered: H3Token | null = null;
  /**
   * Whether the last pointer to touch this editor was a finger.
   *
   * 🔴 Touch has no hover, but it does synthesise ONE `mousemove` per tap — enough for the hover
   * path to fill the card and then LATCH, because `mouseleave` never follows. So on a coarse
   * pointer the token report comes from the CARET instead, which a tap moves anyway, and no card
   * is positioned: `.pe-card` is `position: fixed`, i.e. exactly where a soft keyboard sits.
   */
  let coarse = false;

  function clearHover() {
    if (hovered === null && hover === null) return;
    hovered = null;
    hover = null;
    ontoken?.(null);
  }

  /**
   * Which token is under the pointer.
   *
   * 🔴 `posAtCoords` asks the editor, which is exact — the overlay had to hit-test `elementsFromPoint`
   * against span rectangles, because its spans were in a different element from the pointer events.
   * `precise` (the default) returns `null` off the end of a line rather than the nearest position, so
   * the empty space to the right of a short line answers with no card instead of the last token on it.
   */
  function onMove(e: MouseEvent) {
    if (!view) return;
    const pos = view.posAtCoords({ x: e.clientX, y: e.clientY });
    const token =
      pos === null ? null : (docTokens.find((t) => pos >= t.start && pos < t.end) ?? null);
    if (token === hovered) return;
    hovered = token;
    ontoken?.(token);
    const card = token ? (onlookup?.(token) ?? null) : null;
    if (!token || !card) {
      hover = null;
      return;
    }
    // Anchored to the token's own start, not to the pointer, so the card does not slide while the
    // cursor moves inside one label.
    const at = view.coordsAtPos(token.start);
    hover = at ? { card, x: at.left, y: at.top } : null;
  }

  /** The touch equivalent of {@link onMove}: the token the caret is standing in, reported bare. */
  function reportCaretToken(pos: number) {
    const token = docTokens.find((t) => pos >= t.start && pos < t.end) ?? null;
    if (token === hovered) return;
    hovered = token;
    hover = null;
    ontoken?.(token);
  }
</script>

<div class="pe-wrap">
  <!-- The section strip: what the block contains, which one the caret is in, and a click to get
       there. Six sections deep with no landmarks is the reason "where am I" is a question at all. -->
  {#if sections.length}
    <div class="pe-rail">
      {#each sections as s (s.start)}
        <button
          class="pe-chip"
          class:on={activeSection?.start === s.start}
          title="Jump to {sectionLabel(s.name)}"
          onclick={() => jumpTo(s.bodyStart)}>{sectionLabel(s.name)}</button
        >
      {/each}
    </div>
  {/if}

  <div
    class="pe"
    class:focused
    class:grow
    style="--pe-rows:{rows}; --pe-min:{minHeight}px"
    bind:this={host}
  ></div>

  <div class="pe-status">
    <span class="pe-where"
      >{#if activeSection}<b>{sectionLabel(activeSection.name)}</b>{#if activeShot}
          · <b>[Shot {activeShot}]</b>{/if}{:else}<i>between sections</i>{/if}</span
    >
    <span>line {caretLine}</span>
    <span class="pe-grow"></span>
    <span class="pe-key"><i class="sw shot"></i>shot</span>
    <span class="pe-key"><i class="sw time"></i>time</span>
    <span class="pe-key"><i class="sw label"></i>reference</span>
    <span class="pe-key"><i class="sw header"></i>section</span>
  </div>
</div>

{#if hover}
  <!-- Fixed, and clamped left only: the card is anchored to a token that may be at either edge of a
       wrapped line, and a card that runs off the right of the viewport is a card nobody reads. -->
  <div
    class="pe-card"
    style="left:{Math.max(
      8,
      Math.min(hover.x, (typeof window !== 'undefined' ? window.innerWidth : 1200) - 250)
    )}px; top:{hover.y}px"
  >
    {#if hover.card.image}
      <img src={hover.card.image} alt={hover.card.title} />
    {/if}
    <b>{hover.card.title}</b>
    {#if hover.card.meta}<small>{hover.card.meta}</small>{/if}
    {#if hover.card.body}<p>{hover.card.body}</p>{/if}
    {#if hover.card.warn}<small class="warn">⚠ {hover.card.warn}</small>{/if}
  </div>
{/if}

<style>
  .pe-wrap {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  /* ── The section strip ──────────────────────────────────────────────────────────────────
     Reads as a map of the document, not as a toolbar: no borders, no fill, just the names with the
     current one lit. It scrolls rather than wraps, so the strip is always one line tall and the
     editor below it never moves down as sections are added. */
  .pe-rail {
    display: flex;
    gap: 4px;
    overflow-x: auto;
    scrollbar-width: none;
  }
  .pe-rail::-webkit-scrollbar {
    display: none;
  }
  .pe-chip {
    flex: none;
    width: auto;
    padding: 2px 8px;
    border: 1px solid transparent;
    border-radius: 999px;
    background: none;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 10px;
    cursor: pointer;
    white-space: nowrap;
  }
  .pe-chip:hover {
    color: var(--ink);
    border-color: var(--ring);
  }
  .pe-chip.on {
    color: var(--accent);
    border-color: var(--accent);
  }

  /* The frame is ours; everything inside it is CodeMirror's and is styled by the theme above. */
  .pe {
    border: 1px solid var(--ring);
    border-radius: 9px;
    background: var(--bg-3, var(--bg-2));
    font-size: 11px;
    height: calc(var(--pe-rows) * 1.55em + 18px);
    min-height: var(--pe-min, 280px);
    overflow: hidden;
    resize: vertical;
    transition: border-color 0.14s;
  }
  .pe.focused {
    border-color: var(--accent);
  }
  /* The document's own height. `resize` goes with the fixed height it was resizing. */
  .pe.grow {
    height: auto;
    min-height: 0;
    resize: none;
  }

  /* ── The status line ───────────────────────────────────────────────────────────────────── */
  .pe-status {
    display: flex;
    align-items: center;
    gap: 10px;
    font-family: var(--mono);
    font-size: 9.5px;
    color: var(--muted);
  }
  .pe-where b {
    color: var(--accent);
  }
  .pe-grow {
    flex: 1;
  }
  .pe-key {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .sw {
    width: 7px;
    height: 7px;
    border-radius: 2px;
    display: inline-block;
  }
  .sw.shot,
  .sw.header {
    background: var(--accent);
  }
  .sw.time {
    background: var(--success, #6bbf6b);
  }
  .sw.label {
    background: #c792ea;
  }
  .sw.header {
    opacity: 0.55;
  }

  /* ── The hover card ────────────────────────────────────────────────────────────────────
     Same substance as the reference pills' own card — the full picture and what it is FOR —
     because it answers the same question in a different place. */
  .pe-card {
    position: fixed;
    z-index: 60;
    transform: translateY(calc(-100% - 8px));
    display: flex;
    flex-direction: column;
    gap: 4px;
    width: 240px;
    padding: 8px 9px;
    border: 1px solid var(--accent);
    border-radius: 9px;
    background: var(--bg-2);
    box-shadow: 0 8px 22px rgba(0, 0, 0, 0.35);
    pointer-events: none;
  }
  .pe-card img {
    width: 100%;
    border-radius: 6px;
    display: block;
  }
  .pe-card b {
    font-size: 11.5px;
  }
  .pe-card small {
    font-size: 10px;
    color: var(--muted);
    font-family: var(--mono);
  }
  .pe-card small.warn {
    color: var(--warning);
    font-family: inherit;
  }
  .pe-card p {
    margin: 0;
    font-size: 11px;
    line-height: 1.45;
    color: var(--ink);
    display: -webkit-box;
    -webkit-line-clamp: 4;
    line-clamp: 4;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
</style>
