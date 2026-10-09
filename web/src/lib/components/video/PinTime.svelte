<script lang="ts">
  // Frame-accurate keyframe-pin timing: Start / Middle / End presets + a frame-indexed input,
  // clamped to the shot's REAL rendered frame count (8k+1). Stores back into `seconds` (the wire
  // format) as `frame / fps` so it round-trips to the exact same frame server-side.
  import {
    framesFromSeconds,
    frameToSeconds,
    presetFrame,
    secondsToFrame,
    type PinPreset
  } from '$lib/ltx/frames';

  let {
    seconds = $bindable(),
    shotSeconds,
    fps,
    onchange
  }: {
    seconds: number;
    shotSeconds: number;
    fps: number;
    onchange?: () => void;
  } = $props();

  const frames = $derived(framesFromSeconds(shotSeconds, fps));
  const last = $derived(frames - 1);
  const frame = $derived(secondsToFrame(seconds, fps, shotSeconds));
  const mid = $derived(Math.round(last / 2));

  function setFrame(f: number) {
    const clamped = Math.min(last, Math.max(0, Math.round(f || 0)));
    seconds = frameToSeconds(clamped, fps);
    onchange?.();
  }
  function preset(p: PinPreset) {
    setFrame(presetFrame(p, shotSeconds, fps));
  }
</script>

<div class="pintime">
  <div class="presets">
    <button type="button" class:active={frame === 0} onclick={() => preset('start')}>Start</button>
    <button type="button" class:active={frame === mid} onclick={() => preset('middle')}
      >Middle</button
    >
    <button type="button" class:active={frame === last} onclick={() => preset('end')}>End</button>
  </div>
  <label class="frame-in">
    frame
    <input
      type="number"
      min="0"
      max={last}
      step="1"
      value={frame}
      oninput={(e) => setFrame(parseInt(e.currentTarget.value, 10))}
    />
    / {last}
  </label>
  <span class="ro">{frameToSeconds(frame, fps).toFixed(2)}s of {(frames / fps).toFixed(2)}s</span>
</div>

<style>
  .pintime {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 11px;
    color: var(--muted);
  }
  .presets {
    display: inline-flex;
    gap: 2px;
  }
  .presets button {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 3px 8px;
    font-size: 11px;
    cursor: pointer;
    border-radius: 6px;
  }
  .presets button.active {
    border-color: var(--accent);
    color: var(--accent);
  }
  .frame-in {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--ink);
  }
  .frame-in input {
    width: 4rem;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    color: var(--ink);
    padding: 4px 6px;
    font: inherit;
    font-size: 11px;
  }
  .ro {
    white-space: nowrap;
  }
</style>
