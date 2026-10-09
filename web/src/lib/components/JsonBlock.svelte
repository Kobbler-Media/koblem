<script lang="ts">
  interface Props {
    content: unknown;
    maxHeight?: string;
    label?: string;
  }

  let { content, maxHeight = '400px', label }: Props = $props();

  let copied = $state(false);

  const parsed = $derived.by(() => {
    if (content == null) return { text: '', isJson: false };
    if (typeof content === 'string') {
      const trimmed = content.trim();
      if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
        try {
          return { text: JSON.stringify(JSON.parse(trimmed), null, 2), isJson: true };
        } catch {
          // fall through
        }
      }
      return { text: content, isJson: false };
    }
    try {
      return { text: JSON.stringify(content, null, 2), isJson: true };
    } catch {
      return { text: String(content), isJson: false };
    }
  });

  const highlighted = $derived.by(() => {
    if (!parsed.isJson) return null;
    const escaped = parsed.text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    return escaped.replace(
      /("(?:\\u[a-fA-F0-9]{4}|\\[^u]|[^\\"])*"(?:\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g,
      (match) => {
        let cls = 'jb-num';
        if (/^"/.test(match)) {
          cls = /:$/.test(match) ? 'jb-key' : 'jb-str';
        } else if (match === 'true' || match === 'false') {
          cls = 'jb-bool';
        } else if (match === 'null') {
          cls = 'jb-null';
        }
        return `<span class="${cls}">${match}</span>`;
      }
    );
  });

  async function copy() {
    try {
      await navigator.clipboard.writeText(parsed.text);
      copied = true;
      setTimeout(() => (copied = false), 1200);
    } catch {
      const ta = document.createElement('textarea');
      ta.value = parsed.text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand('copy');
        copied = true;
        setTimeout(() => (copied = false), 1200);
      } catch {
        // give up silently
      }
      document.body.removeChild(ta);
    }
  }
</script>

<div class="jb-wrap" style="--jb-max-height: {maxHeight}">
  <div class="jb-toolbar">
    {#if label}
      <span class="jb-label">{label}</span>
    {:else}
      <span class="jb-label jb-label-kind">{parsed.isJson ? 'JSON' : 'text'}</span>
    {/if}
    <button
      type="button"
      class="jb-copy"
      class:copied
      onclick={copy}
      aria-label="Copy to clipboard"
    >
      {#if copied}
        <svg
          viewBox="0 0 24 24"
          width="13"
          height="13"
          fill="none"
          stroke="currentColor"
          stroke-width="2.5"
          stroke-linecap="round"
          stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg
        >
        Copied
      {:else}
        <svg
          viewBox="0 0 24 24"
          width="13"
          height="13"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          ><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path
            d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"
          ></path></svg
        >
        Copy
      {/if}
    </button>
  </div>
  <pre class="jb-pre" class:is-json={parsed.isJson}>{#if parsed.isJson}
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      {@html highlighted}{:else}{parsed.text}{/if}</pre>
</div>

<style>
  .jb-wrap {
    position: relative;
    background: rgba(0, 0, 0, 0.2);
    border: 1px solid var(--ring);
    border-radius: 10px;
    margin-top: 8px;
    overflow: hidden;
  }

  :global([data-theme='light']) .jb-wrap {
    background: rgba(0, 0, 0, 0.04);
  }

  .jb-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 10px 6px 12px;
    border-bottom: 1px solid var(--ring);
    background: rgba(255, 255, 255, 0.02);
  }

  :global([data-theme='light']) .jb-toolbar {
    background: rgba(0, 0, 0, 0.02);
  }

  .jb-label {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.6px;
    color: var(--muted);
  }

  .jb-label-kind {
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    letter-spacing: 0.4px;
  }

  .jb-copy {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 4px 10px;
    font-size: 11px;
    font-weight: 600;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--ring);
    border-radius: 6px;
    cursor: pointer;
    transition:
      color 0.15s,
      border-color 0.15s,
      background 0.15s;
  }

  .jb-copy:hover {
    color: var(--ink);
    border-color: var(--muted);
  }

  .jb-copy.copied {
    color: var(--accent);
    border-color: var(--accent);
  }

  .jb-pre {
    margin: 0;
    padding: 12px 14px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-size: 12px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
    overflow-x: auto;
    overflow-y: auto;
    max-height: var(--jb-max-height);
    color: var(--ink);
  }

  .jb-pre:not(.is-json) {
    color: var(--muted);
  }

  .jb-pre :global(.jb-key) {
    color: #7dd3fc;
  }
  .jb-pre :global(.jb-str) {
    color: #a7f3d0;
  }
  .jb-pre :global(.jb-num) {
    color: #fcd34d;
  }
  .jb-pre :global(.jb-bool) {
    color: #f0abfc;
  }
  .jb-pre :global(.jb-null) {
    color: #fca5a5;
    font-style: italic;
  }

  :global([data-theme='light']) .jb-pre :global(.jb-key) {
    color: #0369a1;
  }
  :global([data-theme='light']) .jb-pre :global(.jb-str) {
    color: #047857;
  }
  :global([data-theme='light']) .jb-pre :global(.jb-num) {
    color: #b45309;
  }
  :global([data-theme='light']) .jb-pre :global(.jb-bool) {
    color: #a21caf;
  }
  :global([data-theme='light']) .jb-pre :global(.jb-null) {
    color: #b91c1c;
  }
</style>
