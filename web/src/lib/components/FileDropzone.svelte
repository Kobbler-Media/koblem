<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    accept?: string;
    multiple?: boolean;
    disabled?: boolean;
    label?: string;
    hint?: string;
    files?: FileList | null;
    onFiles?: (files: FileList) => void;
    children?: Snippet;
  }

  let {
    accept,
    multiple = false,
    disabled = false,
    label = 'Drop file here',
    hint = 'or click to browse',
    files = $bindable(null),
    onFiles,
    children
  }: Props = $props();

  let dragActive = $state(false);
  let dragDepth = 0;

  function handleFiles(list: FileList | null) {
    if (!list || list.length === 0) return;
    files = list;
    onFiles?.(list);
  }

  function onChange(e: Event) {
    handleFiles((e.target as HTMLInputElement).files);
  }

  function onDragEnter(e: DragEvent) {
    if (disabled || !e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    dragDepth += 1;
    dragActive = true;
  }

  function onDragOver(e: DragEvent) {
    if (disabled || !e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
  }

  function onDragLeave(e: DragEvent) {
    if (disabled || !e.dataTransfer?.types.includes('Files')) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dragActive = false;
  }

  function onDrop(e: DragEvent) {
    if (disabled || !e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    dragDepth = 0;
    dragActive = false;
    handleFiles(e.dataTransfer.files);
  }

  function describeFiles(f: FileList): string {
    if (f.length === 1) return f[0].name;
    return `${f.length} files selected`;
  }
</script>

<label
  class="dropzone"
  class:drag-active={dragActive}
  class:has-file={files && files.length > 0}
  class:disabled
  ondragenter={onDragEnter}
  ondragover={onDragOver}
  ondragleave={onDragLeave}
  ondrop={onDrop}
>
  <input class="hidden" type="file" {accept} {multiple} {disabled} onchange={onChange} />
  <svg
    class="icon"
    viewBox="0 0 24 24"
    width="20"
    height="20"
    fill="currentColor"
    aria-hidden="true"
  >
    <path d="M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z"></path>
  </svg>
  <div class="text">
    {#if children}
      {@render children()}
    {:else if files && files.length > 0}
      <span class="label">{describeFiles(files)}</span>
      <span class="hint">Click or drop to replace</span>
    {:else}
      <span class="label">{label}</span>
      <span class="hint">{hint}</span>
    {/if}
  </div>
</label>

<style>
  .dropzone {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    box-sizing: border-box;
    padding: 14px 16px;
    background: var(--bg-2);
    border: 1px dashed var(--ring);
    border-radius: 10px;
    color: var(--ink);
    font: inherit;
    text-align: left;
    cursor: pointer;
    transition:
      border-color 0.12s,
      background 0.12s;
  }
  .dropzone:hover:not(.disabled) {
    border-color: var(--accent);
  }
  .dropzone.drag-active {
    border-color: var(--accent);
    border-style: solid;
    background: color-mix(in srgb, var(--accent) 8%, var(--bg-2));
  }
  .dropzone.has-file {
    border-style: solid;
  }
  .dropzone.disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .icon {
    flex: 0 0 auto;
    color: var(--muted);
  }
  .dropzone.drag-active .icon,
  .dropzone.has-file .icon {
    color: var(--accent);
  }
  .text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    overflow: hidden;
  }
  .label {
    font-size: 13px;
    font-weight: 500;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hint {
    font-size: 12px;
    color: var(--muted);
  }
  .hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
