<script lang="ts">
  import { page } from '$app/stores';
  import { clearAuth } from '$lib/stores/auth.svelte';
  import { navItems, isActive } from '$lib/data/nav';

  function handleLogout() {
    clearAuth();
  }
</script>

<aside class="sidebar">
  <div class="sidebar-content">
    <nav class="nav-items">
      {#each navItems as item (item.href)}
        <a
          href={item.href}
          class="nav-item"
          class:active={isActive($page.url.pathname, item.href)}
          title={item.newTab ? `${item.label} (opens in a new tab)` : item.label}
          target={item.newTab ? '_blank' : undefined}
          rel={item.newTab ? 'noopener' : undefined}
        >
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
            <path d={item.icon}></path>
          </svg>
          <span class="nav-label">{item.label}</span>
          {#if item.newTab}
            <svg
              class="ext"
              viewBox="0 0 24 24"
              width="12"
              height="12"
              fill="currentColor"
              aria-hidden="true"
            >
              <path
                d="M14 3v2h3.59l-9.83 9.83 1.41 1.41L19 6.41V10h2V3h-7zM19 19H5V5h7V3H5c-1.11 0-2 .9-2 2v14c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2v-7h-2v7z"
              ></path>
            </svg>
          {/if}
        </a>
      {/each}
    </nav>
  </div>

  <div class="sidebar-footer">
    <button
      class="logout-btn"
      type="button"
      aria-label="Log out"
      onclick={handleLogout}
      title="Log out"
    >
      <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
        <path
          d="M17 7l-1.41 1.41L18.17 11H8v2h10.17l-2.58 2.58L17 17l5-5-5-5zM4 5h8V3H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h8v-2H4V5z"
        ></path>
      </svg>
      <span>Log out</span>
    </button>
  </div>
</aside>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    width: 220px;
    background: var(--card);
    border-right: 1px solid var(--ring);
    height: 100%;
    overflow: hidden;
    flex-shrink: 0;
  }

  .sidebar-content {
    flex: 1;
    overflow-y: auto;
    overflow-x: hidden;
    padding: 12px 0;
  }

  .nav-items {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 0 8px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    border-radius: 10px;
    color: var(--ink);
    text-decoration: none;
    font-size: 13px;
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
    padding-left: 20px;
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

  .nav-item .ext {
    flex-shrink: 0;
    opacity: 0.4;
  }
  .nav-item:hover .ext {
    opacity: 0.75;
  }

  .sidebar-footer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-top: 1px solid var(--ring);
    flex-shrink: 0;
  }

  .logout-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 8px 12px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
    font-size: 13px;
    transition:
      color 0.15s,
      background 0.15s;
  }

  .logout-btn:hover {
    color: var(--danger);
    background: rgba(239, 68, 68, 0.1);
  }

  @media (max-width: 768px) {
    .sidebar {
      display: none;
    }
  }
</style>
