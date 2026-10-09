import { describe, expect, it } from 'vitest';
import { parseTierPreference, preferTier, resolveTier } from './tier';

describe('resolveTier', () => {
  it('leaves a desktop client on the final tier', () => {
    expect(resolveTier('auto', false)).toBe('final');
  });

  it('drops a narrow client to the proxy tier', () => {
    expect(resolveTier('auto', true)).toBe('proxy');
  });

  it('honours an explicit choice at any width', () => {
    expect(resolveTier('final', true)).toBe('final');
    expect(resolveTier('proxy', false)).toBe('proxy');
  });
});

describe('parseTierPreference', () => {
  it('reads back what it stored', () => {
    expect(parseTierPreference('proxy')).toBe('proxy');
    expect(parseTierPreference('final')).toBe('final');
    expect(parseTierPreference('auto')).toBe('auto');
  });

  it('falls back to auto for anything else, so a corrupt key cannot pin a client to one tier', () => {
    expect(parseTierPreference(null)).toBe('auto');
    expect(parseTierPreference(undefined)).toBe('auto');
    expect(parseTierPreference('')).toBe('auto');
    expect(parseTierPreference('FINAL')).toBe('auto');
  });
});

describe('preferTier', () => {
  it('plays the tier this client asked for when it has something', () => {
    expect(preferTier('proxy', 'p', 'f')).toBe('p');
    expect(preferTier('final', 'p', 'f')).toBe('f');
  });

  it('falls back to the other tier rather than showing nothing', () => {
    expect(preferTier('proxy', null, 'f')).toBe('f');
    expect(preferTier('final', 'p', null)).toBe('p');
  });

  it('is null only when neither tier has anything', () => {
    expect(preferTier('proxy', null, null)).toBeNull();
    expect(preferTier('final', null, null)).toBeNull();
  });
});
