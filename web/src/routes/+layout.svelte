<script lang="ts">
  import { onMount } from 'svelte';
  import '../app.css';
  import { Toast } from '@koblibs/ui';
  import BeefyBanner from '#lib/components/BeefyBanner.svelte';
  import LoginForm from '#lib/components/LoginForm.svelte';
  import NavBar from '#lib/components/NavBar.svelte';
  import Sidebar from '#lib/components/Sidebar.svelte';
  import MobileMenu from '#lib/components/MobileMenu.svelte';
  import { login } from '#lib/api/client.ts';
  import { getAuthState, loadAuthToken, setAuthToken } from '#lib/stores/auth.svelte.ts';
  import { getUIState, setTheme, setMenuOpen } from '#lib/stores/ui.svelte.ts';
  import { loadNotifyPreference } from '#lib/stores/notify.svelte.ts';
  import { page } from '$app/state';

  let { children } = $props();

  const auth = getAuthState();
  const ui = getUIState();

  let loading = $state(false);
  let hydrated = $state(false);
  let actionMessage = $state('');

  const isAuthed = $derived(auth.isAuthed);
  const theme = $derived(ui.theme);
  const menuOpen = $derived(ui.menuOpen);
  // The Director is a full-page workspace with its own app bar (.dw .appbar), so the
  // global NavBar + left nav would just eat chrome around it. It opens in its own tab.
  const hideChrome = $derived(isAuthed && page.url.pathname.startsWith('/director'));

  const handleLogin = async (username: string, password: string) => {
    loading = true;
    try {
      const resp = await login(username, password);
      setAuthToken(resp.token);
    } finally {
      loading = false;
    }
  };

  onMount(() => {
    // Initialize theme
    const storedTheme = localStorage.getItem('koblem-theme');
    if (storedTheme === 'light' || storedTheme === 'dark') {
      setTheme(storedTheme);
    } else if (window.matchMedia?.('(prefers-color-scheme: light)').matches) {
      setTheme('light');
    } else {
      setTheme('dark');
    }

    // Cleanup startup meta tag
    const startupMeta = document.getElementById('startup-theme-color');
    if (startupMeta) startupMeta.remove();

    // Load saved auth token
    loadAuthToken();

    // Reads an existing grant only — the prompt itself lives behind the toggle
    // on /queue.
    loadNotifyPreference();

    hydrated = true;
  });
</script>

<svelte:head>
  <title>Koblem</title>
  <meta name="color-scheme" content="dark light" />
  <meta name="theme-color" content={theme === 'dark' ? '#0f1115' : '#f5efe7'} />
</svelte:head>

<div class="app" data-theme={theme}>
  {#if !hideChrome}
    <NavBar {isAuthed} onToggleMenu={() => setMenuOpen(!menuOpen)} />
  {/if}

  <div class="app-body">
    {#if isAuthed && !hideChrome}
      <Sidebar />
    {/if}

    <main class="content">
      {#if !hydrated}
        <div class="loading-card">Loading...</div>
      {:else if !isAuthed}
        <LoginForm onLogin={handleLogin} {loading} />
      {:else}
        <Toast message={actionMessage} />
        <!-- 🔴 Above the page, on the same guard as the sidebar and the NavBar:
             the Director workspace hides chrome because it owns its own app bar,
             and a banner nailed over it would sit on the canvas. The Director
             opens in its own tab from a page that DOES show this, so the state
             is seen before you get there. -->
        {#if isAuthed && !hideChrome}
          <BeefyBanner />
        {/if}
        {@render children()}
      {/if}
    </main>
  </div>
</div>

<MobileMenu open={menuOpen} onClose={() => setMenuOpen(false)} />

<style>
  :global(:root) {
    --sans: 'Inter', 'Segoe UI', system-ui, sans-serif;
    --bg: #0f1115;
    --bg-2: #15181e;
    --ink: #f3f1ee;
    --muted: #9da6b0;
    --accent: #d38c5a;
    --accent-dim: rgba(211, 140, 90, 0.15);
    --accent-2: #2d6b67;
    --card: #191d24;
    --ring: #2a3038;
    --shadow: 0 18px 40px rgba(5, 7, 10, 0.5);
    --radius: 18px;
    --tag-bg: rgba(255, 255, 255, 0.08);
    --tag-text: #c6d2da;
    --danger: #ef4444;
    --success: #22c55e;
    --warning: #f97316;
  }

  :global([data-theme='light']) {
    --bg: #f5efe7;
    --bg-2: #efe6db;
    --ink: #1f1b17;
    --muted: #5f6b72;
    --accent: #c87a46;
    --accent-dim: rgba(200, 122, 70, 0.15);
    --accent-2: #276f6a;
    --card: #fff9f1;
    --ring: #d6c5b2;
    --shadow: 0 18px 40px rgba(28, 24, 20, 0.12);
    --tag-bg: rgba(0, 0, 0, 0.06);
    --tag-text: #5c6369;
    --danger: #dc2626;
    --success: #16a34a;
    --warning: #ea580c;
  }

  :global(*),
  :global(*::before),
  :global(*::after) {
    box-sizing: border-box;
  }

  :global(html),
  :global(body) {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
  }

  :global(body) {
    font-family: var(--sans);
    background-color: var(--bg);
    background: radial-gradient(circle at top, var(--bg-2) 0%, var(--bg) 50%, var(--bg-2) 100%);
    color: var(--ink);
  }

  :global(*) {
    scrollbar-width: thin;
    scrollbar-color: var(--ring) transparent;
  }

  :global(*::-webkit-scrollbar) {
    width: 8px;
    height: 8px;
  }

  :global(*::-webkit-scrollbar-track) {
    background: transparent;
  }

  :global(*::-webkit-scrollbar-thumb) {
    background: var(--ring);
    border-radius: 4px;
  }

  :global(*::-webkit-scrollbar-thumb:hover) {
    background: var(--muted);
  }

  .app {
    width: 100%;
    height: 100vh;
    height: 100dvh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .app-body {
    flex: 1;
    display: flex;
    min-height: 0;
    overflow: hidden;
  }

  .content {
    flex: 1;
    overflow-y: auto;
    overflow-x: hidden;
    scrollbar-gutter: stable;
    display: flex;
    flex-direction: column;
    position: relative;
  }

  :global(select option) {
    color: #12161d;
    background: #ffffff;
  }

  .loading-card {
    max-width: 320px;
    margin: 60px auto 0;
    padding: 16px;
    border-radius: var(--radius);
    border: 1px solid var(--ring);
    background: var(--card);
    text-align: center;
  }
</style>
