// The administration page.
//
// It keeps nothing in the browser. The session is a cookie the page cannot
// read, and the CSRF token lives in a variable that dies with the tab, so
// closing the page leaves nothing behind for the next script to find.
//
// Every value that came from the database is placed with `textContent`. A
// provider named `<img onerror=...>` has to render as those characters, not as
// an element, and the page's own Content-Security-Policy is the second line of
// that defence rather than the first.

'use strict';

// What the server hands back from `/api/v1/me`. Required on every write the
// cookie authenticates; the CLI needs none, because a bearer token cannot be
// attached by another origin in the first place.
let csrf = '';
let me = null;
let oldestAuditId = null;

const $ = (id) => document.getElementById(id);

function say(text, bad) {
  const box = $('message');
  box.textContent = text;
  box.className = bad ? 'bad' : 'good';
  box.hidden = !text;
}

/// Call the API. Answers the parsed body, or null when there is none.
async function call(method, path, body) {
  const options = {
    method,
    credentials: 'same-origin',
    headers: {},
  };
  if (body !== undefined) {
    options.headers['content-type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  if (method !== 'GET') {
    options.headers['x-csrf-token'] = csrf;
  }

  const response = await fetch(path, options);
  if (response.status === 204 || response.status === 304) {
    return null;
  }

  let parsed = null;
  const text = await response.text();
  if (text) {
    try {
      parsed = JSON.parse(text);
    } catch (error) {
      parsed = null;
    }
  }

  if (!response.ok) {
    const detail = parsed && parsed.error ? parsed.error : response.status;
    throw new Error(String(detail));
  }
  return parsed;
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

function clear(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

function cell(row, text) {
  const td = document.createElement('td');
  td.textContent = text === null || text === undefined ? '' : String(text);
  row.appendChild(td);
  return td;
}

function actionCell(row, label, onClick) {
  const td = document.createElement('td');
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = label;
  button.addEventListener('click', onClick);
  td.appendChild(button);
  row.appendChild(td);
}

function option(select, value, label) {
  const node = document.createElement('option');
  node.value = value;
  node.textContent = label;
  select.appendChild(node);
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

let users = [];
let groups = [];
let providers = [];

async function loadProviders() {
  providers = (await call('GET', '/api/v1/admin/providers')) || [];
  const body = $('providers-table').querySelector('tbody');
  clear(body);

  for (const provider of providers) {
    const row = document.createElement('tr');
    cell(row, provider.name);
    cell(row, provider.protocol || '');
    cell(row, provider.api_base || '');
    const subjects = provider.assigned_users
      .map((id) => nameOfUser(id))
      .concat(provider.assigned_groups.map((id) => nameOfGroup(id)));
    // A provider nobody is assigned to is configured and unreachable, which
    // has to be visible without opening anything.
    cell(row, subjects.length ? subjects.join(', ') : 'nobody');
    actionCell(row, 'Delete', () => removeProvider(provider));
    body.appendChild(row);
  }

  const select = $('assign-provider');
  clear(select);
  for (const provider of providers) {
    option(select, provider.id, provider.name);
  }
}

function nameOfUser(id) {
  const found = users.find((user) => user.id === id);
  return found ? found.email : id;
}

function nameOfGroup(id) {
  const found = groups.find((group) => group.id === id);
  return found ? 'group ' + found.name : id;
}

async function removeProvider(provider) {
  await call('DELETE', '/api/v1/admin/providers/' + encodeURIComponent(provider.id));
  say('Removed ' + provider.name + '. Every installation that already took its key still holds it.');
  await loadProviders();
}

async function loadGroups() {
  groups = (await call('GET', '/api/v1/admin/groups')) || [];
  const body = $('groups-table').querySelector('tbody');
  clear(body);

  for (const group of groups) {
    const row = document.createElement('tr');
    cell(row, group.name);
    actionCell(row, 'Delete', async () => {
      await call('DELETE', '/api/v1/admin/groups/' + encodeURIComponent(group.id));
      say('Removed the group ' + group.name + '.');
      await refresh();
    });
    body.appendChild(row);
  }

  for (const id of ['membership-group']) {
    const select = $(id);
    clear(select);
    for (const group of groups) {
      option(select, group.id, group.name);
    }
  }
}

async function loadUsers() {
  users = (await call('GET', '/api/v1/admin/users')) || [];
  const body = $('users-table').querySelector('tbody');
  clear(body);

  for (const user of users) {
    const row = document.createElement('tr');
    cell(row, user.email);
    cell(row, user.is_admin ? 'administrator' : 'user');
    cell(row, user.disabled ? 'disabled' : 'active');
    body.appendChild(row);
  }

  const select = $('membership-user');
  clear(select);
  for (const user of users) {
    option(select, user.id, user.email);
  }
}

/// The one dropdown that mixes the two kinds, so an administrator does not
/// have to decide which list to look in before assigning.
function loadSubjects() {
  const select = $('assign-subject');
  clear(select);
  for (const user of users) {
    option(select, 'user:' + user.id, user.email);
  }
  for (const group of groups) {
    option(select, 'group:' + group.id, 'group ' + group.name);
  }
}

async function loadPolicy() {
  const stored = await call('GET', '/api/v1/admin/policy');
  $('policy-body').value = stored ? JSON.stringify(stored.settings, null, 2) : '';
}

async function loadAudit(more) {
  const query = more && oldestAuditId !== null ? '?limit=50&before=' + oldestAuditId : '?limit=50';
  const entries = (await call('GET', '/api/v1/admin/audit' + query)) || [];
  const body = $('audit-table').querySelector('tbody');
  if (!more) {
    clear(body);
  }

  for (const entry of entries) {
    const row = document.createElement('tr');
    cell(row, new Date(entry.at * 1000).toISOString().replace('T', ' ').slice(0, 19));
    cell(row, entry.subject || entry.actor_id || '');
    cell(row, entry.action);
    cell(row, entry.target || '');
    body.appendChild(row);
    oldestAuditId = entry.id;
  }

  $('audit-more').hidden = entries.length === 0;
}

async function refresh() {
  // Users and groups come first: the provider table shows who each provider
  // reaches by name, and the assignment dropdown lists both kinds.
  await loadUsers();
  await loadGroups();
  loadSubjects();
  await loadProviders();
  await loadPolicy();
  oldestAuditId = null;
  await loadAudit(false);
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

function show(session) {
  me = session;
  const signedIn = session !== null;
  $('signin').hidden = signedIn;
  $('who').hidden = !signedIn;
  $('who-email').textContent = signedIn ? session.email : '';
  $('admin').hidden = !signedIn || !session.is_admin;
  $('not-admin').hidden = !signedIn || session.is_admin;
}

/// Ask who we are. Answers null when nobody is signed in.
async function whoAmI() {
  const response = await fetch('/api/v1/me', { credentials: 'same-origin' });
  if (response.status === 401) {
    return null;
  }
  if (!response.ok) {
    throw new Error('the server answered ' + response.status);
  }
  return response.json();
}

async function start() {
  let session = null;
  try {
    session = await whoAmI();
  } catch (error) {
    say(String(error.message), true);
    return;
  }

  if (!session) {
    csrf = '';
    show(null);
    return;
  }

  csrf = session.csrf_token;
  show(session);
  if (session.is_admin) {
    try {
      await refresh();
    } catch (error) {
      say(String(error.message), true);
    }
  }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// Run a handler, showing whatever it refuses with rather than dropping it.
function guard(handler) {
  return async (event) => {
    if (event) {
      event.preventDefault();
    }
    say('');
    try {
      await handler();
    } catch (error) {
      say(String(error.message), true);
    }
  };
}

document.addEventListener('DOMContentLoaded', () => {
  for (const button of document.querySelectorAll('nav button')) {
    button.addEventListener('click', () => {
      for (const other of document.querySelectorAll('nav button')) {
        other.className = other === button ? 'active' : '';
        $('panel-' + other.dataset.panel).hidden = other !== button;
      }
    });
  }

  $('signin-form').addEventListener(
    'submit',
    guard(async () => {
      await call('POST', '/api/v1/login', {
        email: $('signin-email').value,
        password: $('signin-password').value,
      });
      // The password never stays in the form after it has been spent.
      $('signin-password').value = '';
      await start();
    })
  );

  $('logout').addEventListener(
    'click',
    guard(async () => {
      await call('POST', '/api/v1/logout');
      csrf = '';
      show(null);
    })
  );

  $('provider-form').addEventListener(
    'submit',
    guard(async () => {
      const models = $('provider-models')
        .value.split(',')
        .map((name) => name.trim())
        .filter((name) => name.length > 0);
      await call('POST', '/api/v1/admin/providers', {
        name: $('provider-name').value,
        protocol: $('provider-protocol').value || null,
        api_base: $('provider-base').value || null,
        api_key: $('provider-key').value,
        models,
      });
      // The key is written once and never read back, so the field is cleared
      // rather than left holding a credential in a form.
      $('provider-key').value = '';
      $('provider-name').value = '';
      say('Added the provider. Assign it before anyone can use it.');
      await loadProviders();
    })
  );

  const assignmentBody = () => {
    const [kind, id] = $('assign-subject').value.split(':');
    return {
      provider_id: $('assign-provider').value,
      subject_kind: kind,
      subject_id: id,
    };
  };

  $('assign-form').addEventListener(
    'submit',
    guard(async () => {
      await call('POST', '/api/v1/admin/assignments', assignmentBody());
      say('Assigned.');
      await loadProviders();
    })
  );

  $('unassign').addEventListener(
    'click',
    guard(async () => {
      await call('POST', '/api/v1/admin/assignments/remove', assignmentBody());
      say('Removed the assignment. A machine that already holds the key keeps it until the key changes.');
      await loadProviders();
    })
  );

  $('group-form').addEventListener(
    'submit',
    guard(async () => {
      await call('POST', '/api/v1/admin/groups', { name: $('group-name').value });
      $('group-name').value = '';
      await refresh();
    })
  );

  const membershipBody = () => ({
    user_id: $('membership-user').value,
    group_id: $('membership-group').value,
  });

  $('membership-form').addEventListener(
    'submit',
    guard(async () => {
      await call('POST', '/api/v1/admin/memberships', membershipBody());
      say('Added to the group.');
      await loadProviders();
    })
  );

  $('membership-remove').addEventListener(
    'click',
    guard(async () => {
      await call('POST', '/api/v1/admin/memberships/remove', membershipBody());
      say('Removed from the group.');
      await loadProviders();
    })
  );

  $('user-form').addEventListener(
    'submit',
    guard(async () => {
      await call('POST', '/api/v1/admin/users', {
        email: $('user-email').value,
        password: $('user-password').value,
        is_admin: $('user-admin').checked,
      });
      $('user-password').value = '';
      $('user-email').value = '';
      say('Opened the account.');
      await refresh();
    })
  );

  $('policy-form').addEventListener(
    'submit',
    guard(async () => {
      const raw = $('policy-body').value.trim();
      let parsed;
      try {
        parsed = JSON.parse(raw || '{}');
      } catch (error) {
        throw new Error('that is not valid JSON: ' + error.message);
      }
      const stored = await call('PUT', '/api/v1/admin/policy', parsed);
      say('Saved. Every installation applies it at ' + stored.checksum + '.');
    })
  );

  $('policy-clear').addEventListener(
    'click',
    guard(async () => {
      await call('DELETE', '/api/v1/admin/policy');
      $('policy-body').value = '';
      say('Removed. Each installation falls back to its own settings.');
    })
  );

  $('audit-more').addEventListener('click', guard(() => loadAudit(true)));

  start();
});
