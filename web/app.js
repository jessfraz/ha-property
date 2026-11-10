import { mountAppShell, setNotice } from './kit.js';

/** @param {string} id */
function element(id) {
  const found = document.getElementById(id);
  if (!(found instanceof HTMLElement)) throw new Error(`Missing app element: ${id}`);
  return found;
}

/** @param {string} tag @param {string} [text] @param {string} [className] */
function node(tag, text = '', className = '') {
  const result = document.createElement(tag);
  result.textContent = text;
  result.className = className;
  return result;
}

const refreshButton = node('button', 'Refresh', 'quiet');
refreshButton.type = 'button';
refreshButton.setAttribute('aria-label', 'Refresh Property');
const importButton = node('button', 'Import evidence');
importButton.type = 'button';
const shell = mountAppShell({
  name: 'Property', header: element('app-header'), footer: element('app-footer'),
  sourceUrl: 'https://github.com/jessfraz/ha-property',
  actions: [refreshButton, importButton], status: 'Connecting…',
});

let token = '';
let current = null;
let canWrite = false;
let refreshing = false;
let preview = null;
let previewKey = null;
let importBusy = false;
let fileRead = 0;
importButton.hidden = true;

/** @param {string} path @param {RequestInit} [options] */
async function api(path, options = {}) {
  const headers = new Headers(options.headers);
  headers.set('Accept', 'application/json');
  headers.set('X-Property-Request', '1');
  if (token) headers.set('Authorization', `Bearer ${token}`);
  if (options.body) headers.set('Content-Type', 'application/json');
  const response = await fetch(new URL(`v1/${path}`, document.baseURI), {
    ...options, headers, credentials: 'same-origin', cache: 'no-store',
  });
  if (response.status === 401) {
    element('login').hidden = false;
    throw new Error('Connect with an authorized API token to read the ledger.');
  }
  if (!response.ok) {
    if (response.status === 403) throw new Error('This connection cannot perform that action.');
    const messages = {
      binding_mismatch: 'This evidence belongs to another county or parcel. Check the confirmed property.',
      property_changed: 'An import cannot replace the confirmed property.',
      observation_conflict: 'An observation ID already has different evidence. Preserve the original and use a new observation ID.',
      revision_conflict: 'The ledger changed. Refresh and create a new preview.',
      preview_mismatch: 'The import no longer matches its preview. Create a new preview.',
      idempotency_conflict: 'This import key belongs to another request. Create a new preview.',
      capacity: 'The ledger or import limit was reached. Split the import into smaller evidence files.',
      invalid_input: 'The evidence is invalid. Check its typed fields, property identity, amounts, and deadlines.',
      if_match_required: 'The preview revision is required. Create a new preview.',
      database_unavailable: 'The ledger is unavailable. Existing evidence has not been replaced.',
    };
    try {
      const error = await response.json();
      if (Object.hasOwn(messages, error.error_code)) throw new Error(messages[error.error_code]);
    } catch (error) {
      if (Object.values(messages).includes(error.message)) throw error;
    }
    // Do not display arbitrary backend/provider bodies, credentials, or HTML.
    throw new Error(`Property could not complete the request (HTTP ${response.status}).`);
  }
  return response.json();
}

/** @param {string} path */
async function allPages(path) {
  const items = [];
  const seen = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: '200' });
    if (cursor) query.set('page_token', cursor);
    const page = await api(`${path}?${query}`);
    if (!Array.isArray(page.items)) throw new Error('Property returned an invalid evidence page.');
    items.push(...page.items);
    cursor = page.next_page;
    if (cursor !== null && typeof cursor !== 'string') throw new Error('Property returned an invalid evidence cursor.');
    if (cursor && seen.has(cursor)) throw new Error('Evidence paging did not advance. Refresh the ledger.');
    if (cursor) seen.add(cursor);
    if (items.length > 10000) throw new Error('This evidence view exceeded its limit. Use the CLI to inspect the ledger.');
  } while (cursor);
  return items;
}

async function loadSnapshot() {
  // Do not combine different ledger revisions into one apparently current view.
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const [property, summary, assessments, payments] = await Promise.all([
      api('property'), api('summary'), allPages('tax/assessments'), allPages('tax/payments'),
    ]);
    const after = await api('summary');
    if (property.revision === summary.revision && after.revision === summary.revision) {
      return { property, summary, assessments, payments };
    }
  }
  throw new Error('The ledger is changing. Refresh again for a consistent snapshot.');
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  refreshButton.disabled = true;
  shell.setStatus(current ? 'Refreshing…' : 'Connecting…');
  element('workspace').setAttribute('aria-busy', 'true');
  try {
    const [system, data] = await Promise.all([api('system'), loadSnapshot()]);
    current = data;
    canWrite = system.role === 'admin';
    element('login').hidden = true;
    element('token').value = '';
    element('workspace').hidden = false;
    importButton.hidden = !canWrite;
    shell.setVersion(system.version);
    shell.setStatus('Connected', 'ok');
    setNotice(element('notice'), '');
    render();
  } catch (error) {
    shell.setStatus(current ? 'Refresh failed · showing previous snapshot' : 'Disconnected', current ? 'stale' : 'error');
    setNotice(element('notice'), `${error.message}${current ? ' The previous snapshot is still shown.' : ''}`, 'error');
  } finally {
    refreshing = false;
    refreshButton.disabled = false;
    element('workspace').setAttribute('aria-busy', 'false');
  }
}

/** @param {number | null | undefined} cents */
function money(cents) {
  if (typeof cents === 'number' && !Number.isSafeInteger(cents)) return 'Amount outside display range';
  return cents === null || cents === undefined ? 'Unknown' : new Intl.NumberFormat('en-US', {
    style: 'currency', currency: 'USD',
  }).format(cents / 100);
}

/** @param {string | null | undefined} value @param {boolean} [time] @param {string} [timezone] */
function date(value, time = false, timezone = 'UTC') {
  if (!value) return 'Not observed';
  const parsed = new Date(value.length === 10 ? `${value}T12:00:00Z` : value);
  if (!Number.isFinite(parsed.getTime())) return 'Unknown date';
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium', ...(time ? { timeStyle: 'short' } : {}), timeZone: timezone,
  }).format(parsed);
}

/** @param {string} value */
function label(value) {
  const words = value.replaceAll('_', ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** @param {{fiscal_year:number,sequence:string,source_version:string}} identity */
function identityLabel(identity) {
  return `FY ${identity.fiscal_year} · ${identity.sequence} · version ${identity.source_version}`;
}

/** @param {object} first @param {object} second */
function sameBill(first, second) {
  return first.jurisdiction === second.jurisdiction && first.parcel === second.parcel
    && first.fiscal_year === second.fiscal_year && first.sequence === second.sequence
    && first.source_version === second.source_version;
}

/** @param {HTMLElement} host @param {string} title @param {string} text */
function empty(host, title, text) {
  const box = node('div', '', 'empty');
  box.append(node('h3', title), node('p', text));
  host.replaceChildren(box);
  return box;
}

/** @param {string} title @param {string} value @param {string} note */
function metric(title, value, note) {
  const card = node('div', '', 'metric');
  card.append(node('p', title, 'metric-label'), node('p', value, 'metric-value'), node('p', note, 'metric-note'));
  return card;
}

/** @param {HTMLElement} target @param {readonly (readonly [string, string])[]} rows */
function facts(target, rows) {
  const dl = node('dl');
  for (const [key, value] of rows) dl.append(node('dt', key), node('dd', value));
  target.append(dl);
}

/** @param {string} text @param {string} [kind] */
function tag(text, kind = '') {
  return node('span', text, `tag ${kind}`);
}

function render() {
  const { summary, property } = current;
  element('property-label').textContent = property.property?.label ?? 'Tax ledger';
  element('snapshot-status').textContent = `Revision ${summary.revision} · ${summary.last_observed_at ? `last evidence ${date(summary.last_observed_at)}` : 'no evidence yet'}`;
  const unpaid = [...summary.installments].filter(item => item.state !== 'paid')
    .sort((a, b) => a.due_date.localeCompare(b.due_date));
  const next = unpaid[0];
  const nextCount = next ? unpaid.filter(item => item.due_date === next.due_date).length : 0;
  element('summary').replaceChildren(
    metric('Latest property balance', money(summary.known_balance_cents), summary.latest_balance
      ? `Observed ${date(summary.latest_balance.evidence.observed_at)}` : 'No property-wide county balance'),
    metric('Next unpaid installment', next ? date(next.due_date) : 'Unknown', next
      ? `${nextCount > 1 ? `${nextCount} installments due` : `${identityLabel(next.identity)} · installment ${next.number}`} · ${next.timezone}` : 'No confirmed open deadline'),
    metric('Needs attention', String(summary.needs_attention), 'Open installments and unresolved evidence flags'),
  );
  renderAttention();
  renderBills();
  renderAssessments();
  renderPayments();
  renderProperty();
}

function renderAttention() {
  const host = element('attention-items');
  const { summary, property } = current;
  host.replaceChildren();
  if (!property.property) {
    const box = empty(host, 'Confirm a property to begin', 'Import county parcel evidence with the confirmed county FIPS and parcel. An address or ZIP code alone is not a property match.');
    const button = node('button', 'Import evidence');
    button.type = 'button';
    button.addEventListener('click', openImport);
    if (canWrite) box.append(button);
  } else if (summary.needs_attention === 0) {
    empty(host, 'No unresolved items in this snapshot', 'This describes the imported evidence, not a guarantee that every county notice has been received. Check source coverage below.');
  }
  for (const installment of summary.installments.filter(item => item.state !== 'paid')) host.append(installmentCard(installment));
  // The server counts open installments plus non-balance evidence flags.
  // Missing balance detail belongs to the affected installment, not a second
  // unscoped item which would misrepresent that attention count.
  for (const issue of summary.issues.filter(item => item.code !== 'unknown_balance')) host.append(issueCard(issue, true));
  const installments = element('installments');
  installments.replaceChildren(...summary.installments.map(installmentCard));
  element('deadline-section').hidden = !summary.installments.length;
}

/** @param {{code:string,message:string,observation_ids:string[],scope?:object}} issue @param {boolean} [actions] */
function issueCard(issue, actions = false) {
  const card = node('article', '', 'record issue');
  card.append(node('h3', label(issue.code)), node('p', issue.message));
  const scope = issue.scope;
  if (scope?.kind === 'bill' || scope?.kind === 'installment') {
    card.append(node('p', `${identityLabel(scope.bill)}${scope.kind === 'installment' ? ` · installment ${scope.number}` : ''}`, 'record-meta'));
    const bill = current.summary.bills.find(item => sameBill(item.identity, scope.bill));
    if (actions && bill) {
      const detail = node('button', 'View affected bill', 'quiet');
      detail.type = 'button';
      detail.addEventListener('click', () => openBill(bill));
      card.append(detail);
    }
  } else if (scope?.kind === 'capability') {
    card.append(node('p', `County source: ${label(scope.capability)}`, 'record-meta'));
    if (actions) {
      const sources = node('button', 'View county coverage', 'quiet');
      sources.type = 'button';
      sources.addEventListener('click', () => {
        selectView('property');
        element('sources-title').setAttribute('tabindex', '-1');
        element('sources-title').focus();
      });
      card.append(sources);
    }
  } else if (scope?.kind === 'property') {
    card.append(node('p', 'Confirmed property', 'record-meta'));
  }
  if (issue.observation_ids.length) card.append(node('p', `Evidence: ${issue.observation_ids.join(', ')}`, 'record-meta mono'));
  return card;
}

/** @param {object} installment */
function installmentCard(installment) {
  const card = node('article', '', 'record');
  const heading = node('div', '', 'record-heading');
  const text = node('div');
  text.append(node('h3', `Installment ${installment.number}`), node('p', identityLabel(installment.identity), 'muted'));
  heading.append(text, tag(label(installment.state), installment.state === 'paid' ? 'ok' : installment.state === 'contradiction' || installment.state === 'delinquent' ? 'warning' : ''));
  card.append(heading);
  facts(card, [
    ['Billed', money(installment.amount_cents)],
    ['County balance', money(installment.county_balance_cents)],
    ['Unpaid estimate', money(installment.estimated_remaining_cents)],
    ['Due', date(installment.due_date)],
    ['Delinquent after', `${date(installment.delinquent_at, true, installment.timezone)} · ${installment.timezone}`],
  ]);
  for (const issue of installment.issues) card.append(node('p', issue.message, 'muted'));
  return card;
}

function renderBills() {
  const host = element('bills');
  const bills = current.summary.bills;
  host.replaceChildren();
  if (!bills.length) empty(host, 'No bills imported', 'Import annual or supplemental notices to see their installments and evidence. No bill does not mean no tax is due.');
  for (const bill of bills) {
    const card = node('article', '', `record${bill.missing_notice ? ' unknown' : ''}`);
    const heading = node('div', '', 'record-heading');
    const title = node('div');
    title.append(node('h3', bill.bill_kind ? `${label(bill.bill_kind)} bill` : 'Bill awaiting notice'), node('p', identityLabel(bill.identity), 'muted'));
    heading.append(title, tag(bill.superseded ? 'Superseded' : bill.missing_notice ? 'Missing notice' : 'Notice received', bill.missing_notice ? 'warning' : ''));
    card.append(heading);
    const balance = bill.latest_balance;
    facts(card, [
      ['Latest bill balance', balance ? money(balance.observation.data.amount_cents) : 'Unknown'],
      ['Evidence observed', balance ? date(balance.evidence.observed_at) : 'No bill-scoped balance'],
    ]);
    const actions = node('div', '', 'actions');
    const detail = node('button', 'View bill', 'quiet');
    detail.type = 'button';
    detail.setAttribute('aria-label', `View bill ${identityLabel(bill.identity)}`);
    detail.addEventListener('click', () => openBill(bill));
    actions.append(detail);
    card.append(actions);
    host.append(card);
  }
}

/** @param {object} evidence @param {string} [kind] */
function evidenceItem(evidence, kind = '') {
  const item = node('li');
  if (kind) item.append(node('p', kind, 'eyebrow'));
  item.append(node('strong', evidence.document), node('p', `${label(evidence.source)} · observed ${date(evidence.observed_at, true)}`), node('p', `Effective ${date(evidence.effective_at, true)}`, 'muted'));
  if (evidence.url) {
    try {
      const destination = new URL(evidence.url);
      if (['https:', 'http:'].includes(destination.protocol) && !destination.username && !destination.password) {
        const link = node('a', 'Open evidence source ↗');
        link.href = destination.href;
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
        link.setAttribute('aria-label', 'Open evidence source (opens in a new tab)');
        item.append(link);
      }
    } catch { /* The document reference remains visible without an unsafe link. */ }
  }
  return item;
}

/** @param {object} bill */
function openBill(bill) {
  const dialog = element('bill-detail');
  const host = element('bill-content');
  element('bill-title').textContent = bill.bill_kind ? `${label(bill.bill_kind)} bill` : 'Bill awaiting notice';
  host.replaceChildren(node('p', identityLabel(bill.identity), 'muted'));
  if (bill.superseded) host.append(node('p', 'This version is superseded. Its evidence remains in the ledger; use the active adjusted version for current installments.', 'notice'));
  if (bill.missing_notice) host.append(node('p', 'The notice is missing. A payment or balance reference does not establish the full bill or its remaining installments.', 'notice'));
  for (const installment of bill.installments) host.append(installmentCard(installment));
  for (const issue of bill.issues) host.append(issueCard(issue));
  const evidence = node('section', '', 'section');
  evidence.append(node('h3', 'Evidence'));
  const list = node('ul', '', 'evidence-list');
  if (bill.notice) list.append(evidenceItem(bill.notice.evidence, 'Notice'));
  if (bill.latest_balance) list.append(evidenceItem(bill.latest_balance.evidence, 'Bill balance'));
  for (const observation of current.payments) {
    if (observation.observation.data.allocations.some(allocation => sameBill(allocation.bill, bill.identity))) {
      const payment = observation.observation.data;
      const item = evidenceItem(observation.evidence, 'Payment');
      item.append(node('p', `${payment.reference} · ${label(payment.status)}`), node('p', `Payment total: ${money(payment.amount_cents)}`));
      for (const allocation of payment.allocations.filter(item => sameBill(item.bill, bill.identity))) {
        item.append(node('p', `This bill · installment ${allocation.installment}: ${money(allocation.amount_cents)}`));
      }
      list.append(item);
    }
  }
  if (!list.children.length) list.append(node('li', 'No notice or scoped balance evidence is available.'));
  evidence.append(list);
  host.append(evidence);
  dialog.showModal();
}

function renderAssessments() {
  const host = element('assessments');
  host.replaceChildren();
  if (!current.assessments.length) empty(host, 'No assessment evidence', 'Assessed value is not a tax balance. Import assessment evidence separately from tax notices.');
  for (const observation of current.assessments) {
    const assessment = observation.observation.data;
    const card = node('article', '', 'record');
    const heading = node('div', '', 'record-heading');
    heading.append(node('h3', `FY ${assessment.fiscal_year} assessment`), node('p', money(assessment.assessed_value_cents), 'amount'));
    card.append(heading, node('p', assessment.description));
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(observation.evidence));
    card.append(evidence);
    host.append(card);
  }
}

function renderPayments() {
  const host = element('payments');
  host.replaceChildren();
  element('payments-section').hidden = !current.payments.length;
  for (const observation of current.payments) {
    const payment = observation.observation.data;
    const card = node('article', '', 'record');
    const heading = node('div', '', 'record-heading');
    heading.append(node('h3', label(payment.status)), node('p', money(payment.amount_cents), 'amount'));
    card.append(heading, node('p', payment.reference, 'muted mono'));
    if (!payment.allocations.length) card.append(node('p', 'No bill allocation is established. This payment closes no installment.'));
    for (const allocation of payment.allocations) {
      card.append(node('p', `${identityLabel(allocation.bill)} · installment ${allocation.installment}: ${money(allocation.amount_cents)}`));
    }
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(observation.evidence));
    card.append(evidence);
    host.append(card);
  }
}

function renderProperty() {
  const host = element('binding');
  const property = current.property.property;
  host.replaceChildren(node('h2', property ? property.label : 'No property confirmed'));
  if (property) {
    facts(host, [['County FIPS', property.key.jurisdiction], ['Parcel', property.key.parcel], ['Confirmed', date(property.confirmed_at, true)]]);
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(property.confirmation));
    host.append(evidence);
  } else {
    host.append(node('p', 'Confirm the county and exact parcel against official county evidence once. Imports cannot silently change this binding.'));
  }
  const coverage = element('coverage');
  coverage.replaceChildren();
  if (!current.summary.coverage.length) empty(coverage, 'County coverage is unknown', 'No source coverage has been checked. An empty result is not evidence of a zero balance.');
  for (const item of current.summary.coverage) {
    const card = node('article', '', 'record');
    const heading = node('div', '', 'record-heading');
    heading.append(node('h3', label(item.capability)), tag(label(item.status), item.status === 'complete' ? 'ok' : 'warning'));
    card.append(heading, node('p', item.detail), node('p', `Checked ${date(item.checked_at, true)}`, 'record-meta'));
    coverage.append(card);
  }
  element('sync').disabled = !property || !canWrite;
}

/** @param {string} view */
function selectView(view) {
  const titles = { attention: 'Needs attention', bills: 'Bills', assessments: 'Assessments', property: 'Property' };
  if (!Object.hasOwn(titles, view)) return;
  for (const button of document.querySelectorAll('[data-view]')) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  for (const name of Object.keys(titles)) element(`${name}-view`).hidden = name !== view;
  element('view-title').textContent = titles[view];
}

function openImport() {
  if (!canWrite) return;
  element('import-dialog').showModal();
}

function clearPreview() {
  preview = null;
  previewKey = null;
  element('import-preview').hidden = true;
  element('confirm-binding').checked = false;
  setNotice(element('import-notice'), '');
}

/** JSON numbers must remain exact when a reviewed import is sent back. */
function exactNumbers(value) {
  if (typeof value === 'number') return Number.isSafeInteger(value);
  if (Array.isArray(value)) return value.every(exactNumbers);
  if (value !== null && typeof value === 'object') return Object.values(value).every(exactNumbers);
  return true;
}

function setImportBusy(busy) {
  importBusy = busy;
  for (const id of ['preview-import', 'apply-import', 'import-input', 'import-file', 'close-import']) element(id).disabled = busy;
  element('import-dialog').setAttribute('aria-busy', String(busy));
}

element('import-input').addEventListener('input', () => {
  fileRead += 1;
  element('preview-import').disabled = importBusy;
  element('import-file').value = '';
  element('file-status').textContent = '';
  clearPreview();
});
element('import-file').addEventListener('change', async () => {
  const generation = ++fileRead;
  clearPreview();
  const file = element('import-file').files?.[0];
  if (!file) return;
  element('import-input').value = '';
  if (file.size > 4 * 1024 * 1024) {
    setNotice(element('import-notice'), 'Choose an evidence file smaller than 4 MB.', 'error');
    element('import-input').value = '';
    return;
  }
  element('preview-import').disabled = true;
  element('file-status').textContent = 'Reading evidence file…';
  try {
    const text = await file.text();
    if (generation !== fileRead) return;
    element('import-input').value = text;
    element('file-status').textContent = `${file.name} · ${(file.size / 1024).toFixed(1)} KB · ready to preview`;
  } catch {
    if (generation !== fileRead) return;
    element('import-input').value = '';
    setNotice(element('import-notice'), 'The evidence file could not be read.', 'error');
  } finally {
    if (generation === fileRead) element('preview-import').disabled = importBusy;
  }
});

element('import-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (importBusy) return;
  clearPreview();
  let input;
  if (new TextEncoder().encode(element('import-input').value).length > 4 * 1024 * 1024) {
    setNotice(element('import-notice'), 'Choose evidence smaller than 4 MB. Nothing was imported.', 'error');
    return;
  }
  try { input = JSON.parse(element('import-input').value); } catch {
    setNotice(element('import-notice'), 'The evidence is not valid JSON. Nothing was imported.', 'error');
    return;
  }
  if (!exactNumbers(input)) {
    setNotice(element('import-notice'), 'Amounts and other numbers must be exact whole integers within the supported range. Nothing was imported.', 'error');
    return;
  }
  setImportBusy(true);
  try {
    preview = await api('imports/preview', { method: 'POST', body: JSON.stringify(input) });
    previewKey = crypto.randomUUID();
    const host = element('preview-content');
    const summary = node('div', '', 'preview-summary');
    summary.append(node('p', `${preview.added} new observations · ${preview.duplicates} duplicates`), node('p', `Based on ledger revision ${preview.revision}`), node('p', `Preview hash: ${preview.hash}`, 'mono'));
    if (preview.added === 0) summary.append(node('p', 'No new observations will be added. Applying still confirms this property or updates imported coverage and records a receipt.'));
    host.replaceChildren(summary);
    facts(host, [['Property', preview.input.property.label], ['County FIPS', preview.input.property.key.jurisdiction], ['Parcel', preview.input.property.key.parcel], ['Resulting property balance', money(preview.summary.known_balance_cents)], ['Needs attention after import', String(preview.summary.needs_attention)]]);
    const evidence = node('section', '', 'section');
    evidence.append(node('h4', 'Evidence in this file'));
    const list = node('ul', '', 'evidence-list');
    const existing = new Set(preview.duplicate_ids);
    const seen = new Set();
    for (const observation of preview.input.observations) {
      const item = evidenceItem(observation.evidence);
      const { kind, data } = observation.observation;
      const duplicate = existing.has(observation.id) || seen.has(observation.id);
      seen.add(observation.id);
      item.prepend(node('p', duplicate ? 'Duplicate · already recorded or repeated in this file' : 'New observation', 'eyebrow'), node('p', `${label(kind)} · ${observation.id}`, 'evidence-identity'));
      if (kind === 'notice') {
        item.append(node('p', `${label(data.bill_kind)} · ${identityLabel(data.identity)}`));
        for (const installment of data.installments) item.append(node('p', `Installment ${installment.number}: ${money(installment.amount_cents)} · due ${date(installment.due_date)}`));
      } else if (kind === 'payment') {
        item.append(node('p', `${label(data.status)} · ${money(data.amount_cents)}`));
        if (!data.allocations.length) item.append(node('p', 'No allocation. This payment closes no installment.'));
        for (const allocation of data.allocations) item.append(node('p', `${identityLabel(allocation.bill)} · installment ${allocation.installment}: ${money(allocation.amount_cents)}`));
      } else if (kind === 'balance') {
        const scope = data.scope;
        item.append(node('p', `${label(scope.scope)} balance: ${money(data.amount_cents)}`));
        if (scope.scope === 'bill' || scope.scope === 'installment') item.append(node('p', `${identityLabel(scope.bill)}${scope.scope === 'installment' ? ` · installment ${scope.number}` : ''}`));
      } else if (kind === 'assessment') {
        item.append(node('p', `FY ${data.fiscal_year}: ${money(data.assessed_value_cents)}`));
      }
      list.append(item);
    }
    evidence.append(list);
    host.append(evidence);
    const confirm = !current?.property.property;
    element('confirm-binding-label').hidden = !confirm;
    element('apply-import').textContent = preview.added === 0 ? 'Apply reviewed import (no new observations)' : 'Apply reviewed import';
    element('apply-import').disabled = confirm;
    element('import-preview').hidden = false;
    element('preview-title').setAttribute('tabindex', '-1');
    element('preview-title').focus();
  } catch (error) {
    clearPreview();
    setNotice(element('import-notice'), `${error.message} Nothing was imported.`, 'error');
  } finally {
    setImportBusy(false);
    element('apply-import').disabled = !preview || (!current?.property.property && !element('confirm-binding').checked);
  }
});

element('confirm-binding').addEventListener('change', () => {
  element('apply-import').disabled = !preview || (!current?.property.property && !element('confirm-binding').checked);
});

element('apply-import').addEventListener('click', async () => {
  if (!preview || !previewKey || importBusy) return;
  if (!current?.property.property && !element('confirm-binding').checked) return;
  setImportBusy(true);
  try {
    const result = await api('imports', { method: 'POST', headers: { 'If-Match': `"${preview.revision}"` }, body: JSON.stringify({
      preview, if_match: preview.revision, idempotency_key: previewKey,
    }) });
    clearPreview();
    element('import-input').value = '';
    element('import-file').value = '';
    element('file-status').textContent = '';
    setNotice(element('import-notice'), `Import applied at revision ${result.revision}. ${result.added} new observations, ${result.duplicates} duplicates.`);
    await refresh();
  } catch (error) {
    // Keep the exact reviewed request and idempotency key for a safe retry if
    // transport failed after a write. A changed input always clears both.
    setNotice(element('import-notice'), `${error.message} Check the ledger before retrying.`, 'error');
  } finally {
    setImportBusy(false);
    element('apply-import').disabled = !preview || (!current?.property.property && !element('confirm-binding').checked);
  }
});

element('sync').addEventListener('click', async () => {
  const button = element('sync');
  button.disabled = true;
  element('sync-status').textContent = 'Checking county source availability…';
  try {
    const run = await api('sync', { method: 'POST' });
    element('sync-status').textContent = run.status === 'succeeded'
      ? 'County check completed. Review the updated coverage below.'
      : `County check ${label(run.status).toLowerCase()}. Unavailable sources are not empty results; existing evidence is preserved.`;
    await refresh();
  } catch (error) {
    element('sync-status').textContent = error.message;
  } finally { button.disabled = !current?.property.property || !canWrite; }
});

element('login').addEventListener('submit', event => {
  event.preventDefault();
  token = element('token').value;
  refresh();
});
refreshButton.addEventListener('click', refresh);
importButton.addEventListener('click', openImport);
for (const button of document.querySelectorAll('[data-view]')) button.addEventListener('click', () => selectView(button.dataset.view));
element('close-bill').addEventListener('click', () => element('bill-detail').close());
element('close-import').addEventListener('click', () => element('import-dialog').close());
element('import-dialog').addEventListener('cancel', event => { if (importBusy) event.preventDefault(); });
refresh();
