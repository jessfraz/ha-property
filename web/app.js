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

/** An opaque idempotency key also works on HA's ordinary local HTTP origin. */
function requestKey() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('');
}

/** @param {Error} error @param {string} fallback */
function errorSentence(error, fallback) {
  const message = error instanceof TypeError || error instanceof SyntaxError ? fallback : error.message;
  return /[.!?]$/.test(message) ? message : `${message}.`;
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
let lookup = null;
let selectedParcel = null;
let setupPreview = null;
let setupKey = null;
let setupGeneration = 0;
let lookupController = null;
let setupBusy = false;
let syncRun = null;
let syncPolling = false;
let syncController = null;
let pageActive = true;
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
      revision_conflict: path === 'property' ? 'The ledger changed. Refresh before saving the date again.' : 'The ledger changed. Refresh and create a new preview.',
      preview_mismatch: 'The import no longer matches its preview. Create a new preview.',
      idempotency_conflict: 'This import key belongs to another request. Create a new preview.',
      capacity: 'The ledger or import limit was reached. Split the import into smaller evidence files.',
      invalid_input: path === 'property/resolve'
        ? 'Enter a ten-digit parcel number or a full street address. Query wildcards are not supported.'
        : path === 'property' ? 'Enter a valid purchase date that is not in the future, or leave it blank.'
          : 'The evidence is invalid. Check its typed fields, property identity, amounts, and deadlines.',
      if_match_required: path === 'property' ? 'Refresh the property before saving its history date.' : 'The preview revision is required. Create a new preview.',
      database_unavailable: 'The ledger is unavailable. Existing evidence has not been replaced.',
      county_unavailable: 'The county lookup is unavailable. No property was confirmed.',
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
  if (refreshing) return false;
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
    return true;
  } catch (error) {
    shell.setStatus(current ? 'Refresh failed · showing previous snapshot' : 'Disconnected', current ? 'stale' : 'error');
    setNotice(element('notice'), `${error.message}${current ? ' The previous snapshot is still shown.' : ''}`, 'error');
    return false;
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
  return `FY ${identity.fiscal_year} · ${identity.sequence}${identity.source_version === 'unversioned' ? '' : ` · version ${identity.source_version}`}`;
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
  element('snapshot-status').textContent = summary.last_observed_at ? `Last evidence ${date(summary.last_observed_at)}` : 'No evidence yet';
  const unpaid = [...summary.installments].filter(item => item.state !== 'paid')
    .sort((a, b) => a.due_date.localeCompare(b.due_date));
  const next = unpaid[0];
  const nextCount = next ? unpaid.filter(item => item.due_date === next.due_date).length : 0;
  element('summary').replaceChildren(
    metric('Latest property balance', money(summary.known_balance_cents), summary.latest_balance
      ? `Observed ${date(summary.latest_balance.evidence.observed_at)}` : 'No property-wide county balance'),
    metric('Next unpaid installment', next ? date(next.due_date) : 'Unknown', next
      ? `${nextCount > 1 ? `${nextCount} installments due` : `${identityLabel(next.identity)} · installment ${next.number}`} · ${next.timezone}` : 'No confirmed open deadline'),
    metric('Needs attention', String(summary.needs_attention), `${unpaid.length} open installments · ${summary.needs_attention - unpaid.length} evidence flags`),
  );
  renderAttention();
  renderBills();
  renderAssessments();
  renderPayments();
  renderProperty();
  element('summary').hidden = !property.property || element('attention-view').hidden;
}

function renderAttention() {
  const host = element('attention-items');
  const { summary, property } = current;
  host.replaceChildren();
  if (!property.property) {
    const box = empty(host, 'Add a property', 'Find your county parcel and confirm the match to start the ledger.');
    const button = node('button', 'Find county parcel');
    button.type = 'button';
    button.addEventListener('click', openSetup);
    if (canWrite) box.append(button);
  } else if (summary.needs_attention === 0) {
    empty(host, 'Nothing needs attention', summary.last_observed_at ? `Evidence through ${date(summary.last_observed_at)}.` : 'No evidence has been imported yet.');
  }
  for (const installment of summary.installments.filter(item => item.state !== 'paid')) host.append(installmentCard(installment));
  if (property.property) renderAttentionGroups(host);
  const installments = element('installments');
  installments.replaceChildren(...summary.installments.map(installmentCard));
  element('deadline-section').hidden = !summary.installments.length;
}

/** @param {HTMLElement} host */
function renderAttentionGroups(host) {
  const groups = new Map();
  const unknownBalance = current.summary.issues.some(issue => issue.code === 'unknown_property_balance');
  for (const issue of current.summary.issues) {
    if (issue.code === 'unknown_balance') continue;
    let cause = issue.code;
    if (['missing_notice', 'assessment_without_notice'].includes(cause)
        || cause === 'coverage_gap' && issue.scope?.capability === 'notices') cause = 'notices';
    else if (cause === 'unknown_property_balance'
        || cause === 'coverage_gap' && issue.scope?.capability === 'balances' && unknownBalance) cause = 'balance';
    else if (['coverage_gap', 'coverage_incomplete', 'coverage_unknown'].includes(cause)) cause = 'coverage';
    const issues = groups.get(cause) ?? [];
    issues.push(issue);
    groups.set(cause, issues);
  }
  // Keep contradictory or overdue evidence prominent; collection limits follow
  // the concrete records and user actions they affect.
  const rank = cause => ['balance_conflict', 'balance_outdated', 'balance', 'notices', 'coverage'].indexOf(cause);
  const ordered = [...groups.entries()].sort(([a], [b]) => {
    const first = rank(a), second = rank(b);
    return (first < 0 ? 0 : first + 1) - (second < 0 ? 0 : second + 1);
  });
  for (const [cause, issues] of ordered) {
    const card = node('article', '', 'record issue issue-group');
    const heading = node('div', '', 'record-heading');
    const billIssues = issues.filter(issue => issue.scope?.kind === 'bill');
    let title;
    let description;
    if (cause === 'notices') {
      title = billIssues.length ? `${billIssues.length} ${billIssues.length === 1 ? 'bill needs a notice' : 'bills need notices'}` : 'Assessment notices needed';
      description = 'County records reference these tax years, but the notices are not on file.';
    } else if (cause === 'balance') {
      title = 'County balance not confirmed';
      description = 'The current property-wide balance is unknown. Check the county viewer or import a recent balance.';
    } else if (cause === 'coverage') {
      title = 'County coverage is limited';
      description = 'Some sources have a limited history or could not be checked.';
    } else {
      title = label(cause);
      description = issues[0].message;
    }
    heading.append(node('h3', title));
    card.append(heading, node('p', description));
    const rows = node('ul', '', 'issue-records');
    for (const issue of [...billIssues].sort((a, b) => b.scope.bill.fiscal_year - a.scope.bill.fiscal_year || a.scope.bill.sequence.localeCompare(b.scope.bill.sequence))) {
      const bill = current.summary.bills.find(item => sameBill(item.identity, issue.scope.bill));
      const row = node('li');
      row.append(node('span', identityLabel(issue.scope.bill)));
      if (bill) row.append(viewButton('View', () => openBill(bill), `View affected bill ${identityLabel(bill.identity)}`));
      rows.append(row);
    }
    if (rows.children.length) card.append(rows);
    const assessmentIssues = issues.filter(issue => issue.code === 'assessment_without_notice');
    if (assessmentIssues.length) {
      const years = [...new Set(current.assessments.map(item => item.observation.data.fiscal_year))]
        .filter(year => !billIssues.some(issue => issue.scope.bill.fiscal_year === year)
          && !current.summary.bills.some(bill => bill.identity.fiscal_year === year && !bill.superseded && !bill.missing_notice))
        .sort((a, b) => b - a);
      if (years.length) card.append(node('p', `Assessments without a notice: ${years.map(year => `FY ${year}`).join(', ')}.`, 'record-meta'));
    }
    const sources = issues.filter(issue => issue.scope?.kind === 'capability');
    if (sources.length) {
      card.append(node('p', sources.map(issue => {
        const coverage = current.summary.coverage.find(item => item.capability === issue.scope.capability);
        return `${label(issue.scope.capability)}: ${coverage ? label(coverage.status) : 'Unknown'}`;
      }).join(' · '), 'record-meta'));
    }
    const actions = node('div', '', 'actions');
    if (canWrite && ['notices', 'balance'].includes(cause)) actions.append(viewButton(cause === 'notices' ? 'Import notices' : 'Import balance', openImport));
    if (cause === 'notices' && assessmentIssues.length) actions.append(viewButton('View assessments', () => selectView('assessments')));
    if (!['notices', 'balance', 'coverage'].includes(cause)) {
      const detail = node('details');
      detail.append(node('summary', 'Affected evidence'));
      for (const issue of issues) detail.append(issueCard(issue, true));
      card.append(detail);
    }
    if (actions.children.length) card.append(actions);
    host.append(card);
  }
  if (current.summary.coverage.length) host.append(viewButton('View county coverage', () => {
    selectView('property');
    element('sources-title').setAttribute('tabindex', '-1');
    element('sources-title').focus();
  }));
}

/** @param {string} title @param {() => void} action @param {string} [accessibleLabel] */
function viewButton(title, action, accessibleLabel) {
  const button = node('button', title, 'quiet');
  button.type = 'button';
  if (accessibleLabel) button.setAttribute('aria-label', accessibleLabel);
  button.addEventListener('click', action);
  return button;
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
  for (const issue of installment.issues.filter(item => item.code !== 'unknown_balance')) card.append(node('p', issue.message, 'muted'));
  return card;
}

function renderBills() {
  const host = element('bills');
  const bills = [...current.summary.bills].sort((a, b) => b.identity.fiscal_year - a.identity.fiscal_year || a.identity.sequence.localeCompare(b.identity.sequence));
  host.replaceChildren();
  if (!bills.length) empty(host, 'No bills on file', 'Import a notice to see its balance and installments.');
  let year = null;
  let group;
  for (const bill of bills) {
    if (year !== bill.identity.fiscal_year) {
      year = bill.identity.fiscal_year;
      group = node('section', '', 'bill-year stack');
      group.append(node('h2', `FY ${year}`));
      host.append(group);
    }
    const card = node('article', '', `record bill-row${bill.missing_notice ? ' unknown' : ''}`);
    const heading = node('div', '', 'record-heading');
    const title = node('div');
    title.append(node('h3', bill.bill_kind ? `${label(bill.bill_kind)} bill` : 'Bill awaiting notice'), node('p', identityLabel(bill.identity), 'muted'));
    heading.append(title, tag(bill.superseded ? 'Superseded' : bill.missing_notice ? 'Missing notice' : 'Notice received', bill.missing_notice ? 'warning' : ''));
    card.append(heading);
    const balance = bill.latest_balance;
    const balanceLine = node('p', '', 'bill-balance');
    balanceLine.append(node('span', 'Bill balance', 'muted'), node('strong', balance ? money(balance.observation.data.amount_cents) : 'Unknown'));
    if (balance) balanceLine.append(node('span', `Observed ${date(balance.evidence.observed_at)}`, 'muted'));
    card.append(balanceLine);
    const actions = node('div', '', 'actions');
    const detail = node('button', 'View bill', 'quiet');
    detail.type = 'button';
    detail.setAttribute('aria-label', `View bill ${identityLabel(bill.identity)}`);
    detail.addEventListener('click', () => openBill(bill));
    actions.append(detail);
    card.append(actions);
    group.append(card);
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
  for (const observation of [...current.assessments].sort((a, b) => b.observation.data.fiscal_year - a.observation.data.fiscal_year)) {
    const assessment = observation.observation.data;
    const card = node('article', '', 'record');
    const heading = node('div', '', 'record-heading');
    heading.append(node('h3', `FY ${assessment.fiscal_year} assessment`), node('p', money(assessment.assessed_value_cents), 'amount'));
    card.append(heading);
    const details = node('details');
    details.append(node('summary', 'Assessment details and source'), node('p', assessment.description));
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(observation.evidence));
    details.append(evidence);
    card.append(details);
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
    if (canWrite) host.append(propertyDateForm(property));
    else facts(host, [['Bought on', property.bought_on ? date(property.bought_on) : 'Not set']]);
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(property.confirmation));
    const source = node('details');
    source.append(node('summary', 'Parcel confirmation source'), evidence);
    host.append(source);
  } else {
    host.append(node('p', 'Confirm the county and exact parcel against official county evidence once. Imports cannot silently change this binding.'));
  }
  element('property-setup').hidden = Boolean(property) || !canWrite;
  const coverage = element('coverage');
  coverage.replaceChildren();
  if (!current.summary.coverage.length) empty(coverage, 'County sources not checked', 'Check the county source to collect available records.');
  for (const item of current.summary.coverage) {
    const card = node('article', '', 'record');
    const heading = node('div', '', 'record-heading');
    heading.append(node('h3', label(item.capability)), tag(label(item.status), item.status === 'complete' ? 'ok' : item.status === 'partial' ? '' : 'warning'));
    const details = node('details');
    details.append(node('summary', 'Source details'), node('p', item.detail));
    card.append(heading, node('p', `Checked ${date(item.checked_at, true)}`, 'record-meta'), details);
    coverage.append(card);
  }
  updateSyncButton();
}

/** @param {object} property */
function propertyDateForm(property) {
  const form = node('form', '', 'property-date');
  const caption = node('label', 'Bought on (optional)');
  const input = document.createElement('input');
  input.type = 'date';
  input.id = 'property-bought-on';
  input.setAttribute('aria-describedby', 'property-history-help');
  input.value = property.bought_on ?? '';
  input.max = countyToday();
  caption.append(input);
  const help = node('p', 'Only include records from this date onward. Leave blank for all available history.', 'field-help');
  help.id = 'property-history-help';
  const save = node('button', 'Save date', 'quiet');
  save.type = 'submit';
  const notice = node('p');
  notice.hidden = true;
  form.append(caption, help, save, notice);
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const revision = current.property.revision;
    input.disabled = true;
    save.disabled = true;
    setNotice(notice, 'Saving…');
    try {
      await api('property', { method: 'PATCH', headers: { 'If-Match': `"${revision}"` }, body: JSON.stringify({ bought_on: input.value || null }) });
      if (await refresh()) setNotice(element('notice'), 'History date saved.');
      else setNotice(notice, 'Date saved. Refresh to load the updated ledger.');
    } catch (error) {
      setNotice(notice, errorSentence(error, 'The history date could not be saved.'), 'error');
    } finally {
      input.disabled = false;
      save.disabled = false;
    }
  });
  return form;
}

function countyToday() {
  return new Intl.DateTimeFormat('sv-SE', { timeZone: 'America/Los_Angeles' }).format(new Date());
}

/** @param {string} view */
function selectView(view) {
  const titles = { attention: 'Needs attention', bills: 'Bills', assessments: 'Assessments', property: 'Property' };
  if (!Object.hasOwn(titles, view)) return;
  for (const button of document.querySelectorAll('[data-view]')) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  for (const name of Object.keys(titles)) element(`${name}-view`).hidden = name !== view;
  element('view-title').textContent = titles[view];
  element('summary').hidden = view !== 'attention' || !current?.property.property;
}

function openImport() {
  if (!canWrite) return;
  element('import-dialog').showModal();
}

function openSetup() {
  if (!canWrite || current?.property.property) return;
  selectView('property');
  element('setup-address').focus();
}

function clearPropertyPreview() {
  setupPreview = null;
  setupKey = null;
  element('property-preview').hidden = true;
}

function updatePropertyReview() {
  element('review-property').disabled = setupBusy || !selectedParcel || lookup?.truncated
    || !element('confirm-parcel').checked || !element('setup-label').value.trim();
  element('apply-property').disabled = setupBusy || !setupPreview || !element('confirm-parcel').checked;
}

function setSetupBusy(busy) {
  setupBusy = busy;
  for (const id of ['setup-label', 'setup-bought-on', 'confirm-parcel', 'lookup-parcels']) element(id).disabled = busy;
  for (const radio of document.querySelectorAll('[name="lookup-method"]')) radio.disabled = busy;
  updateLookupMethod();
  for (const radio of element('parcel-options').querySelectorAll('input')) radio.disabled = busy || Boolean(lookup?.truncated);
  element('property-setup').setAttribute('aria-busy', String(busy));
  updatePropertyReview();
}

function invalidatePropertyLookup() {
  setupGeneration += 1;
  lookupController?.abort();
  lookupController = null;
  lookup = null;
  selectedParcel = null;
  clearPropertyPreview();
  element('confirm-parcel').checked = false;
  element('lookup-result').hidden = true;
  element('property-confirmation').hidden = true;
  element('parcel-options').replaceChildren();
  element('lookup-parcels').disabled = setupBusy;
  setNotice(element('setup-notice'), '');
  updatePropertyReview();
}

/** @param {object} result */
function showParcelCandidates(result) {
  if (!Array.isArray(result.candidates) || result.candidates.length > 100
      || !result.coverage || result.coverage.capability !== 'assessments'
      || typeof result.truncated !== 'boolean'
      || result.candidates.some(candidate => !candidate.key || candidate.key.jurisdiction !== '06037'
        || typeof candidate.key.parcel !== 'string' || !candidate.key.parcel
        || typeof candidate.address !== 'string' || !candidate.address
        || !Number.isSafeInteger(candidate.roll_year) || candidate.evidence?.source !== 'county')) {
    throw new Error('The county lookup did not return valid, county-bound parcel records. Nothing was confirmed.');
  }
  lookup = result;
  const host = element('parcel-options');
  host.replaceChildren();
  element('lookup-result').hidden = false;
  element('lookup-coverage').textContent = lookupNotice(result.coverage);
  if (result.truncated) {
    setNotice(element('setup-notice'), 'More parcels matched than can be shown. Narrow the street address, including the unit if applicable, and search again. This incomplete list cannot confirm a parcel.', 'error');
  } else if (!result.candidates.length) {
    setNotice(element('setup-notice'), 'No parcel match was returned. The property remains unconfirmed; this does not mean that the property does not exist or no tax is due. Check the current county record or try a more precise street address.');
  } else {
    setNotice(element('setup-notice'), `${result.candidates.length} candidate ${result.candidates.length === 1 ? 'parcel' : 'parcels'} returned. No parcel has been selected or confirmed.`);
  }
  for (const [index, candidate] of result.candidates.entries()) {
    const card = node('article', '', 'record parcel-option');
    const radio = document.createElement('input');
    radio.type = 'radio';
    radio.name = 'county-parcel';
    radio.id = `county-parcel-${index}`;
    radio.value = String(index);
    radio.disabled = result.truncated;
    const content = node('div');
    const caption = document.createElement('label');
    caption.htmlFor = radio.id;
    caption.append(node('strong', candidate.address), document.createTextNode(' '), node('span', `Parcel ${candidate.key.parcel}`, 'mono'));
    content.append(caption);
    facts(content, [['County', 'Los Angeles County, California'], ['County FIPS', candidate.key.jurisdiction], ['Published roll', String(candidate.roll_year)]]);
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(candidate.evidence, 'Parcel source'));
    content.append(evidence);
    radio.addEventListener('change', () => {
      setupGeneration += 1;
      selectedParcel = candidate;
      clearPropertyPreview();
      element('confirm-parcel').checked = false;
      updatePropertyReview();
    });
    card.append(radio, content);
    host.append(card);
  }
  element('parcel-candidates').hidden = !result.candidates.length;
  element('property-confirmation').hidden = result.truncated || !result.candidates.length;
  updatePropertyReview();
}

/** @param {object} coverage */
function lookupNotice(coverage) {
  const note = coverage.status === 'complete'
    ? 'These are candidate records, not a confirmed property. Verify the current official record before confirming.'
    : 'These published records are partial or may be stale. Verify the current official county record before confirming a parcel.';
  return `${note} ${coverage.detail} Checked ${date(coverage.checked_at, true)}.`;
}

function updateLookupMethod() {
  const parcel = document.querySelector('[name="lookup-method"]:checked').value === 'parcel';
  element('setup-parcel-field').hidden = !parcel;
  element('setup-address-field').hidden = parcel;
  element('setup-parcel').disabled = setupBusy || !parcel;
  element('setup-parcel').required = parcel;
  element('setup-address').disabled = setupBusy || parcel;
  element('setup-address').required = !parcel;
}

for (const radio of document.querySelectorAll('[name="lookup-method"]')) radio.addEventListener('change', () => {
  invalidatePropertyLookup();
  updateLookupMethod();
});
element('setup-parcel').addEventListener('input', invalidatePropertyLookup);
element('setup-address').addEventListener('input', invalidatePropertyLookup);
element('setup-bought-on').max = countyToday();
element('setup-bought-on').addEventListener('input', () => {
  setupGeneration += 1;
  clearPropertyPreview();
  updatePropertyReview();
});
element('setup-label').addEventListener('input', () => {
  setupGeneration += 1;
  clearPropertyPreview();
  updatePropertyReview();
});
element('confirm-parcel').addEventListener('change', () => {
  if (!element('confirm-parcel').checked) clearPropertyPreview();
  updatePropertyReview();
});
element('parcel-lookup-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (setupBusy || !canWrite || current?.property.property) return;
  invalidatePropertyLookup();
  const generation = setupGeneration;
  const controller = new AbortController();
  lookupController = controller;
  element('lookup-parcels').disabled = true;
  setNotice(element('setup-notice'), 'Looking up published county parcel records…');
  try {
    const query = document.querySelector('[name="lookup-method"]:checked').value === 'parcel'
      ? { parcel: element('setup-parcel').value.trim() }
      : { address: element('setup-address').value.trim() };
    const result = await api('property/resolve', { method: 'POST', body: JSON.stringify(query), signal: controller.signal });
    if (generation !== setupGeneration || current?.property.property || !canWrite) return;
    showParcelCandidates(result);
  } catch (error) {
    if (generation !== setupGeneration || error.name === 'AbortError') return;
    setNotice(element('setup-notice'), `${errorSentence(error, 'The county lookup could not be read.')} Your search is still entered; no property was confirmed.`, 'error');
  } finally {
    if (lookupController === controller) {
      lookupController = null;
      element('lookup-parcels').disabled = setupBusy;
    }
  }
});
element('review-property').addEventListener('click', async () => {
  if (setupBusy || !selectedParcel || !lookup || lookup.truncated || !element('confirm-parcel').checked || current?.property.property) return;
  const generation = setupGeneration;
  const input = {
    property: { key: selectedParcel.key, label: element('setup-label').value.trim(), bought_on: element('setup-bought-on').value || null, confirmed_at: new Date().toISOString(), confirmation: selectedParcel.evidence },
    observations: [], coverage: [lookup.coverage],
  };
  setSetupBusy(true);
  clearPropertyPreview();
  try {
    const result = await api('imports/preview', { method: 'POST', body: JSON.stringify(input) });
    if (generation !== setupGeneration || current?.property.property) return;
    setupPreview = result;
    setupKey = requestKey();
    const host = element('property-preview-content');
    host.replaceChildren(node('p', 'This confirms the property binding only. It does not establish a tax balance, settle a bill, or change an existing calendar.'));
    facts(host, [['Property label', result.input.property.label], ['Street address', selectedParcel.address], ['County', 'Los Angeles County, California'], ['County FIPS', result.input.property.key.jurisdiction], ['Selected parcel', result.input.property.key.parcel], ['Bought on', result.input.property.bought_on ? date(result.input.property.bought_on) : 'Not set'], ['Published roll', String(selectedParcel.roll_year)]]);
    host.append(node('p', `Preview hash: ${result.hash}`, 'mono'));
    const evidence = node('ul', '', 'evidence-list');
    evidence.append(evidenceItem(result.input.property.confirmation, 'County parcel confirmation'));
    host.append(evidence);
    host.append(node('p', lookupNotice(lookup.coverage), 'notice'));
    element('property-preview').hidden = false;
    element('property-preview-title').setAttribute('tabindex', '-1');
    element('property-preview-title').focus();
    setNotice(element('setup-notice'), 'Review the exact county, parcel and source below. Nothing has been saved yet.');
  } catch (error) {
    clearPropertyPreview();
    setNotice(element('setup-notice'), `${errorSentence(error, 'The property preview could not be read.')} No property was confirmed.`, 'error');
  } finally { setSetupBusy(false); }
});
element('apply-property').addEventListener('click', async () => {
  if (setupBusy || !setupPreview || !setupKey || !element('confirm-parcel').checked || current?.property.property) return;
  setSetupBusy(true);
  try {
    const result = await api('imports', { method: 'POST', headers: { 'If-Match': `"${setupPreview.revision}"` }, body: JSON.stringify({ preview: setupPreview, if_match: setupPreview.revision, idempotency_key: setupKey }) });
    const expected = setupPreview.input.property.key;
    const verified = await refresh();
    if (verified && current.property.property?.key.jurisdiction === expected.jurisdiction && current.property.property?.key.parcel === expected.parcel) {
      clearPropertyPreview();
      setNotice(element('notice'), 'Property confirmed. County sources can now be checked.');
      element('binding').setAttribute('tabindex', '-1');
      element('binding').focus();
    } else {
      setNotice(element('setup-notice'), `Confirmation was saved at revision ${result.revision}. Refresh to verify the current property before continuing.`, 'error');
    }
  } catch (error) {
    // Preserve the exact preview and key after an uncertain write. Only an
    // explicit retry can resubmit that same reviewed confirmation.
    setNotice(element('setup-notice'), `${errorSentence(error, 'The confirmation response could not be read.')} Check the current property before retrying this exact confirmation.`, 'error');
  } finally { setSetupBusy(false); }
});
element('setup-import').addEventListener('click', openImport);

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
    previewKey = requestKey();
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

function updateSyncButton() {
  const button = element('sync');
  button.disabled = syncPolling || !current?.property.property || !canWrite;
  button.textContent = syncRun?.status === 'running' ? 'Resume county check' : 'Check county source';
}

/** @param {object} run @param {string} [expected] */
function validateSyncRun(run, expected) {
  if (typeof run.id !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(run.id)
      || (expected && run.id !== expected)
      || !['running', 'succeeded', 'partial', 'unavailable', 'unsupported', 'interrupted'].includes(run.status)) {
    throw new Error('Property returned an invalid county-check receipt. Refresh before continuing.');
  }
  return run;
}

/** @param {AbortSignal} signal */
function waitForSync(signal) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { signal.removeEventListener('abort', abort); resolve(); }, 750);
    function abort() {
      clearTimeout(timer);
      signal.removeEventListener('abort', abort);
      reject(new DOMException('County check display closed.', 'AbortError'));
    }
    if (signal.aborted) abort();
    else signal.addEventListener('abort', abort, { once: true });
  });
}

element('sync').addEventListener('click', async () => {
  if (syncPolling || !canWrite || !current?.property.property || !pageActive) return;
  syncPolling = true;
  const controller = new AbortController();
  syncController = controller;
  let timedOut = false;
  const deadlineTimer = setTimeout(() => { timedOut = true; controller.abort(); }, 120000);
  updateSyncButton();
  element('sync-status').textContent = syncRun?.status === 'running' ? 'Reading the existing county check…' : 'Starting a read-only county check…';
  try {
    if (!syncRun || syncRun.status !== 'running') syncRun = validateSyncRun(await api('sync', { method: 'POST', signal: controller.signal }));
    const id = syncRun.id;
    const deadline = Date.now() + 120000;
    while (syncRun.status === 'running' && Date.now() < deadline) {
      element('sync-status').textContent = `County check is still running. Run ${id}. Existing evidence remains available.`;
      await waitForSync(controller.signal);
      syncRun = validateSyncRun(await api(`sync/${encodeURIComponent(id)}`, { signal: controller.signal }), id);
    }
    if (syncRun.status === 'running') {
      element('sync-status').textContent = `County check is still running. Resume run ${id} to read its status; no new check will be submitted.`;
      return;
    }
    const outcome = syncRun.status === 'succeeded'
      ? 'County check completed. Review the updated evidence and coverage below.'
      : syncRun.status === 'partial'
        ? 'County check partially completed. Review each source’s coverage; unavailable sources are not empty results.'
        : `County check ${label(syncRun.status).toLowerCase()}. Unavailable sources are not empty results; existing evidence is preserved.`;
    element('sync-status').textContent = `County result received. Refreshing the ledger view… Run ${id}.`;
    const verified = await refresh();
    element('sync-status').textContent = `${outcome} Run ${id}.${verified ? '' : ' The ledger view has not refreshed; the previous snapshot may still be shown.'}`;
  } catch (error) {
    if (!pageActive) return;
    if (error.name === 'AbortError' && timedOut) {
      element('sync-status').textContent = syncRun?.status === 'running'
        ? `County check is still running. Resume run ${syncRun.id} to read its status; no new check will be submitted.`
        : 'No county-check receipt was received in time. A check may have started; refresh before requesting another.';
      return;
    }
    if (error.name === 'AbortError') return;
    element('sync-status').textContent = syncRun?.status === 'running'
      ? `${errorSentence(error, 'The county-check status could not be read.')} Resume run ${syncRun.id} to read its status. No new check has been submitted.`
      : `${errorSentence(error, 'The county-check receipt could not be read.')} A check may have started; refresh before requesting another.`;
  } finally {
    clearTimeout(deadlineTimer);
    if (syncController === controller) syncController = null;
    syncPolling = false;
    updateSyncButton();
  }
});

window.addEventListener('pagehide', () => {
  pageActive = false;
  lookupController?.abort();
  syncController?.abort();
});
window.addEventListener('pageshow', () => { pageActive = true; updateSyncButton(); });

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
