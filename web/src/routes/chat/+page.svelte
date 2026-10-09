<script lang="ts">
  import { onMount } from 'svelte';
  import { audioInputAllowed, getHealth, sendChat } from '$lib/api/client';
  import type { ChatResponse, SamplingConfig, SamplingPreset } from '$lib/types';
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import SamplingControls, {
    fromSamplingConfig,
    loadSamplingFromStorage,
    saveSamplingToStorage,
    toSamplingConfig,
    type SamplingState
  } from '$lib/components/SamplingControls.svelte';

  interface Exchange {
    prompt: string;
    system?: string;
    response: string;
    images: string[];
    audio: string[];
    /** Non-fatal things the server did to this turn — today: dropped audio. */
    warnings: string[];
    prompt_tokens?: number;
    completion_tokens?: number;
    duration_ms?: number;
    preset?: SamplingPreset;
  }

  interface PendingImage {
    id: string;
    name: string;
    size: number;
    dataUrl: string;
  }

  interface PendingAudio {
    id: string;
    name: string;
    size: number;
    dataUrl: string;
  }

  // Per-request caps so we stay under the 40 MiB server limit even after
  // base64 inflates the raw bytes by ~1.33x.
  const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
  const MAX_AUDIO_BYTES = 25 * 1024 * 1024;

  /// One sentence, used by the picker tooltip, the drop/paste rejection and the
  /// notice under the prompt, so the three never drift apart.
  const NO_AUDIO_REASON =
    'the loaded model has no audio encoder (Beefy LLM runs Qwen3.8, whose mmproj is vision-only). Images still work.';

  const DEFAULT_SYSTEM_PROMPT = `You are a helpful, friendly assistant. Answer clearly and concisely. If a question is ambiguous, ask for clarification. When you don't know something, say so instead of guessing.`;

  const PROMPT_KEY = 'koblem-chat-prompt';
  const SYSTEM_KEY = 'koblem-chat-system';
  const SAMPLING_KEY = 'koblem-chat-sampling';

  function todayIso(): string {
    const d = new Date();
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, '0');
    const day = String(d.getDate()).padStart(2, '0');
    return `${y}-${m}-${day}`;
  }

  function loadStored(key: string, fallback: string): string {
    if (typeof localStorage === 'undefined') return fallback;
    const v = localStorage.getItem(key);
    return v === null ? fallback : v;
  }

  let systemPrompt = $state(loadStored(SYSTEM_KEY, DEFAULT_SYSTEM_PROMPT));
  let showSystem = $state(true);
  let prompt = $state(loadStored(PROMPT_KEY, ''));
  let sampling: SamplingState = $state(loadSamplingFromStorage(SAMPLING_KEY));
  let sending = $state(false);
  let error = $state('');
  let exchanges = $state<Exchange[]>([]);
  let pendingImages = $state<PendingImage[]>([]);
  let pendingAudio = $state<PendingAudio[]>([]);
  let fileInputEl: HTMLInputElement | undefined = $state();
  let audioInputEl: HTMLInputElement | undefined = $state();
  /// 🔴 Can the LOADED model read audio at all? Off `/health`, which derives it
  /// from which llama-server koblem is pointed at — there is no probe for this,
  /// `/v1/models` reports nothing about the mmproj. Optimistic until told
  /// otherwise: a `/health` that failed must not disable a feature that works.
  ///
  /// This is pre-emption, not enforcement. The server drops audio it cannot read
  /// and says so in `warnings`; this exists so nobody stages 20 MB of wav first.
  let audioSupported = $state(true);
  let dragActive = $state(false);
  let dragDepth = 0;

  $effect(() => {
    if (typeof localStorage !== 'undefined') localStorage.setItem(PROMPT_KEY, prompt);
  });
  $effect(() => {
    if (typeof localStorage !== 'undefined') localStorage.setItem(SYSTEM_KEY, systemPrompt);
  });
  $effect(() => {
    saveSamplingToStorage(SAMPLING_KEY, sampling);
  });

  /// One-shot prefill from sessionStorage. Set by the Review History page's
  /// "Open in Chat" button so the user lands here with the verbatim
  /// system/user from a past review session and its sampling overrides
  /// already applied. The key is consumed immediately so a refresh doesn't
  /// re-prefill over edits the user has since made.
  onMount(() => {
    if (typeof sessionStorage === 'undefined') return;
    const raw = sessionStorage.getItem('koblem-chat-prefill');
    if (!raw) return;
    sessionStorage.removeItem('koblem-chat-prefill');
    try {
      const pf = JSON.parse(raw) as {
        system?: string;
        prompt?: string;
        sampling?: SamplingConfig;
      };
      if (typeof pf.system === 'string') systemPrompt = pf.system;
      if (typeof pf.prompt === 'string') prompt = pf.prompt;
      if (pf.sampling) sampling = fromSamplingConfig(pf.sampling);
      // Expand the system block so the loaded prompt is immediately visible.
      showSystem = true;
    } catch {
      // Ignore malformed payloads — the user can re-trigger from history.
    }
  });

  // Separate from the prefill onMount above: that one is a synchronous one-shot
  // read of sessionStorage and must not be made async to wait on a network call.
  onMount(async () => {
    try {
      audioSupported = audioInputAllowed(await getHealth());
    } catch {
      // Optimistic on failure. The server still drops what it cannot read and
      // reports it in `warnings`, so the worst case is one wasted attachment —
      // better than greying out a working feature because /health blipped.
    }
  });

  async function handleSend() {
    if (!prompt.trim() && pendingImages.length === 0 && pendingAudio.length === 0) return;
    sending = true;
    error = '';
    // Prepend the date to the USER prompt, NOT the system prompt — the
    // system prompt must stay byte-stable across requests so llama.cpp's KV
    // prefix cache can hit globally instead of busting every midnight UTC
    // (or every time the user edits the system prompt).
    const dateLine = `CURRENT DATE: ${todayIso()}`;
    const userText = prompt.trim() ? `${dateLine}\n\n${prompt}` : dateLine;
    const currentImages = pendingImages.map((img) => img.dataUrl);
    const currentAudio = pendingAudio.map((a) => a.dataUrl);
    const currentSystem = systemPrompt.trim();
    try {
      const cfg = toSamplingConfig(sampling);
      const resp: ChatResponse = await sendChat({
        prompt: userText,
        system: currentSystem || undefined,
        images: currentImages.length > 0 ? currentImages : undefined,
        audio: currentAudio.length > 0 ? currentAudio : undefined,
        ...cfg
      });
      exchanges = [
        ...exchanges,
        {
          prompt: userText,
          system: currentSystem,
          response: resp.response,
          images: currentImages,
          audio: currentAudio,
          warnings: resp.warnings ?? [],
          prompt_tokens: resp.prompt_tokens,
          completion_tokens: resp.completion_tokens,
          duration_ms: resp.duration_ms,
          preset: sampling.preset ?? undefined
        }
      ];
      // The server is the authority on whether it could read audio, and this
      // response is the freshest reading there is — a mode flipped in another tab
      // shows up here before /health is asked again.
      if ((resp.warnings ?? []).length > 0) audioSupported = false;
      pendingImages = [];
      pendingAudio = [];
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to send';
    }
    sending = false;
  }

  function readAsDataUrl(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error ?? new Error('read failed'));
      reader.onload = () => resolve(String(reader.result));
      reader.readAsDataURL(file);
    });
  }

  // Route each dropped/selected/pasted file by MIME: images and audio land in
  // their own pending lists; anything else is skipped with a note. This one
  // path backs the drop zone, both file pickers, and paste.
  async function handleFiles(files: FileList | File[] | null | undefined) {
    if (!files) return;
    const list = Array.from(files);
    const acceptedImages: PendingImage[] = [];
    const acceptedAudio: PendingAudio[] = [];
    for (const file of list) {
      const isImage = file.type.startsWith('image/');
      const isAudio = file.type.startsWith('audio/');
      if (!isImage && !isAudio) {
        error = `Skipped ${file.name}: not an image or audio file (${file.type || 'unknown'})`;
        continue;
      }
      if (isAudio && !audioSupported) {
        error = `Skipped ${file.name}: ${NO_AUDIO_REASON}`;
        continue;
      }
      const cap = isAudio ? MAX_AUDIO_BYTES : MAX_IMAGE_BYTES;
      if (file.size > cap) {
        error = `${file.name} is ${(file.size / 1024 / 1024).toFixed(1)} MB — limit is ${cap / 1024 / 1024} MB`;
        continue;
      }
      try {
        const dataUrl = await readAsDataUrl(file);
        const entry = { id: crypto.randomUUID(), name: file.name, size: file.size, dataUrl };
        if (isAudio) acceptedAudio.push(entry);
        else acceptedImages.push(entry);
      } catch (e) {
        error = `Failed to read ${file.name}: ${e instanceof Error ? e.message : e}`;
      }
    }
    if (acceptedImages.length > 0) {
      pendingImages = [...pendingImages, ...acceptedImages];
      error = '';
    }
    if (acceptedAudio.length > 0) {
      pendingAudio = [...pendingAudio, ...acceptedAudio];
      error = '';
    }
  }

  function onFileInput(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    void handleFiles(target.files);
    target.value = '';
  }

  function onPaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const item of items) {
      if (item.kind === 'file') {
        const f = item.getAsFile();
        if (f && (f.type.startsWith('image/') || f.type.startsWith('audio/'))) files.push(f);
      }
    }
    if (files.length > 0) {
      e.preventDefault();
      void handleFiles(files);
    }
  }

  function removeImage(id: string) {
    pendingImages = pendingImages.filter((img) => img.id !== id);
  }

  function removeAudio(id: string) {
    pendingAudio = pendingAudio.filter((a) => a.id !== id);
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
    void handleFiles(e.dataTransfer.files);
  }

  function formatBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      handleSend();
    }
  }

  function clearHistory() {
    exchanges = [];
  }

  function clearPrompt() {
    prompt = '';
  }

  function resetSystemPrompt() {
    systemPrompt = DEFAULT_SYSTEM_PROMPT;
  }

  function clearSystemPrompt() {
    systemPrompt = '';
  }
</script>

<div class="page-container chat-page">
  <div class="section-header">
    <h2>Chat</h2>
    {#if exchanges.length > 0}
      <button class="btn-sec compact" type="button" onclick={clearHistory}>Clear History</button>
    {/if}
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  <!-- Exchange history -->
  {#if exchanges.length > 0}
    <div class="exchange-list">
      {#each exchanges as ex, i (i)}
        <div class="exchange-card">
          <div class="exchange-prompt">
            <div class="exchange-label">Prompt</div>
            {#if ex.images.length > 0}
              <div class="exchange-images">
                {#each ex.images as src, j (j)}
                  <img class="exchange-image" {src} alt="attachment {j + 1}" />
                {/each}
              </div>
            {/if}
            {#if ex.audio.length > 0}
              <div class="exchange-audio">
                {#each ex.audio as src, j (j)}
                  <audio class="exchange-audio-player" controls {src}></audio>
                {/each}
              </div>
            {/if}
            {#if ex.prompt}
              <JsonBlock content={ex.prompt} label="Prompt" />
            {/if}
          </div>
          {#if ex.warnings.length > 0}
            <!-- 🔴 On the message, not in the page-level error banner: this is a
                 fact about THIS turn ("the model never heard your audio"), and it
                 has to still be there three turns later when you are re-reading
                 an answer that does not mention the sound. -->
            <div class="exchange-warnings">
              {#each ex.warnings as w, k (k)}
                <div class="exchange-warning">⚠ {w}</div>
              {/each}
            </div>
          {/if}
          <div class="exchange-response">
            <div class="exchange-label">Response</div>
            <JsonBlock content={ex.response} label="Response" />
          </div>
          {#if ex.prompt_tokens != null || ex.completion_tokens != null || ex.duration_ms != null || ex.preset}
            <div class="exchange-meta">
              {#if ex.preset}
                <span class="preset-chip">{ex.preset}</span>
              {/if}
              {#if ex.prompt_tokens != null}
                <span class="meta-item">Prompt: {ex.prompt_tokens} tokens</span>
              {/if}
              {#if ex.completion_tokens != null}
                <span class="meta-item">Completion: {ex.completion_tokens} tokens</span>
              {/if}
              {#if ex.duration_ms != null}
                <span class="meta-item">{ex.duration_ms}ms</span>
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {:else}
    <div class="empty-card">
      <svg viewBox="0 0 24 24" width="48" height="48" fill="currentColor" class="empty-icon">
        <path
          d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zm0 14H6l-2 2V4h16v12z"
        ></path>
      </svg>
      <p>Send a prompt to the LLM and see the response here.</p>
    </div>
  {/if}

  <!-- Input area -->
  <div class="input-area">
    <div
      class="input-card"
      class:drag-active={dragActive}
      ondragenter={onDragEnter}
      ondragover={onDragOver}
      ondragleave={onDragLeave}
      ondrop={onDrop}
      role="region"
      aria-label={audioSupported
        ? 'Chat input, drop images or audio to attach'
        : 'Chat input, drop images to attach'}
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
      <!-- System prompt toggle -->
      <div class="field-header">
        <button class="system-toggle" type="button" onclick={() => (showSystem = !showSystem)}>
          <svg
            class="chevron"
            class:expanded={showSystem}
            viewBox="0 0 24 24"
            width="14"
            height="14"
            fill="currentColor"
          >
            <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"></path>
          </svg>
          <span>System Prompt</span>
          {#if systemPrompt.trim()}
            <span class="system-active-dot"></span>
          {/if}
        </button>
        {#if showSystem}
          <div class="field-actions">
            <button class="link-btn" type="button" onclick={resetSystemPrompt}>Reset</button>
            <button class="link-btn" type="button" onclick={clearSystemPrompt}>Clear</button>
          </div>
        {/if}
      </div>

      {#if showSystem}
        <textarea
          class="system-textarea"
          bind:value={systemPrompt}
          rows="6"
          placeholder="System instructions..."></textarea>
        <div class="date-hint">
          A <code>CURRENT DATE: {todayIso()}</code> line is automatically prepended to your prompt (kept
          out of the system prompt so it caches).
        </div>
      {/if}

      <!-- Main prompt -->
      <div class="field-header">
        <span class="field-label">Prompt</span>
        <div class="field-actions">
          <button class="link-btn" type="button" onclick={() => fileInputEl?.click()}
            >Attach Image</button
          >
          <button
            class="link-btn"
            type="button"
            disabled={!audioSupported}
            title={audioSupported ? 'Attach an audio file' : `Unavailable: ${NO_AUDIO_REASON}`}
            onclick={() => audioInputEl?.click()}>Attach Audio</button
          >
          {#if prompt}
            <button class="link-btn" type="button" onclick={clearPrompt}>Clear</button>
          {/if}
        </div>
      </div>
      <input
        bind:this={fileInputEl}
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

      {#if !audioSupported}
        <div class="no-audio-note">
          ⚠ Audio input is unavailable — {NO_AUDIO_REASON}
          <a href="/gpu-services">GPU Services</a>
        </div>
      {/if}

      {#if pendingImages.length > 0}
        <div class="image-strip">
          {#each pendingImages as img (img.id)}
            <div class="image-chip">
              <img class="image-thumb" src={img.dataUrl} alt={img.name} />
              <button
                class="image-remove"
                type="button"
                title="Remove {img.name}"
                aria-label="Remove {img.name}"
                onclick={() => removeImage(img.id)}>×</button
              >
              <div class="image-meta">{formatBytes(img.size)}</div>
            </div>
          {/each}
        </div>
      {/if}

      {#if pendingAudio.length > 0}
        <div class="audio-strip">
          {#each pendingAudio as a (a.id)}
            <div class="audio-chip">
              <div class="audio-chip-head">
                <span class="audio-name" title={a.name}>{a.name}</span>
                <button
                  class="audio-remove"
                  type="button"
                  title="Remove {a.name}"
                  aria-label="Remove {a.name}"
                  onclick={() => removeAudio(a.id)}>×</button
                >
              </div>
              <audio class="audio-preview" controls src={a.dataUrl}></audio>
              <div class="audio-meta">{formatBytes(a.size)}</div>
            </div>
          {/each}
        </div>
      {/if}

      <textarea
        class="prompt-textarea"
        bind:value={prompt}
        rows="8"
        placeholder="Type your prompt... (Ctrl+Enter to send, or paste an image)"
        onkeydown={handleKeydown}
        onpaste={onPaste}></textarea>

      <!-- Sampling controls -->
      <div class="sampling-wrap">
        <SamplingControls bind:value={sampling} collapsible defaultExpanded={false} />
      </div>

      <div class="input-controls">
        <button
          class="btn-primary send-btn"
          type="button"
          onclick={handleSend}
          disabled={sending ||
            (!prompt.trim() && pendingImages.length === 0 && pendingAudio.length === 0)}
        >
          {#if sending}
            <span class="spinner"></span>
            Sending...
          {:else}
            Send
          {/if}
        </button>
      </div>
    </div>
  </div>
</div>

<style>
  .chat-page {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
  }

  .section-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .exchange-list {
    display: flex;
    flex-direction: column;
    gap: 12px;
    flex: 1 1 0;
    min-height: 240px;
    overflow-y: auto;
    padding-bottom: 16px;
  }

  .exchange-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 14px;
    overflow: hidden;
    flex: 0 0 auto;
  }

  .exchange-prompt {
    padding: 14px 18px;
    border-bottom: 1px solid var(--ring);
  }

  .exchange-response {
    padding: 14px 18px;
  }

  .exchange-warnings {
    padding: 10px 18px;
    border-bottom: 1px solid var(--ring);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .exchange-warning {
    font-size: 12px;
    line-height: 1.5;
    color: var(--warning);
  }

  .exchange-label {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    font-weight: 600;
    margin-bottom: 6px;
  }

  .exchange-meta {
    display: flex;
    gap: 16px;
    padding: 10px 18px;
    border-top: 1px solid var(--ring);
    background: rgba(0, 0, 0, 0.1);
  }

  :global([data-theme='light']) .exchange-meta {
    background: rgba(0, 0, 0, 0.03);
  }

  .meta-item {
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .preset-chip {
    font-size: 10px;
    font-weight: 600;
    padding: 2px 7px;
    border-radius: 5px;
    background: var(--accent-dim);
    color: var(--accent);
    text-transform: lowercase;
    letter-spacing: normal;
  }

  .empty-card {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 16px;
    padding: 60px 24px;
    text-align: center;
    color: var(--muted);
    flex: 1;
  }

  .empty-icon {
    opacity: 0.2;
  }

  .empty-card p {
    max-width: 360px;
    font-size: 14px;
    line-height: 1.5;
  }

  /* Input area */
  .input-area {
    flex-shrink: 0;
    padding-top: 12px;
  }

  .input-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 16px;
    max-width: 1100px;
    position: relative;
    transition: border-color 0.15s;
  }

  .input-card.drag-active {
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

  .field-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 6px;
  }

  .field-label {
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
  }

  .field-actions {
    display: flex;
    gap: 8px;
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

  .no-audio-note {
    margin: 6px 0 10px;
    padding: 8px 12px;
    border: 1px solid var(--warning);
    border-radius: 10px;
    background: color-mix(in srgb, var(--warning) 10%, transparent);
    color: var(--muted);
    font-size: 12px;
    line-height: 1.5;
  }

  .no-audio-note a {
    color: var(--warning);
  }

  .date-hint {
    font-size: 11px;
    color: var(--muted);
    margin: -4px 0 10px;
  }

  .date-hint code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    color: var(--ink);
  }

  .system-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    padding: 0;
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .system-toggle:hover {
    color: var(--ink);
  }

  .system-active-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
  }

  .chevron {
    transition: transform 0.2s;
  }

  .chevron.expanded {
    transform: rotate(180deg);
  }

  .system-textarea {
    width: 100%;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: rgba(0, 0, 0, 0.15);
    color: var(--ink);
    font-size: 13px;
    line-height: 1.5;
    resize: vertical;
    margin-bottom: 10px;
  }

  :global([data-theme='light']) .system-textarea {
    background: rgba(0, 0, 0, 0.04);
  }

  .prompt-textarea {
    width: 100%;
    padding: 12px 14px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: transparent;
    color: var(--ink);
    font-size: 14px;
    line-height: 1.5;
    resize: vertical;
  }

  .file-input-hidden {
    display: none;
  }

  .image-strip {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-bottom: 10px;
  }

  .image-chip {
    position: relative;
    width: 96px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    overflow: hidden;
    background: rgba(0, 0, 0, 0.1);
  }

  .image-thumb {
    width: 96px;
    height: 96px;
    object-fit: cover;
    display: block;
  }

  .image-remove {
    position: absolute;
    top: 4px;
    right: 4px;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    border: none;
    background: rgba(0, 0, 0, 0.65);
    color: #fff;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }

  .image-remove:hover {
    background: rgba(239, 68, 68, 0.85);
  }

  .image-meta {
    padding: 3px 6px;
    font-size: 10px;
    color: var(--muted);
    text-align: center;
    font-variant-numeric: tabular-nums;
  }

  .audio-strip {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-bottom: 10px;
  }

  .audio-chip {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 10px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: rgba(0, 0, 0, 0.1);
    min-width: 240px;
    max-width: 320px;
  }

  :global([data-theme='light']) .audio-chip {
    background: rgba(0, 0, 0, 0.03);
  }

  .audio-chip-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }

  .audio-name {
    font-size: 12px;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .audio-remove {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    border: none;
    background: rgba(0, 0, 0, 0.45);
    color: #fff;
    font-size: 15px;
    line-height: 1;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }

  .audio-remove:hover {
    background: rgba(239, 68, 68, 0.85);
  }

  .audio-preview {
    width: 100%;
    height: 34px;
  }

  .audio-meta {
    font-size: 10px;
    color: var(--muted);
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .exchange-audio {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 10px;
  }

  .exchange-audio-player {
    width: 100%;
    max-width: 360px;
    height: 34px;
  }

  .exchange-images {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 10px;
  }

  .exchange-image {
    max-width: 160px;
    max-height: 160px;
    border-radius: 8px;
    border: 1px solid var(--ring);
    display: block;
  }

  .prompt-textarea:focus {
    outline: none;
    border-color: var(--accent);
  }

  .sampling-wrap {
    margin-top: 12px;
  }

  .input-controls {
    display: flex;
    gap: 16px;
    align-items: flex-end;
    margin-top: 12px;
    flex-wrap: wrap;
  }

  .send-btn {
    margin-left: auto;
    padding: 10px 24px;
    display: flex;
    align-items: center;
    gap: 8px;
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

  .error-banner {
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger);
    font-size: 13px;
    margin-bottom: 12px;
    max-width: 900px;
  }

  .compact {
    padding: 6px 14px;
    font-size: 12px;
  }

  @media (max-width: 600px) {
    .input-controls {
      flex-direction: column;
      align-items: stretch;
    }

    .send-btn {
      margin-left: 0;
    }
  }
</style>
