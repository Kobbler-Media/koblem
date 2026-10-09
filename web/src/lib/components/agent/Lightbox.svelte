<script lang="ts">
  import { Modal } from '@koblibs/ui';

  let {
    src,
    alt = '',
    onClose
  }: {
    src: string;
    alt?: string;
    onClose: () => void;
  } = $props();

  let saving = $state(false);
  let saveError = $state('');

  // `src` carries the session bearer in its query string, which is fine for an
  // <img> subresource but would park a live credential in the URL bar, in
  // history and in any screenshot if it were navigated to. So the bytes are
  // fetched and handed to the browser as an origin-local blob instead.
  async function saveOriginal() {
    saving = true;
    saveError = '';
    try {
      const res = await fetch(src);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const blob = await res.blob();
      const href = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = href;
      a.download = filename(src, blob.type);
      a.click();
      URL.revokeObjectURL(href);
    } catch (e) {
      saveError = e instanceof Error ? e.message : 'download failed';
    } finally {
      saving = false;
    }
  }

  function filename(url: string, mime: string): string {
    const base = url.split('?')[0].split('/').pop() || 'asset';
    const ext =
      mime
        .split(';')[0]
        .split('/')[1]
        ?.replace(/[^a-z0-9]/gi, '') ?? '';
    return ext && !base.includes('.') ? `${base}.${ext}` : base;
  }
</script>

<Modal {onClose} width="min(92vw, 1200px)">
  <img class="full" {src} {alt} />
  {#snippet footer()}
    {#if saveError}
      <span class="save-error">Could not save it ({saveError})</span>
    {/if}
    <button class="btn-sec" type="button" onclick={saveOriginal} disabled={saving}>
      {saving ? 'Saving…' : 'Save original'}
    </button>
    <button class="btn-sec" type="button" onclick={onClose}>Close</button>
  {/snippet}
</Modal>

<style>
  .full {
    display: block;
    width: 100%;
    height: auto;
    max-height: 74vh;
    max-height: 74dvh;
    object-fit: contain;
    border-radius: 10px;
    background: var(--bg-2);
  }

  .save-error {
    font-size: 12px;
    color: var(--danger);
    margin-right: auto;
    align-self: center;
  }
</style>
