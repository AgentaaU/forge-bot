// Meta refresh reloads the document. The browser restores the viewport, but
// each horizontally scrollable table has a separate offset.
(() => {
  const tables = document.querySelectorAll('.table-scroll');
  const key = 'forge-bot:table-scroll:' + location.pathname;
  // Browsers classify same-URL meta refreshes differently. The pathname in
  // the storage key and the saved query identify the view to restore.
  try {
    const saved = JSON.parse(sessionStorage.getItem(key));
    if (saved?.search === location.search) {
      for (const table of tables) table.scrollLeft = saved.offsets[table.id] ?? 0;
    }
  } catch {
    // Storage may be unavailable in restricted browsing contexts.
  }

  window.addEventListener('pagehide', () => {
    const offsets = Object.fromEntries(Array.from(tables, table => [table.id, table.scrollLeft]));
    try {
      sessionStorage.setItem(key, JSON.stringify({ search: location.search, offsets }));
    } catch {
      // Automatic refresh still works without storage.
    }
  });
})();
