<script lang="ts">
  import { page } from '$app/stores';
  import { registerOverlay, unregisterOverlay } from '$lib/stores/ui.svelte';
  import { clearAuth } from '$lib/stores/auth.svelte';
  import { navItems, isActive } from '$lib/data/nav';

  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open, onClose }: Props = $props();

  $effect(() => {
    if (open) {
      registerOverlay('mobile-menu', onClose);
      return () => unregisterOverlay('mobile-menu');
    }
  });

  function handleLogout() {
    clearAuth();
    onClose();
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="backdrop" onclick={onClose}></div>
{/if}

<div class="mobile-nav" class:open>
  <div class="mobile-nav-header">
    <span class="mobile-brand">Koblem</span>
    <button class="close-btn" type="button" aria-label="Close" onclick={onClose}>
      <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
        <path
          d="M19 6.41L17.59 5 12 10.59 6.41 5 5 6.41 10.59 12 5 17.59 6.41 19 12 13.41 17.59 19 19 17.59 13.41 12z"
        ></path>
      </svg>
    </button>
  </div>

  <nav class="mobile-nav-items">
    {#each navItems as item (item.href)}
      <a
        href={item.href}
        class="nav-item"
        class:active={isActive($page.url.pathname, item.href)}
        title={item.label}
        onclick={onClose}
      >
        <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
          <path d={item.icon}></path>
        </svg>
        <span class="nav-label">{item.label}</span>
      </a>
    {/each}
  </nav>

  <div class="mobile-nav-footer">
    <button class="logout-btn" type="button" onclick={handleLogout}>
      <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
        <path
          d="M17 7l-1.41 1.41L18.17 11H8v2h10.17l-2.58 2.58L17 17l5-5-5-5zM4 5h8V3H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h8v-2H4V5z"
        ></path>
      </svg>
      <span>Log out</span>
    </button>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 90;
  }

  .mobile-nav {
    position: fixed;
    top: 0;
    left: 0;
    bottom: 0;
    width: 280px;
    background: var(--card);
    border-right: 1px solid var(--ring);
    z-index: 100;
    transform: translateX(-100%);
    transition: transform 0.3s cubic-bezier(0.16, 1, 0.3, 1);
    display: flex;
    flex-direction: column;
  }

  .mobile-nav.open {
    transform: translateX(0);
  }

  .mobile-nav-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px;
    border-bottom: 1px solid var(--ring);
  }

  .mobile-brand {
    font-size: 20px;
    font-weight: 700;
    letter-spacing: 0.6px;
    color: var(--ink);
  }

  .close-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    padding: 4px;
    border-radius: 6px;
  }

  .close-btn:hover {
    color: var(--ink);
    background: rgba(255, 255, 255, 0.06);
  }

  .mobile-nav-items {
    flex: 1;
    overflow-y: auto;
    padding: 12px 8px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 16px;
    border-radius: 10px;
    color: var(--ink);
    text-decoration: none;
    font-size: 14px;
    transition:
      background 0.15s,
      color 0.15s;
  }

  .nav-item:hover {
    background: rgba(255, 255, 255, 0.06);
  }

  :global([data-theme='light']) .nav-item:hover {
    background: rgba(0, 0, 0, 0.06);
  }

  .nav-item.active {
    background: linear-gradient(90deg, var(--accent-dim) 0%, rgba(211, 140, 90, 0.05) 100%);
    color: var(--accent);
    border-radius: 0 10px 10px 0;
    margin-left: -8px;
    padding-left: 24px;
    border-left: 3px solid var(--accent);
  }

  :global([data-theme='light']) .nav-item.active {
    background: linear-gradient(90deg, var(--accent-dim) 0%, rgba(200, 122, 70, 0.05) 100%);
  }

  .nav-item svg {
    flex-shrink: 0;
    opacity: 0.7;
  }

  .nav-item.active svg {
    opacity: 1;
  }

  .nav-label {
    flex: 1;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .mobile-nav-footer {
    padding: 12px 16px;
    border-top: 1px solid var(--ring);
  }

  .logout-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 10px 12px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
    font-size: 14px;
  }

  .logout-btn:hover {
    color: var(--danger);
    background: rgba(239, 68, 68, 0.1);
  }

  /* Exact complement of Sidebar/NavBar's `max-width: 768px`. `min-width: 769px`
     leaves a gap at fractional widths in (768, 769) where the sidebar is back
     but this drawer is not yet hidden. */
  @media (width > 768px) {
    .backdrop,
    .mobile-nav {
      display: none;
    }
  }
</style>
