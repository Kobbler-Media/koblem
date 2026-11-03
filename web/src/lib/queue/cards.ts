import { listGpuCards, type GpuCard } from '#lib/api/client.ts';

let inFlight: Promise<GpuCard[]> | null = null;

/// Every render panel mounts the queue controls; one card list serves all of them.
/// A failure clears the cache so the next disclosure retries.
export function gpuCards(): Promise<GpuCard[]> {
  inFlight ??= listGpuCards()
    .then((r) => r.cards)
    .catch((e) => {
      inFlight = null;
      throw e;
    });
  return inFlight;
}

export function cardOption(card: GpuCard): string {
  const tail = card.uuid.length > 12 ? `…${card.uuid.slice(-8)}` : card.uuid;
  return `${card.label} · ${tail}${card.enabled ? '' : ' (disabled)'}`;
}
