<script lang="ts">
  import type { ChatEdit, ChatMsg } from '#lib/director/chat.ts';

  type Props = {
    messages: ChatMsg[];
    busy: boolean;
    onSend: (text: string) => void;
    onClear: () => void;
    onClose: () => void;
    onRevertEdit: (edit: ChatEdit) => void;
  };

  let { messages, busy, onSend, onClear, onClose, onRevertEdit }: Props = $props();

  let draft = $state('');
  let listEl: HTMLDivElement | undefined = $state();

  $effect(() => {
    void [messages.length, busy];
    if (listEl) listEl.scrollTop = listEl.scrollHeight;
  });

  function send() {
    if (busy) return;
    const text = draft.trim();
    if (!text) return;
    onSend(text);
    draft = '';
  }

  function onTextareaKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  function onOverlayKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onClose();
  }
  function onOverlayClick(e: MouseEvent) {
    if (e.target === e.currentTarget) onClose();
  }
</script>

<div
  class="overlay"
  role="button"
  tabindex="-1"
  onclick={onOverlayClick}
  onkeydown={onOverlayKeydown}
>
  <div class="panel" role="dialog" aria-modal="true" aria-label="Director chat">
    <header class="head">
      <div class="brand">
        <span class="badge" aria-hidden="true">💬</span>
        <div class="titles">
          <div class="title">Director chat</div>
          <div class="subtitle">Ask about your shots, or tell me to edit them</div>
        </div>
      </div>
      <div class="head-actions">
        <button class="ghost" onclick={onClear} disabled={messages.length === 0}>Clear</button>
        <button class="ghost icon" onclick={onClose} aria-label="Close">✕</button>
      </div>
    </header>

    <div class="list" bind:this={listEl}>
      {#if messages.length === 0}
        <div class="empty">
          <div class="empty-icon" aria-hidden="true">🎬</div>
          <p>Ask about your shots or story — or just tell me what to change.</p>
          <p class="empty-eg">
            “make shot 3 scarier”, “what's the pacing like?”, “add an establishing shot at the
            start”
          </p>
        </div>
      {:else}
        {#each messages as message, mi (mi)}
          <div class="row {message.role}">
            <div class="bubble {message.role}">{message.content}</div>
            {#if message.role === 'assistant' && message.edits?.length}
              <div class="edits">
                {#each message.edits as edit (edit.id)}
                  <div class="edit-card {edit.kind}" class:reverted={edit.reverted}>
                    <div class="edit-head">
                      <span class="edit-label">{edit.label}</span>
                      {#if edit.reverted}
                        <span class="edit-reverted">Reverted</span>
                      {:else}
                        <button class="edit-revert" onclick={() => onRevertEdit(edit)}>
                          ↩ Revert
                        </button>
                      {/if}
                    </div>
                    {#if edit.kind === 'add'}
                      <div class="edit-block add">
                        <span class="tag">Added</span>
                        <p class="edit-text">{edit.after}</p>
                      </div>
                    {:else if edit.kind === 'delete'}
                      <div class="edit-block del">
                        <span class="tag">Removed</span>
                        <p class="edit-text">{edit.before}</p>
                      </div>
                    {:else}
                      <div class="edit-block del">
                        <span class="tag">Before</span>
                        <p class="edit-text">{edit.before || '(empty)'}</p>
                      </div>
                      <div class="edit-block add">
                        <span class="tag">After</span>
                        <p class="edit-text">{edit.after}</p>
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            {/if}
          </div>
        {/each}
      {/if}

      {#if busy}
        <div class="thinking"><span></span><span></span><span></span></div>
      {/if}
    </div>

    <div class="composer">
      <textarea
        class="input"
        rows="2"
        placeholder="Message the director…  (Enter to send, Shift+Enter for a new line)"
        bind:value={draft}
        onkeydown={onTextareaKeydown}
        disabled={busy}></textarea>
      <button class="send" onclick={send} disabled={busy || !draft.trim()} aria-label="Send">
        ➤
      </button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(3px);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: 3vh 2vw;
  }
  .panel {
    background: var(--bg-1, #16181c);
    border: 1px solid var(--ring, #333);
    border-radius: 16px;
    width: min(94vw, 880px);
    height: min(88vh, 880px);
    /* dvh tracks the live viewport so a retracting mobile URL bar can't crop the composer */
    height: min(88dvh, 880px);
    display: flex;
    flex-direction: column;
    color: var(--ink, #e8e8e8);
    overflow: hidden;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.5);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 14px 16px;
    border-bottom: 1px solid var(--ring, #333);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 11px;
    min-width: 0;
  }
  .badge {
    width: 34px;
    height: 34px;
    display: grid;
    place-items: center;
    border-radius: 10px;
    background: color-mix(in srgb, var(--accent, #6ea8fe) 18%, transparent);
    font-size: 17px;
  }
  .title {
    font-size: 15px;
    font-weight: 650;
    line-height: 1.1;
  }
  .subtitle {
    font-size: 12px;
    color: var(--muted, #9da6b0);
    margin-top: 2px;
  }
  .head-actions {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .ghost {
    background: none;
    border: 1px solid transparent;
    color: var(--muted, #9da6b0);
    border-radius: 8px;
    padding: 6px 10px;
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }
  .ghost:hover:not(:disabled) {
    color: var(--ink, #e8e8e8);
    border-color: var(--ring, #333);
  }
  .ghost:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .ghost.icon {
    padding: 6px 9px;
  }

  .list {
    flex: 1;
    overflow-y: auto;
    padding: 18px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .empty {
    margin: auto;
    text-align: center;
    color: var(--muted, #9da6b0);
    max-width: 440px;
  }
  .empty-icon {
    font-size: 34px;
    margin-bottom: 8px;
    opacity: 0.9;
  }
  .empty p {
    margin: 0 0 6px;
    font-size: 14px;
    line-height: 1.5;
  }
  .empty-eg {
    font-size: 12.5px;
    opacity: 0.8;
    font-style: italic;
  }

  .row {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-width: 100%;
  }
  .row.user {
    align-items: flex-end;
  }
  .row.assistant {
    align-items: flex-start;
  }
  .bubble {
    border-radius: 12px;
    padding: 9px 13px;
    font-size: 13.5px;
    line-height: 1.55;
    max-width: 84%;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .bubble.user {
    background: color-mix(in srgb, var(--accent, #6ea8fe) 16%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent, #6ea8fe) 26%, transparent);
  }
  .bubble.assistant {
    background: var(--bg-2, #1e2126);
    border: 1px solid var(--ring, #333);
  }

  .edits {
    display: flex;
    flex-direction: column;
    gap: 10px;
    width: 84%;
    max-width: 84%;
  }
  .edit-card {
    border: 1px solid var(--ring, #333);
    border-radius: 12px;
    background: var(--bg-2, #1e2126);
    overflow: hidden;
  }
  .edit-card.reverted {
    opacity: 0.55;
  }
  .edit-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--ring, #333);
  }
  .edit-label {
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.02em;
  }
  .edit-card.reverted .edit-label {
    text-decoration: line-through;
    color: var(--muted, #9da6b0);
  }
  .edit-revert {
    background: none;
    border: 1px solid var(--ring, #333);
    color: var(--muted, #9da6b0);
    border-radius: 7px;
    padding: 3px 9px;
    font: inherit;
    font-size: 11px;
    cursor: pointer;
    white-space: nowrap;
  }
  .edit-revert:hover {
    color: var(--ink, #e8e8e8);
    border-color: var(--accent, #6ea8fe);
  }
  .edit-reverted {
    font-size: 11px;
    color: var(--muted, #9da6b0);
  }
  .edit-block {
    padding: 8px 12px 8px 14px;
    border-left: 3px solid var(--ring, #333);
  }
  .edit-block + .edit-block {
    border-top: 1px solid var(--ring, #333);
  }
  .edit-block.del {
    border-left-color: color-mix(in srgb, var(--danger, #ef4444) 70%, transparent);
    background: color-mix(in srgb, var(--danger, #ef4444) 6%, transparent);
  }
  .edit-block.add {
    border-left-color: color-mix(in srgb, #3ba55d 75%, transparent);
    background: color-mix(in srgb, #3ba55d 7%, transparent);
  }
  .tag {
    display: block;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted, #9da6b0);
    margin-bottom: 3px;
  }
  .edit-text {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 180px;
    overflow-y: auto;
  }

  .thinking {
    display: flex;
    gap: 5px;
    padding: 4px 2px;
  }
  .thinking span {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--muted, #9da6b0);
    animation: blink 1.2s infinite ease-in-out both;
  }
  .thinking span:nth-child(2) {
    animation-delay: 0.2s;
  }
  .thinking span:nth-child(3) {
    animation-delay: 0.4s;
  }
  @keyframes blink {
    0%,
    80%,
    100% {
      opacity: 0.25;
    }
    40% {
      opacity: 1;
    }
  }

  .composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    padding: 12px 14px;
    border-top: 1px solid var(--ring, #333);
  }
  .input {
    flex: 1;
    resize: none;
    background: var(--bg-2, #1e2126);
    border: 1px solid var(--ring, #333);
    border-radius: 12px;
    color: var(--ink, #e8e8e8);
    padding: 10px 12px;
    font: inherit;
    font-size: 13.5px;
    line-height: 1.5;
    max-height: 160px;
  }
  .input:focus {
    outline: none;
    border-color: var(--accent, #6ea8fe);
  }
  .input:disabled {
    opacity: 0.6;
  }
  .send {
    width: 40px;
    height: 40px;
    flex: none;
    border-radius: 11px;
    border: 1px solid color-mix(in srgb, var(--accent, #6ea8fe) 45%, transparent);
    background: color-mix(in srgb, var(--accent, #6ea8fe) 18%, transparent);
    color: var(--accent, #6ea8fe);
    font-size: 15px;
    cursor: pointer;
    display: grid;
    place-items: center;
  }
  .send:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent, #6ea8fe) 30%, transparent);
  }
  .send:disabled {
    opacity: 0.4;
    cursor: default;
  }
</style>
