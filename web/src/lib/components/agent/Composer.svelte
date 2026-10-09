<script lang="ts">
  import { onMount } from 'svelte';
  import { audioInputAllowed, getHealth } from '$lib/api/client';
  import AttachmentTray from './AttachmentTray.svelte';
  import {
    clipboardAttachments,
    readAttachments,
    NO_AUDIO_REASON,
    type PendingAttachment
  } from './attachments';

  let {
    running = false,
    cancelling = false,
    sending = false,
    onSend,
    onCancel
  }: {
    running?: boolean;
    cancelling?: boolean;
    sending?: boolean;
    onSend: (content: string, attachments: PendingAttachment[]) => Promise<void>;
    onCancel: () => void;
  } = $props();

  let text = $state('');
  let attachments = $state<PendingAttachment[]>([]);
  let notice = $state('');
  let imageInputEl: HTMLInputElement | undefined = $state();
  let audioInputEl: HTMLInputElement | undefined = $state();
  let dragActive = $state(false);
  let dragDepth = 0;
  /// 🔴 Can the LOADED model read audio? Off `/health`, which DERIVES it from
  /// which llama-server koblem is pointed at — there is no probe, `/v1/models`
  /// says nothing about the mmproj. Optimistic until told otherwise: a `/health`
  /// that blipped must not grey out a feature that works.
  ///
  /// Asked here rather than passed in as a prop because this component is the
  /// only thing in the workspace that stages a file, and it is where the answer
  /// is acted on.
  let audioSupported = $state(true);

  onMount(async () => {
    try {
      audioSupported = audioInputAllowed(await getHealth());
    } catch {
      // Leave it optimistic — see above.
    }
  });

  const empty = $derived(!text.trim() && attachments.length === 0);
  const blocked = $derived(sending || running);

  async function take(files: FileList | File[] | null | undefined) {
    const { accepted, errors } = await readAttachments(files, attachments, audioSupported);
    if (accepted.length > 0) attachments = [...attachments, ...accepted];
    if (errors.length === 0) notice = '';
    else notice = errors.length === 1 ? errors[0] : `${errors[0]} (+${errors.length - 1} more)`;
  }

  function onFileInput(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    void take(target.files);
    target.value = '';
  }

  function onPaste(e: ClipboardEvent) {
    const files = clipboardAttachments(e);
    if (files.length === 0) return;
    e.preventDefault();
    void take(files);
  }

  function removeAttachment(id: string) {
    attachments = attachments.filter((a) => a.id !== id);
  }

  // dragDepth counters avoid the flicker you get when dragging over child
  // elements (each child fires dragenter/leave independently).
  function onDragEnter(e: DragEvent) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    dragDepth += 1;
    dragActive = true;
  }

  function onDragOver(e: DragEvent) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  }

  function onDragLeave(e: DragEvent) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dragActive = false;
  }

  function onDrop(e: DragEvent) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    dragDepth = 0;
    dragActive = false;
    void take(e.dataTransfer.files);
  }

  async function submit() {
    if (empty || blocked) return;
    // 🔴 Re-check at SEND, not just at mount, when audio is actually staged.
    // Beefy can be flipped from another tab between opening this composer and
    // pressing Send, and the agent has no per-message warning channel — the turn
    // would go out mute with nothing on it to say so. Costs one cheap /health,
    // and only on the turns that would be affected.
    if (attachments.some((a) => a.kind === 'audio')) {
      try {
        audioSupported = audioInputAllowed(await getHealth());
      } catch {
        // Optimistic — the server drops what it cannot read either way.
      }
      if (!audioSupported) {
        attachments = attachments.filter((a) => a.kind !== 'audio');
        // Hand the turn back rather than sending a silently-mute version of it.
        // One more click, with the reason on screen, beats an answer about a
        // sound the model never received.
        notice = `Audio removed, nothing was sent — ${NO_AUDIO_REASON}`;
        return;
      }
    }
    try {
      await onSend(text.trim(), attachments);
      text = '';
      attachments = [];
      notice = '';
    } catch {
      // The page owns the error banner; keep the draft so it can be retried.
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void submit();
    }
  }
</script>

<div
  class="composer"
  class:drag-active={dragActive}
  ondragenter={onDragEnter}
  ondragover={onDragOver}
  ondragleave={onDragLeave}
  ondrop={onDrop}
  role="region"
  aria-label={audioSupported
    ? 'Message the agent, drop images or audio to attach'
    : 'Message the agent, drop images to attach'}
>
  {#if dragActive}
    <div class="drop-overlay" aria-hidden="true">
      <svg viewBox="0 0 24 24" width="32" height="32" fill="currentColor">
        <path
          d="M21 19V5c0-1.1-.9-2-2-2H5c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2zM8.5 13.5l2.5 3.01L14.5 12l4.5 6H5l3.5-4.5z"
        ></path>
      </svg>
      <span>{audioSupported ? 'Drop images or audio to attach' : 'Drop images to attach'}</span>
    </div>
  {/if}

  {#if notice}
    <p class="notice">{notice}</p>
  {/if}

  {#if !audioSupported}
    <p class="no-audio">
      ⚠ Audio attachments are unavailable — {NO_AUDIO_REASON}
      <a href="/gpu-services">GPU Services</a>
    </p>
  {/if}

  <AttachmentTray {attachments} onRemove={removeAttachment} />

  <textarea
    class="prompt"
    bind:value={text}
    rows="3"
    placeholder="Ask for anything — an image, a voice line, a video, a look at a picture… (Ctrl+Enter to send, or paste a file)"
    onkeydown={onKeydown}
    onpaste={onPaste}></textarea>

  <input
    bind:this={imageInputEl}
    class="file-input-hidden"
    type="file"
    accept="image/*"
    multiple
    onchange={onFileInput}
  />
  <input
    bind:this={audioInputEl}
    class="file-input-hidden"
    type="file"
    accept="audio/*"
    multiple
    onchange={onFileInput}
  />

  <div class="composer-actions">
    <button class="link-btn" type="button" onclick={() => imageInputEl?.click()}
      >Attach Image</button
    >
    <button
      class="link-btn"
      type="button"
      disabled={!audioSupported}
      title={audioSupported ? 'Attach an audio file' : `Unavailable: ${NO_AUDIO_REASON}`}
      onclick={() => audioInputEl?.click()}>Attach Audio</button
    >
    <span class="spacer"></span>
    {#if running}
      <button class="btn-danger-outline" type="button" onclick={onCancel} disabled={cancelling}>
        {cancelling ? 'Cancelling…' : 'Cancel turn'}
      </button>
    {/if}
    <button class="btn-primary send-btn" type="button" onclick={submit} disabled={empty || blocked}>
      {#if sending}
        <span class="spinner"></span>
        Sending…
      {:else if running}
        Working…
      {:else}
        Send
      {/if}
    </button>
  </div>
</div>

<style>
  .composer {
    position: relative;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 14px;
    transition: border-color 0.15s;
  }

  .composer.drag-active {
    border-color: var(--accent);
  }

  .drop-overlay {
    position: absolute;
    inset: 0;
    border-radius: 16px;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(2px);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    color: #fff;
    font-size: 14px;
    font-weight: 600;
    pointer-events: none;
    z-index: 2;
  }

  :global([data-theme='light']) .drop-overlay {
    background: rgba(0, 0, 0, 0.35);
  }

  .notice {
    margin: 0 0 8px;
    font-size: 12px;
    color: var(--warning);
  }

  .prompt {
    width: 100%;
    padding: 12px 14px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: transparent;
    color: var(--ink);
    font-size: 14px;
    line-height: 1.5;
    resize: vertical;
    font-family: inherit;
  }

  .prompt:focus {
    outline: none;
    border-color: var(--accent);
  }

  .file-input-hidden {
    display: none;
  }

  .composer-actions {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 12px;
    flex-wrap: wrap;
  }

  .spacer {
    flex: 1;
  }

  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    padding: 0;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .link-btn:hover {
    color: var(--accent);
  }

  .link-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .link-btn:disabled:hover {
    color: var(--muted);
  }

  .no-audio {
    margin: 0 0 8px;
    padding: 8px 10px;
    border: 1px solid var(--warning);
    border-radius: 10px;
    background: color-mix(in srgb, var(--warning) 10%, transparent);
    color: var(--muted);
    font-size: 12px;
    line-height: 1.5;
  }

  .no-audio a {
    color: var(--warning);
  }

  .send-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 24px;
  }

  .send-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .spinner {
    display: inline-block;
    width: 14px;
    height: 14px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.75s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
