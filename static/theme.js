'use strict';
(() => {
  const key = 'youtube-downloader-theme';
  const system = window.matchMedia('(prefers-color-scheme: dark)');
  const valid = value => ['light', 'dark', 'system'].includes(value);
  let preference = 'system';
  try {
    const saved = localStorage.getItem(key);
    if (valid(saved)) preference = saved;
  } catch { /* Keep theme switching available when storage is blocked. */ }

  function apply() {
    const dark = preference === 'dark' || (preference === 'system' && system.matches);
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', dark ? '#11131b' : '#f8f9fb');
    const button = document.getElementById('theme-toggle');
    if (button) {
      const modes = ['light', 'dark', 'system'];
      const labels = { light: '淺色', dark: '深色', system: '跟隨系統' };
      const next = modes[(modes.indexOf(preference) + 1) % modes.length];
      button.dataset.mode = preference;
      const label = `目前：${labels[preference]}；點擊切換為${labels[next]}`;
      button.setAttribute('aria-label', label);
      button.title = label;
    }
  }

  // Run before the stylesheet and body load to restore the first painted theme.
  apply();
  system.addEventListener('change', apply);
  window.addEventListener('storage', event => {
    if (event.key !== key && event.key !== null) return;
    preference = valid(event.newValue) ? event.newValue : 'system';
    apply();
  });
  document.addEventListener('DOMContentLoaded', () => {
    apply();
    document.documentElement.classList.add('theme-ready');
    document.getElementById('theme-toggle')?.addEventListener('click', () => {
      const modes = ['light', 'dark', 'system'];
      preference = modes[(modes.indexOf(preference) + 1) % modes.length];
      apply();
      try { localStorage.setItem(key, preference); } catch { /* Storage may be unavailable. */ }
    });
  });
})();
