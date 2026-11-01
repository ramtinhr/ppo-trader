// Click a column title to sort by it (again to reverse); Shift+click adds it as the next sort
// column (again to reverse, a third time to remove it). Works on every table with a one-row
// header, including tables drawn again later. Tables inside [data-no-auto-sort] sort themselves.
(() => {
  const keysOf = new WeakMap();
  const collator = new Intl.Collator('en', { numeric: true, sensitivity: 'base' });
  const NUMBER = /^[+\-−]?\d[\d,]*(\.\d+)?/;
  const NUMERIC_TEXT = /^[+\-−]?\d[\d,]*(\.\d+)?\s*(%|h|s|usdt|btc|bps)?(\s*(\/|of)\s*[\d,.]+%?)?(\s*\(.*\))?$/i;
  let busy = false;

  const eligible = (table) => table && table.tHead && table.tHead.rows.length === 1 && table.tBodies.length > 0
    && !table.closest('[data-no-auto-sort]');

  function column(th) {
    let col = 0;
    for (let c = th.previousElementSibling; c; c = c.previousElementSibling) col += c.colSpan || 1;
    return col;
  }

  function cellAt(tr, col) {
    let at = 0;
    for (const td of tr.cells) {
      if (col < at + (td.colSpan || 1)) return at === col ? td : null;
      at += td.colSpan || 1;
    }
    return null;
  }

  function value(td) {
    if (!td) return null;
    if (td.dataset.sortValue !== undefined) {
      const n = Number(td.dataset.sortValue);
      return Number.isNaN(n) ? td.dataset.sortValue : n;
    }
    const text = td.textContent.trim();
    if (!text || text === '–' || text === '-') return null;
    if (NUMERIC_TEXT.test(text)) return Number(text.match(NUMBER)[0].replace(/,/g, '').replace('−', '-'));
    return text;
  }

  function compare(a, b) {
    if (a === null || b === null) return a === b ? 0 : a === null ? 1 : -1;
    if (typeof a === 'number' && typeof b === 'number') return a - b;
    if (typeof a === 'number') return -1;
    if (typeof b === 'number') return 1;
    return collator.compare(a, b);
  }

  function decorate(table) {
    for (const th of table.tHead.rows[0].cells) {
      th.style.cursor = 'pointer';
      th.style.userSelect = 'none';
      if (!th.title) th.title = 'Click to sort · Shift+click to sort by more columns';
      // Arrows drawn by a report's own single-column sort.
      for (const node of th.childNodes) {
        if (node.nodeType === 3) node.textContent = node.textContent.replace(/\s*[▲▼]\s*$/, '');
      }
    }
  }

  function indicate(table, keys) {
    for (const th of table.tHead.rows[0].cells) {
      th.querySelector('.ts-ind')?.remove();
      const i = keys.findIndex((k) => k.col === column(th));
      th.classList.toggle('ts-sorted', i >= 0);
      if (i >= 0) {
        th.insertAdjacentHTML('beforeend',
          `<span class="ts-ind">${keys[i].dir > 0 ? '▲' : '▼'}${keys.length > 1 ? i + 1 : ''}</span>`);
      }
    }
  }

  function apply(table) {
    const keys = keysOf.get(table) || [];
    decorate(table);
    indicate(table, keys);
    if (!keys.length) return;
    const body = table.tBodies[0];
    const rows = [...body.rows];
    rows.forEach((tr, i) => { if (tr.dataset.tsOrder === undefined) tr.dataset.tsOrder = i; });
    const values = new Map(rows.map((tr) => [tr, keys.map((k) => value(cellAt(tr, k.col)))]));
    rows.sort((x, y) => {
      const vx = values.get(x), vy = values.get(y);
      for (let i = 0; i < keys.length; i++) {
        if (vx[i] === null || vy[i] === null) {
          const c = compare(vx[i], vy[i]);
          if (c) return c;
          continue;
        }
        const c = compare(vx[i], vy[i]) * keys[i].dir;
        if (c) return c;
      }
      return x.dataset.tsOrder - y.dataset.tsOrder;
    });
    body.append(...rows);
  }

  function run(fn) {
    busy = true;
    try { fn(); } finally { observer.takeRecords(); busy = false; }
  }

  document.addEventListener('click', (event) => {
    const th = event.target.closest('thead th');
    const table = th && th.closest('table');
    if (!eligible(table)) return;
    event.stopPropagation();
    const col = column(th);
    let keys = keysOf.get(table) || [];
    const i = keys.findIndex((k) => k.col === col);
    if (event.shiftKey || event.ctrlKey || event.metaKey) {
      if (i < 0) keys = [...keys, { col, dir: -1 }];
      else if (keys[i].dir < 0) keys = keys.map((k, j) => (j === i ? { col, dir: 1 } : k));
      else keys = keys.filter((_, j) => j !== i);
    } else {
      keys = keys.length === 1 && i === 0 ? [{ col, dir: -keys[0].dir }] : [{ col, dir: -1 }];
    }
    keysOf.set(table, keys);
    run(() => apply(table));
  }, true);

  const observer = new MutationObserver((mutations) => {
    if (busy) return;
    const tables = new Set();
    for (const m of mutations) {
      const t = m.target.closest ? m.target.closest('table') : null;
      if (t) tables.add(t);
      for (const n of m.addedNodes) {
        if (n.nodeType !== 1) continue;
        if (n.tagName === 'TABLE') tables.add(n);
        n.querySelectorAll?.('table').forEach((x) => tables.add(x));
      }
    }
    run(() => tables.forEach((t) => { if (eligible(t)) apply(t); }));
  });

  const style = document.createElement('style');
  style.textContent = 'th.ts-sorted { color: var(--text, inherit); } .ts-ind { margin-left: 4px; font-size: 11px; color: var(--accent, #4f8cff); }';
  document.head.appendChild(style);
  run(() => document.querySelectorAll('table').forEach((t) => { if (eligible(t)) decorate(t); }));
  observer.observe(document.body, { childList: true, subtree: true });
})();
