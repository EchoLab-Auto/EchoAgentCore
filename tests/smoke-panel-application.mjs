// Normal management protocol smoke on an isolated loopback test server.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';

const port = Number(process.argv[2]);
assert(Number.isInteger(port) && port > 0 && port <= 65535, 'expected loopback port');
const url = `ws://127.0.0.1:${port}/ws`;

async function roundTrip() {
  const ws = new WebSocket(url);
  const module = `smoke.not-installed.${randomUUID()}`;
  const method = 'smoke.read';
  try {
    await new Promise((resolve, reject) => {
      let discovery = false;
      let completed = false;
      const finish = (error) => {
        if (completed) return;
        completed = true;
        clearTimeout(timer);
        error ? reject(error) : resolve();
      };
      const timer = setTimeout(() => finish(new Error('management application response timed out')), 8000);
      ws.onerror = () => finish(new Error('management websocket error'));
      ws.onclose = () => finish(new Error('management websocket closed before replies'));
      ws.onopen = () => ws.send(JSON.stringify({
        broadcast: true, frame: { type: 'command', payload: 'RequestTeamsList' },
      }));
      ws.onmessage = ({ data }) => {
        try {
          const root = JSON.parse(data);
          const frame = root.frame ?? root;
          if (frame.type !== 'event') return;
          if (!discovery && Array.isArray(frame.payload?.TeamsList?.teams)) {
            discovery = true;
            ws.send(JSON.stringify({ type: 'command', payload: {
              InvokeModuleOp: { module, method, payload: {} },
            } }));
          }
          const result = frame.payload?.ModuleInvokeResult;
          if (result?.module === module && result.method === method) {
            assert.equal(discovery, true);
            assert.equal(result.ok, false);
            assert.equal(typeof result.error, 'string');
            finish();
          }
        } catch (error) {
          finish(error);
        }
      };
    });
  } finally {
    if (ws.readyState !== WebSocket.CLOSED) {
      const closed = new Promise((resolve) => ws.addEventListener('close', resolve, { once: true }));
      ws.close();
      let deadline;
      try {
        await Promise.race([closed, new Promise((_, reject) => {
          deadline = setTimeout(() => reject(new Error('management websocket did not close')), 2000);
        })]);
      } finally {
        clearTimeout(deadline);
      }
    }
  }
}

await roundTrip();
await roundTrip();
console.log('initial connection and browser reconnect both returned TeamsList and ModuleInvokeResult');
