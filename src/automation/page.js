async function worklinkPage(a) {
  const visible = e => !!e && !!(e.offsetWidth || e.offsetHeight || e.getClientRects().length) && getComputedStyle(e).visibility !== 'hidden';
  const norm = s => String(s || '').replace(/\s+/g, ' ').trim();
  const label = e => norm(e.getAttribute('aria-label') || (e.labels && [...e.labels].map(x => x.innerText).join(' ')) || e.getAttribute('placeholder') || e.innerText || e.getAttribute('title') || e.getAttribute('name')).slice(0, 100);
  const target = ref => {
    const entry = globalThis.__worklinkRefs;
    if (!entry || !ref.startsWith(entry.id + ':')) throw Error('ref_expired: take a new snapshot');
    const e = entry.items.get(ref);
    if (!e || !e.isConnected || !visible(e)) throw Error('ref_expired: element is no longer visible');
    return e;
  };
  if (a.op === 'snapshot') {
    const id = a.id;
    const all = [...document.querySelectorAll('a[href],button,input:not([type=hidden]),textarea,select,[role=button],[role=link],[contenteditable=true],[role=checkbox],[role=combobox]')].filter(visible).slice(0, 1000);
    globalThis.__worklinkRefs = {id, items: new Map()};
    const header = {tab: a.tab, title: document.title.slice(0, 100), url: location.href.slice(0, 256), revision: id};
    const controls = []; let used = JSON.stringify(header).length + 150; let next = null;
    all.forEach((e, i) => globalThis.__worklinkRefs.items.set(id + ':' + i, e));
    for (let i = a.offset; i < all.length; i++) {
      const e = all[i], ref = id + ':' + i;
      const item = {ref, role: e.getAttribute('role') || e.tagName.toLowerCase(), name: label(e)};
      if (e.disabled || e.getAttribute('aria-disabled') === 'true') item.disabled = true;
      if (e.tagName === 'INPUT' && e.type === 'password') item.password = true;
      const cost = JSON.stringify(item).length;
      if (controls.length >= a.items || used + cost > a.chars) { next = i; break; }
      controls.push(item); used += cost;
    }
    const root = document.querySelector('main,[role=main],article') || document.body;
    const summary = norm(root ? root.innerText : '').slice(0, Math.max(0, Math.min(600, a.chars - used - 100)));
    return {...header, controls, summary, next_offset: next, total_controls: all.length, source_truncated: all.length >= 1000};
  }
  if (a.op === 'point' || a.op === 'focus') {
    const e = target(a.ref);
    if (e.disabled || e.readOnly || e.getAttribute('aria-disabled') === 'true') throw Error('element_disabled');
    if (a.op === 'focus') {
      if (a.editable && !(e.isContentEditable || ['INPUT', 'TEXTAREA'].includes(e.tagName))) throw Error('not_editable');
      e.focus(); return {focused: document.activeElement === e};
    }
    e.scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'});
    await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
    const b = e.getBoundingClientRect(), x = b.left + b.width / 2, y = b.top + b.height / 2;
    const top = document.elementFromPoint(x, y);
    if (!top || !(top === e || e.contains(top))) throw Error('element_obscured: another element covers the target');
    return {x, y};
  }
  if (a.op === 'text') {
    const e = a.selector ? document.querySelector(a.selector) : document.querySelector('main,[role=main],article') || document.body;
    if (!e) throw Error('selector_not_found');
    return {text: String(e.innerText || '').slice(0, 2097152), source_truncated: String(e.innerText || '').length > 2097152};
  }
  if (a.op === 'wait') {
    const e = a.selector ? document.querySelector(a.selector) : document.body;
    return {ready: visible(e) && (!a.text || (e.innerText || '').includes(a.text))};
  }
  if (a.op === 'ask_probe') {
    const input = document.querySelector(a.input || '#prompt-textarea,textarea[data-testid="prompt-textarea"],textarea[placeholder]');
    const send = document.querySelector(a.send || '[data-testid="send-button"],button[aria-label="Send prompt"],button[aria-label="发送提示"]');
    const answers = [...document.querySelectorAll(a.answer || '[data-message-author-role="assistant"]')];
    const last = answers.at(-1);
    const generating = !!document.querySelector('[data-testid="stop-button"],[data-testid="stop-response-button"],[data-is-streaming="true"],button[aria-label="Stop generating"],button[aria-label="Stop response"],button[aria-label="停止生成"],button[aria-label="停止响应"]');
    return {input_ready: visible(input), send_ready: visible(send) && !send.disabled, draft: input ? norm(input.isContentEditable ? input.innerText : input.value) : '', count: answers.length, text: last ? String(last.innerText || '').slice(0, 2097152) : '', generating};
  }
  if (a.op === 'ask_refs') {
    const input = document.querySelector(a.input || '#prompt-textarea,textarea[data-testid="prompt-textarea"],textarea[placeholder]');
    const send = document.querySelector(a.send || '[data-testid="send-button"],button[aria-label="Send prompt"],button[aria-label="发送提示"]');
    if (!visible(input)) throw Error('login_or_input_required: open the browser and verify login');
    globalThis.__worklinkRefs = {id: a.id, items: new Map([[a.id + ':input', input], [a.id + ':send', send]])};
    return {input: a.id + ':input', send: a.id + ':send'};
  }
  throw Error('unsupported_page_operation');
}
