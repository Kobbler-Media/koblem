export { registerOverlay, unregisterOverlay, closeTopOverlay, hasOpenOverlays } from '@koblibs/ui';

let theme = $state<'dark' | 'light'>('dark');
let menuOpen = $state(false);

export function getUIState() {
  return {
    get theme() {
      return theme;
    },
    get menuOpen() {
      return menuOpen;
    }
  };
}

export function setTheme(newTheme: 'dark' | 'light') {
  theme = newTheme;
  if (typeof document !== 'undefined') {
    document.documentElement.setAttribute('data-theme', newTheme);
    localStorage.setItem('koblem-theme', newTheme);
  }
}

export function toggleTheme() {
  setTheme(theme === 'dark' ? 'light' : 'dark');
}

export function setMenuOpen(open: boolean) {
  menuOpen = open;
}
