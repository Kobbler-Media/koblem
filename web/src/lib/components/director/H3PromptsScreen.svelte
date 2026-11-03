<script lang="ts">
  import H3PromptsPanel from './H3PromptsPanel.svelte';
  import { DEFAULT_VOCAB, type EngineVocab } from '#lib/director/engine.ts';

  /**
   * The desktop's **Prompts screen** — `H3PromptsPanel` in a full-sheet modal.
   *
   * Chrome only: the header, the close button and the size. Everything that DOES anything is in the
   * panel, because the phone pushes that same panel as a `bare` sheet with the shell's own back
   * button — two surfaces, one implementation of what Replace all means.
   *
   * 🔴 A screen rather than another tab in the rail-panel modal (`expandPanel`). Those four are one
   * generation's fields blown up to a roomy size at 640px; this is the whole project's text, and a
   * block narrower than the composer already shows it would be a worse place to read a prompt than
   * the place this exists to replace.
   */
  let {
    docs,
    vocab = DEFAULT_VOCAB,
    onprompt,
    oncapture,
    ongoto,
    onclose
  }: {
    docs: {
      id: string;
      no: number;
      gist: string;
      text: string;
      written: boolean;
    }[];
    vocab?: EngineVocab;
    onprompt: (id: string, block: string) => void;
    oncapture: (label: string) => void;
    ongoto: (id: string) => void;
    onclose: () => void;
  } = $props();
</script>

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="modal prompts-modal">
    <div class="chat-h">
      <div class="chat-badge">✎</div>
      <div style="flex:1">
        <b>Prompts</b><small
          >Every {vocab.renderUnit}'s block, as text — filter, find and replace across all of them</small
        >
      </div>
      <button class="btn icon-only ghost" title="Close" onclick={onclose}>✕</button>
    </div>

    <H3PromptsPanel {docs} {vocab} {onprompt} {oncapture} {ongoto} />
  </div>
</div>

<style>
  /* 🔴 Nearly the whole window, and deliberately with no max width — the rest of the workspace caps
     its modals at a readable measure, and this one must not. A prompt block is long wrapped
     paragraphs of schema-ish prose; every column the sheet gives up is another wrap in every one of
     fourteen boxes, and re-reading a sentence that breaks four times is the thing this screen exists
     to stop. The 2vw gutter is what keeps it reading as a sheet over the workspace rather than as a
     page that replaced it. */
  .prompts-modal {
    width: 96vw;
    height: 94vh;
    height: 94dvh;
  }
  .chat-h small {
    display: block;
    color: var(--muted);
    font-size: 11px;
  }
</style>
