<script lang="ts">
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import type { StoryContext } from '#lib/api/client.ts';

  // The project's story bible: canon the LLM grounds every shot in, never rendered. Bound straight
  // into the editor's own state — the callbacks are only for the reference images, which have to
  // reach the asset store before a ref can be written here.
  let {
    story = $bindable(),
    onchange,
    onaddref,
    onremoveref,
    onrefimage,
    onclearrefimage
  }: {
    story: StoryContext;
    onchange: () => void;
    onaddref: () => void;
    onremoveref: (i: number) => void;
    onrefimage: (i: number, files: FileList | null) => void;
    onclearrefimage: (i: number) => void;
  } = $props();
</script>

<p class="hint" style="margin-bottom:10px">
  Canon the LLM grounds every shot in. Never rendered — it just steers the writing.
</p>
<label class="field"
  ><span>Backstory / scenario</span>
  <textarea
    class="inp"
    rows="3"
    bind:value={story.backstory}
    oninput={onchange}
    placeholder="The world, characters and plot the shots live in."></textarea>
</label>
<label class="field"
  ><span>Tone / visual style</span>
  <textarea
    class="inp"
    rows="2"
    bind:value={story.tone}
    oninput={onchange}
    placeholder="Neon noir, rain, desaturated but for signage."></textarea>
</label>
<label class="field"
  ><span>Rules (always honour)</span>
  <textarea
    class="inp"
    rows="2"
    bind:value={story.rules}
    oninput={onchange}
    placeholder="Keep the red scarf in every shot."></textarea>
</label>
<div class="lbl-row"><span>Characters &amp; feel images</span></div>
{#each story.references as ref, i (i)}
  <div class="refrow">
    {#if ref.image_b64}
      <img class="refimg" src={assetSrc(ref.image_b64)} alt={ref.label || 'reference'} />
    {:else}
      <label class="refimg empty" title="Add reference image"
        ><span class="plus">＋</span> Add image
        <input
          type="file"
          accept="image/*"
          hidden
          onchange={(e) => onrefimage(i, e.currentTarget.files)}
        />
      </label>
    {/if}
    <div class="refmeta">
      <input bind:value={ref.label} oninput={onchange} placeholder="Name / tag" />
      <textarea
        rows="2"
        bind:value={ref.description}
        oninput={onchange}
        placeholder="Identity notes"></textarea>
      <div class="inline-row" style="gap:6px">
        {#if ref.image_b64}<button class="miniai" onclick={() => onclearrefimage(i)}
            >Remove image</button
          >{/if}
        <button class="miniai" onclick={() => onremoveref(i)}>Delete</button>
      </div>
    </div>
  </div>
{/each}
<button class="btn sm" style="width:100%;justify-content:center" onclick={onaddref}
  >＋ Add character or feel image</button
>
