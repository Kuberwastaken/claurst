import { test } from 'node:test';
import * as assert from 'node:assert/strict';
import { extractText, parseLine } from './acpProtocol';

test('parseLine: blank and whitespace-only lines are null', () => {
  assert.equal(parseLine(''), null);
  assert.equal(parseLine('   \n'), null);
});

test('parseLine: malformed JSON is null, not a throw', () => {
  assert.equal(parseLine('{not json'), null);
});

test('parseLine: non-object JSON (e.g. a bare number) is null', () => {
  assert.equal(parseLine('42'), null);
});

test('parseLine: a response with a result', () => {
  const parsed = parseLine('{"jsonrpc":"2.0","id":1,"result":{"ok":true}}');
  assert.deepEqual(parsed, { kind: 'response', id: 1, result: { ok: true }, error: undefined });
});

test('parseLine: a response with an error', () => {
  const parsed = parseLine('{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"bad"}}');
  assert.deepEqual(parsed, {
    kind: 'response',
    id: 2,
    result: undefined,
    error: { code: -32600, message: 'bad' },
  });
});

test('parseLine: an incoming request (has id and method)', () => {
  const parsed = parseLine(
    '{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"a":1}}',
  );
  assert.deepEqual(parsed, {
    kind: 'request',
    id: 7,
    method: 'session/request_permission',
    params: { a: 1 },
  });
});

test('parseLine: a notification (method, no id)', () => {
  const parsed = parseLine('{"jsonrpc":"2.0","method":"session/update","params":{"b":2}}');
  assert.deepEqual(parsed, { kind: 'notification', method: 'session/update', params: { b: 2 } });
});

test('parseLine: id present but neither result/error/method is unroutable', () => {
  assert.equal(parseLine('{"jsonrpc":"2.0","id":1}'), null);
});

test('parseLine: null id is treated as absent (matches acp::RequestId::Null on a notification)', () => {
  const parsed = parseLine('{"jsonrpc":"2.0","id":null,"method":"session/update","params":{}}');
  assert.deepEqual(parsed, { kind: 'notification', method: 'session/update', params: {} });
});

test('extractText: text content block', () => {
  assert.equal(extractText({ type: 'text', text: 'hello' }), 'hello');
});

test('extractText: non-text content block returns empty string', () => {
  assert.equal(extractText({ type: 'image', data: 'abc', mimeType: 'image/png' }), '');
});

test('extractText: missing content returns empty string', () => {
  assert.equal(extractText(undefined), '');
  assert.equal(extractText(null), '');
});
