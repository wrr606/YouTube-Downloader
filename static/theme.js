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
    const select = document.getElementById('theme-select');
    if (select) select.value = preference;
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
    document.getElementById('theme-select')?.addEventListener('change', event => {
      preference = valid(event.target.value) ? event.target.value : 'system';
      apply();
      try { localStorage.setItem(key, preference); } catch { /* Storage may be unavailable. */ }
    });
  });
})();
