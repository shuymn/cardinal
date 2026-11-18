'use strict';
// Rewrites each time into the viewer's local time with its age, and marks an observation time
// that has gone stale since the page was written. Each time is handled on its own: one that
// fails keeps the UTC text and the stale mark the page was written with.
{
  const staleAfterMs = Number(document.body.dataset.staleAfterMs);
  const now = Date.now();
  const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });
  const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
  const relative = (ms) => {
    const s = Math.round((ms - now) / 1000);
    const a = Math.abs(s);
    if (a < 60) return relativeFormat.format(s, 'second');
    if (a < 3600) return relativeFormat.format(Math.round(s / 60), 'minute');
    if (a < 86400) return relativeFormat.format(Math.round(s / 3600), 'hour');
    return relativeFormat.format(Math.round(s / 86400), 'day');
  };
  for (const node of document.querySelectorAll('time[data-ms]')) {
    try {
      const ms = Number(node.dataset.ms);
      const date = new Date(ms);
      if (!Number.isFinite(ms) || Number.isNaN(date.getTime())) continue;
      node.textContent = `${dateFormat.format(date)} (${relative(ms)})`;
      const marked = node.nextElementSibling && node.nextElementSibling.classList.contains('stale');
      if (node.classList.contains('observed') && !marked && now - ms > staleAfterMs) {
        const badge = document.createElement('span');
        badge.className = 'badge stale';
        badge.textContent = 'stale';
        node.after(' ', badge);
      }
    } catch {
      // The server-rendered text stays.
    }
  }
}
