// The node half of the `computer_script` tool.
//
// Started once per session and kept alive, so a variable set in one call is
// still there in the next. It talks to the host over a loopback socket rather
// than over stdin: the host has to send code *and* answer host calls made
// while that code runs, and one pipe cannot carry both without the script
// blocking on the same descriptor the host is writing to.
//
// Framing is one JSON object per line, in both directions:
//   host -> here   {"type":"run","id":N,"code":"...","readOnly":bool}
//   here -> host   {"type":"host","id":N,"op":"click","args":{...}}
//   host -> here   {"type":"host_result","id":N,"ok":true,"value":...}
//   here -> host   {"type":"done","id":N,"ok":bool,"output":"...","value":...}

'use strict';

const net = require('net');

const PORT = Number(process.argv[2]);
const TOKEN = process.argv[3];

const socket = net.connect(PORT, '127.0.0.1');
socket.setEncoding('utf8');

/** Resolvers for host calls still in flight, by id. */
const pending = new Map();
let nextHostId = 1;

/** Whether the call being run right now refuses everything that writes. */
let readOnly = false;

/** What the running call has printed. */
let printed = [];

function send(message) {
  socket.write(JSON.stringify(message) + '\n');
}

/** Ask the host to do something, and wait for its answer. */
function host(op, args) {
  const id = nextHostId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    send({ type: 'host', id, op, args: args || {} });
  });
}

/** A host op that changes the machine rather than reading it. */
function writing(op, args) {
  if (readOnly) {
    return Promise.reject(new Error(`read_only is set, so ${op} is refused`));
  }
  return host(op, args);
}

// The surface user code sees. Reading is always allowed; anything that moves
// the pointer, presses a key or writes the clipboard goes through `writing`,
// so `read_only` closes every one of them in one place.
const api = {
  screenshot: (args) => host('screenshot', args),
  displays: () => host('displays'),
  windows: () => host('windows'),
  cursor: () => host('cursor'),
  move: (x, y) => writing('move', { x, y }),
  click: (x, y, button) => writing('click', { x, y, button: button || 'left' }),
  doubleClick: (x, y) => writing('double_click', { x, y }),
  drag: (x1, y1, x2, y2) => writing('drag', { x1, y1, x2, y2 }),
  type: (text) => writing('type', { text }),
  key: (combo) => writing('key', { combo }),
  scroll: (direction, amount) => writing('scroll', { direction, amount: amount || 3 }),
  clipboard: (text) =>
    text === undefined ? host('clipboard_read') : writing('clipboard_write', { text }),
  wait: (ms) => new Promise((r) => setTimeout(r, ms)),
};

for (const [name, fn] of Object.entries(api)) {
  globalThis[name] = fn;
}

// `print` rather than `console.log`, because the result is a value the host
// returns to the model and not a stream anyone is watching.
globalThis.print = (...parts) => {
  printed.push(parts.map((p) => (typeof p === 'string' ? p : JSON.stringify(p))).join(' '));
};
console.log = globalThis.print;

/**
 * Run one call's code.
 *
 * Wrapped in an async function so the body may use top-level `await`, and
 * evaluated with indirect `eval` so an assignment without `let` lands on
 * `globalThis` and is still there next call.
 */
async function run(code) {
  const body = `(async () => {\n${code}\n})()`;
  return await (0, eval)(body);
}

let buffer = '';
socket.on('data', (chunk) => {
  buffer += chunk;
  let cut;
  while ((cut = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, cut);
    buffer = buffer.slice(cut + 1);
    if (line.trim() === '') continue;
    let message;
    try {
      message = JSON.parse(line);
    } catch (error) {
      continue;
    }
    handle(message);
  }
});

function handle(message) {
  if (message.type === 'host_result') {
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.ok) {
      waiter.resolve(message.value);
    } else {
      waiter.reject(new Error(message.error || 'host call failed'));
    }
    return;
  }

  if (message.type === 'run') {
    readOnly = Boolean(message.readOnly);
    printed = [];
    run(message.code).then(
      (value) => {
        send({
          type: 'done',
          id: message.id,
          ok: true,
          output: printed.join('\n'),
          value: value === undefined ? null : value,
        });
      },
      (error) => {
        send({
          type: 'done',
          id: message.id,
          ok: false,
          output: printed.join('\n'),
          error: String((error && error.stack) || error),
        });
      },
    );
  }
}


socket.on('connect', () => {
  send({ type: 'hello', token: TOKEN });
});

socket.on('error', () => process.exit(1));
socket.on('close', () => process.exit(0));
