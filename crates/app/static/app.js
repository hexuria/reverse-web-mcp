// The UI is three doors at once:
//   accessibility  every control has a role and an accessible name (see index.html)
//   pixels         the same page
//   WebMCP         registerTool on navigator.modelContext when the browser has it,
//                  and always on window.__webmcp so a harness can call the same tools.

const api = {
  async call(method, path, body, door) {
    const headers = { 'content-type': 'application/json', 'x-door': door || 'ui' };
    const r = await fetch(path, { method, headers, body: body ? JSON.stringify(body) : undefined });
    const j = await r.json();
    if (!r.ok) throw new Error(j.error || r.statusText);
    return j;
  },
};

let customers = [];
let invoices = [];

function toast(msg) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (t.hidden = true), 2500);
}

function customerName(id) {
  const c = customers.find((c) => c.id === id);
  return c ? c.name : `#${id}`;
}

async function refresh() {
  const [state] = await Promise.all([api.call('GET', '/oracle/state')]);
  customers = state.customers;
  invoices = state.invoices;
  document.getElementById('seed').textContent = `seed ${state.seed}`;

  const ctb = document.getElementById('customers');
  ctb.innerHTML = '';
  for (const c of customers) {
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${c.id}</td><td>${c.name}</td><td>${c.email}</td>`;
    ctb.appendChild(tr);
  }
  const sel = document.getElementById('customer-select');
  sel.innerHTML = '';
  for (const c of customers) {
    const o = document.createElement('option');
    o.value = c.id;
    o.textContent = `${c.name} (#${c.id})`;
    sel.appendChild(o);
  }

  const itb = document.getElementById('invoices');
  itb.innerHTML = '';
  for (const inv of invoices) {
    const tr = document.createElement('tr');
    tr.innerHTML =
      `<td>${inv.id}</td><td>${customerName(inv.customer_id)}</td><td>${inv.amount_cents}</td>` +
      `<td><span class="status ${inv.status}">${inv.status}</span></td>` +
      `<td>${inv.approved ? 'yes' : 'no'}</td><td></td>`;
    const actions = tr.lastElementChild;
    const approve = document.createElement('button');
    approve.className = 'secondary';
    approve.textContent = 'Approve';
    approve.setAttribute('aria-label', `Approve invoice ${inv.id}`);
    approve.disabled = inv.approved;
    approve.onclick = () => approveInvoice(inv.id);
    const send = document.createElement('button');
    send.textContent = 'Send';
    send.setAttribute('aria-label', `Send invoice ${inv.id}`);
    send.disabled = inv.status !== 'draft';
    send.onclick = () => sendInvoice(inv.id);
    actions.append(approve, ' ', send);
    itb.appendChild(tr);
  }

  const m = location.pathname.match(/^\/ui\/invoices\/(\d+)$/);
  const detail = document.getElementById('detail');
  if (m) {
    const inv = invoices.find((i) => i.id === Number(m[1]));
    detail.innerHTML = inv
      ? `<h2>Invoice ${inv.id}</h2><p>${customerName(inv.customer_id)} · ${inv.amount_cents} cents · ${inv.status} · approved: ${inv.approved}</p>`
      : `<h2>Invoice ${m[1]}</h2><p>Not found.</p>`;
    if (inv) {
      const b = document.createElement('button');
      b.textContent = inv.approved ? 'Approved' : 'Approve';
      b.setAttribute('aria-label', `Approve invoice ${inv.id}`);
      b.disabled = inv.approved;
      b.onclick = () => approveInvoice(inv.id);
      detail.appendChild(b);
    }
  } else {
    detail.innerHTML = '';
  }

  if (state.chaos && state.chaos.ui_modal && !refresh._modalShown) {
    refresh._modalShown = true;
    const d = document.getElementById('modal');
    if (!d.open) d.showModal();
  }
}

async function approveInvoice(id) {
  // UI-only: the server refuses without X-UI.
  const r = await fetch(`/ui/approve/${id}`, { method: 'POST', headers: { 'x-ui': '1' } });
  const j = await r.json();
  if (!r.ok) throw new Error(j.error);
  toast(`Approved invoice ${id}`);
  await refresh();
  return j;
}

async function sendInvoice(id, key) {
  const headers = { 'x-door': 'ui' };
  if (key) headers['idempotency-key'] = key;
  const r = await fetch(`/api/invoices/${id}/send`, { method: 'POST', headers });
  const j = await r.json();
  if (!r.ok) {
    toast(`Send failed: ${j.error}`);
    throw new Error(j.error);
  }
  toast(`Sent invoice ${id}`);
  await refresh();
  return j;
}

document.getElementById('new-invoice').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  const j = await api.call('POST', '/api/invoices', {
    customer_id: Number(f.get('customer_id')),
    amount_cents: Number(f.get('amount_cents')),
  }, 'ui');
  toast(`Created invoice ${j.id}`);
  await refresh();
});

document.getElementById('modal-close').addEventListener('click', () => document.getElementById('modal').close());

// ---- WebMCP ----
// Same operations as the API, executed from the page with door=webmcp.
const webmcpTools = [
  {
    name: 'listCustomers',
    description: 'Find customers, optionally by exact name or by name prefix.',
    inputSchema: { type: 'object', properties: { name: { type: 'string' }, name_prefix: { type: 'string' } } },
    execute: async ({ name, name_prefix }) => api.call('GET', `/api/customers${name_prefix ? `?name_prefix=${encodeURIComponent(name_prefix)}` : name ? `?name=${encodeURIComponent(name)}` : ''}`, null, 'webmcp'),
  },
  {
    name: 'createInvoice',
    description: 'Create a draft invoice for a customer id.',
    inputSchema: { type: 'object', properties: { customer_id: { type: 'integer' }, amount_cents: { type: 'integer' }, idempotency_key: { type: 'string' } }, required: ['customer_id', 'amount_cents'] },
    execute: async (a) => {
      const headers = { 'content-type': 'application/json', 'x-door': 'webmcp' };
      if (a.idempotency_key) headers['idempotency-key'] = a.idempotency_key;
      const r = await fetch('/api/invoices', { method: 'POST', headers, body: JSON.stringify({ customer_id: a.customer_id, amount_cents: a.amount_cents }) });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error);
      refresh();
      return j;
    },
  },
  {
    name: 'sendInvoice',
    description: 'Email an invoice to its customer. Marks it sent.',
    inputSchema: { type: 'object', properties: { id: { type: 'integer' }, idempotency_key: { type: 'string' } }, required: ['id'] },
    execute: async (a) => {
      const headers = { 'x-door': 'webmcp' };
      if (a.idempotency_key) headers['idempotency-key'] = a.idempotency_key;
      const r = await fetch(`/api/invoices/${a.id}/send`, { method: 'POST', headers });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error);
      refresh();
      return j;
    },
  },
  {
    name: 'sendReceipt',
    description: 'Email a receipt for a paid invoice.',
    inputSchema: { type: 'object', properties: { id: { type: 'integer' }, idempotency_key: { type: 'string' } }, required: ['id'] },
    execute: async (a) => {
      const headers = { 'x-door': 'webmcp' };
      if (a.idempotency_key) headers['idempotency-key'] = a.idempotency_key;
      const r = await fetch(`/api/invoices/${a.id}/receipt`, { method: 'POST', headers });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error);
      refresh();
      return j;
    },
  },
  {
    name: 'createReport',
    description: 'Create a report over a list of invoice ids.',
    inputSchema: { type: 'object', properties: { invoice_ids: { type: 'array', items: { type: 'integer' } }, idempotency_key: { type: 'string' } }, required: ['invoice_ids'] },
    execute: async (a) => {
      const headers = { 'content-type': 'application/json', 'x-door': 'webmcp' };
      if (a.idempotency_key) headers['idempotency-key'] = a.idempotency_key;
      const r = await fetch('/api/reports', { method: 'POST', headers, body: JSON.stringify({ invoice_ids: a.invoice_ids }) });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error);
      return j;
    },
  },
  {
    name: 'listInvoices',
    description: 'List invoices, optionally for one customer id.',
    inputSchema: { type: 'object', properties: { customer_id: { type: 'integer' } } },
    execute: async ({ customer_id }) => api.call('GET', `/api/invoices${customer_id ? `?customer_id=${customer_id}` : ''}`, null, 'webmcp'),
  },
];

window.__webmcp = {
  list: () => webmcpTools.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })),
  call: async (name, args) => {
    const t = webmcpTools.find((t) => t.name === name);
    if (!t) throw new Error(`no tool ${name}`);
    return t.execute(args || {});
  },
};

(function registerWebMCP() {
  const el = document.getElementById('webmcp');
  const mc = navigator.modelContext;
  if (mc && typeof mc.registerTool === 'function') {
    for (const t of webmcpTools) {
      try { mc.registerTool(t); } catch (e) { console.warn('registerTool failed', t.name, e); }
    }
    el.textContent = `webmcp: native (${webmcpTools.length} tools)`;
  } else {
    el.textContent = `webmcp: shim (${webmcpTools.length} tools on window.__webmcp)`;
  }
})();

refresh();
const es = new EventSource('/events');
es.onmessage = () => refresh();
for (const k of ['invoice.created', 'invoice.sent', 'invoice.approved', 'payment.received', 'receipt.sent', 'report.created', 'customer.created']) {
  es.addEventListener(k, () => refresh());
}
