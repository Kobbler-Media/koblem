<script lang="ts">
  import { toggleTheme, getUIState } from '#lib/stores/ui.svelte.ts';
  import QueueIndicator from '#lib/components/QueueIndicator.svelte';

  interface Props {
    isAuthed: boolean;
    onToggleMenu: () => void;
  }

  let { isAuthed, onToggleMenu }: Props = $props();

  const ui = getUIState();
</script>

<header class="topbar">
  {#if isAuthed}
    <button class="mobile-menu" type="button" aria-label="Menu" onclick={onToggleMenu}>
      <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
        <path d="M4 7h16v2H4V7zm0 5h16v2H4v-2zm0 5h16v2H4v-2z"></path>
      </svg>
    </button>
  {/if}
  <a href="/" class="brand">Koblem</a>

  <div class="topbar-actions">
    {#if isAuthed}
      <QueueIndicator />
      <button
        class="icon-btn"
        type="button"
        aria-label="Toggle theme"
        onclick={toggleTheme}
        title={ui.theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}
      >
        {#if ui.theme === 'dark'}
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
            <path
              d="M12 7c-2.76 0-5 2.24-5 5s2.24 5 5 5 5-2.24 5-5-2.24-5-5-5zM2 13h2c.55 0 1-.45 1-1s-.45-1-1-1H2c-.55 0-1 .45-1 1s.45 1 1 1zm18 0h2c.55 0 1-.45 1-1s-.45-1-1-1h-2c-.55 0-1 .45-1 1s.45 1 1 1zM11 2v2c0 .55.45 1 1 1s1-.45 1-1V2c0-.55-.45-1-1-1s-1 .45-1 1zm0 18v2c0 .55.45 1 1 1s1-.45 1-1v-2c0-.55-.45-1-1-1s-1 .45-1 1zM5.99 4.58a.996.996 0 0 0-1.41 0 .996.996 0 0 0 0 1.41l1.06 1.06c.39.39 1.03.39 1.41 0s.39-1.03 0-1.41L5.99 4.58zm12.37 12.37a.996.996 0 0 0-1.41 0 .996.996 0 0 0 0 1.41l1.06 1.06c.39.39 1.03.39 1.41 0a.996.996 0 0 0 0-1.41l-1.06-1.06zm1.06-10.96a.996.996 0 0 0 0-1.41.996.996 0 0 0-1.41 0l-1.06 1.06c-.39.39-.39 1.03 0 1.41s1.03.39 1.41 0l1.06-1.06zM7.05 18.36a.996.996 0 0 0 0-1.41.996.996 0 0 0-1.41 0l-1.06 1.06c-.39.39-.39 1.03 0 1.41s1.03.39 1.41 0l1.06-1.06z"
            ></path>
          </svg>
        {:else}
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
            <path
              d="M9.37 5.51A7.35 7.35 0 0 0 9.1 7.5c0 4.08 3.32 7.4 7.4 7.4.68 0 1.35-.09 1.99-.27A7.014 7.014 0 0 1 12 19c-3.86 0-7-3.14-7-7 0-2.93 1.81-5.45 4.37-6.49zM12 3a9 9 0 1 0 9 9c0-.46-.04-.92-.1-1.36a5.389 5.389 0 0 1-4.4 2.26 5.403 5.403 0 0 1-3.14-9.8c-.44-.06-.9-.1-1.36-.1z"
            ></path>
          </svg>
        {/if}
      </button>
    {/if}
  </div>
</header>

<style>
  .topbar {
    position: sticky;
    top: 0;
    z-index: 50;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 0 20px;
    height: 56px;
    border-bottom: 1px solid var(--ring);
    background: linear-gradient(180deg, rgba(21, 24, 30, 0.95), rgba(17, 20, 25, 0.85));
    backdrop-filter: blur(6px);
    box-sizing: border-box;
  }

  :global([data-theme='light']) .topbar {
    background: linear-gradient(180deg, rgba(245, 239, 231, 0.95), rgba(239, 230, 219, 0.85));
  }

  .brand {
    font-family: inherit;
    font-size: 24px;
    font-weight: 700;
    letter-spacing: 0.6px;
    white-space: nowrap;
    text-decoration: none;
    color: var(--ink);
    line-height: 32px;
  }

  .brand:hover {
    color: var(--accent);
  }

  .topbar-actions {
    display: flex;
    gap: 10px;
    align-items: center;
    margin-left: auto;
  }

  .icon-btn {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    width: 32px;
    height: 32px;
    cursor: pointer;
    color: var(--ink);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }

  .icon-btn:hover {
    background: var(--ring);
  }

  .mobile-menu {
    display: none;
    border: 1px solid var(--ring);
    background: var(--card);
    color: var(--ink);
    border-radius: 12px;
    width: 32px;
    height: 32px;
    cursor: pointer;
    align-items: center;
    justify-content: center;
    padding: 0;
  }

  @media (max-width: 768px) {
    .topbar {
      padding: 0 12px;
      height: 48px;
      justify-content: flex-start;
    }

    .brand {
      font-size: 20px;
      line-height: 28px;
    }

    .mobile-menu {
      display: inline-flex;
    }
  }
</style>
