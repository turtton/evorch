import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { compareStreams, createRecordParser, inspectStream, runCli } from './inspect-client-hello.mjs';

const fixture = readFileSync(new URL('../../../../crates/providers/tests/fixtures/claude-transport/native-2.1.295-client-hello.bin', import.meta.url));
const u16 = value => { const b = Buffer.alloc(2); b.writeUInt16BE(value); return b; };
const vector = (bytes, width = 2) => Buffer.concat([width === 1 ? Buffer.from([bytes.length]) : u16(bytes.length), bytes]);
const extension = (type, bytes) => Buffer.concat([u16(type), vector(bytes)]);
const record = (bytes, type = 22) => Buffer.concat([Buffer.from([type, 3, 1]), u16(bytes.length), bytes]);

function hello({ seed = 1, sessionLength = 4, cipher = 0x1301, group = 29, keyLength = 32, unknown = Buffer.from('opaque-secret'), reverse = false, psk = null, extraExtensions = [] } = {}) {
  const extensions = [
    extension(10, vector(u16(group))),
    extension(51, vector(Buffer.concat([u16(group), vector(Buffer.alloc(keyLength, seed))]))),
    extension(65000, unknown),
    ...extraExtensions,
  ];
  if (reverse) extensions.reverse();
  if (psk) {
    const identities = Buffer.concat([vector(Buffer.alloc(psk.ticketLength ?? 8, psk.seed)), Buffer.alloc(4, psk.seed)]);
    const binders = vector(Buffer.alloc(psk.binderLength ?? 32, psk.seed), 1);
    extensions.push(extension(41, Buffer.concat([vector(identities), vector(binders)])));
  }
  const body = Buffer.concat([
    u16(0x0303), Buffer.alloc(32, seed), vector(Buffer.alloc(sessionLength, seed), 1),
    vector(u16(cipher)), Buffer.from([1, 0]), vector(Buffer.concat(extensions)),
  ]);
  const header = Buffer.alloc(4);
  header[0] = 1;
  header.writeUIntBE(body.length, 1, 3);
  return Buffer.concat([header, body]);
}

test('independent native fixture preserves its cipher, extension and key-share structure', () => {
  const inspected = inspectStream(fixture);
  assert.equal(inspected.byteLength, 1504);
  assert.deepEqual(inspected.records, [{ type: 22, version: 0x0301, length: 1499, offset: 0 }]);
  assert.equal(inspected.clientHellos.length, 1);
  const result = inspected.clientHellos[0];
  assert.equal(result.version, 0x0303);
  assert.equal(result.sessionLength, 32);
  assert.deepEqual(result.ciphers, [4865, 4866, 4867, 49195, 49199, 49196, 49200, 52393, 52392, 49161, 49171, 49162, 49172, 156, 157, 47, 53]);
  assert.deepEqual(result.extensions.map(e => e.type), [0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43]);
  assert.deepEqual(result.extensions.find(e => e.type === 51).detail.entries, [{ group: 4588, length: 1216 }, { group: 29, length: 32 }]);
  assert.equal(result.normalizedSha256, '8845ac2401a951ffc4acef2824c3422124c7883e0c9bc4b5f90d3c6be05da2f9');
});

test('only transient random, session ID and key bytes are normalized by default', () => {
  const original = record(hello());
  const unchanged = Buffer.from(original);
  const changed = compareStreams(original, record(hello({ seed: 127 })));
  assert.equal(changed.normalizedHandshakeEqual, true);
  assert.equal(changed.recordBoundariesEqual, true);
  for (const options of [
    { cipher: 0x1302 }, { group: 23 }, { keyLength: 31 }, { sessionLength: 3 },
    { unknown: Buffer.from('opaque-secreu') }, { reverse: true },
    { unknown: Buffer.from('opaque-secret-longer') },
  ]) {
    assert.equal(compareStreams(original, record(hello(options))).normalizedHandshakeEqual, false);
  }
  assert.deepEqual(original, unchanged);
});

test('TCP chunks, split handshake headers, multiple records and multiple hellos reassemble', () => {
  const message = hello();
  const fragmented = Buffer.concat([
    record(message.subarray(0, 2)), record(message.subarray(2, 79)),
    record(Buffer.concat([message.subarray(79), message])),
    record(Buffer.from([1]), 20), record(Buffer.from('encrypted'), 23),
  ]);
  const parser = createRecordParser();
  for (let offset = 0, index = 0; offset < fragmented.length; index++) {
    const length = [1, 2, 3, 5, 13, 64][index % 6];
    parser.push(fragmented.subarray(offset, offset + length));
    offset += length;
  }
  const result = parser.finish();
  assert.deepEqual(result, inspectStream(fragmented));
  assert.equal(result.clientHellos.length, 2);
  assert.equal(result.clientHellos[0].normalizedSha256, result.clientHellos[1].normalizedSha256);
  assert.deepEqual(result.clientHellos[0].recordBoundaries.map(r => r.index), [0, 1, 2]);
  assert.equal(result.clientHellos[1].recordBoundaries[0].offsetInRecord, message.length - 79);
  const single = record(message);
  const split = Buffer.concat([record(message.subarray(0, 2)), record(message.subarray(2))]);
  const comparison = compareStreams(single, split);
  assert.equal(comparison.normalizedHandshakeEqual, true);
  assert.equal(comparison.recordBoundariesEqual, false);
  assert.equal(compareStreams(single, record(Buffer.concat([message, message]))).normalizedHandshakeEqual, false);
});

test('incomplete records, handshake frames and malformed nested vectors fail closed', () => {
  const good = record(hello());
  const oddCiphers = hello();
  // Handshake header, version, random, session length + four bytes precede cipher length.
  oddCiphers.writeUInt16BE(1, 43);
  const badExtensionLength = hello();
  const firstExtensionLengthOffset = 53;
  badExtensionLength.writeUInt16BE(65535, firstExtensionLengthOffset);
  const badHandshakeLength = hello();
  badHandshakeLength.writeUIntBE(badHandshakeLength.length, 1, 3);
  const invalidInputs = [
    Buffer.from([22, 3, 1, 255, 255]), Buffer.from([255, 3, 1, 0, 0]),
    good.subarray(0, 3), good.subarray(0, good.length - 1),
    record(hello().subarray(0, 2)), record(hello().subarray(0, hello().length - 1)),
    record(oddCiphers), record(badExtensionLength), record(badHandshakeLength),
    Buffer.concat([record(hello().subarray(0, 2)), record(Buffer.from([1]), 20)]),
    record(hello({ extraExtensions: [extension(43, Buffer.from([3, 3, 4, 3]))] })),
    record(hello({ extraExtensions: [extension(16, Buffer.from([0, 2, 5, 1]))] })),
    record(hello({ extraExtensions: [extension(51, Buffer.from([0, 0]))] })),
    record(hello({ psk: { seed: 1, binderLength: 31 } })),
    Buffer.from('{"requests":["sensitive"]}'), Buffer.alloc(0),
  ];
  for (const bytes of invalidInputs) assert.throws(() => inspectStream(bytes));
});

test('PSK normalization is opt-in and preserves identity/binder shapes', () => {
  const first = record(hello({ psk: { seed: 1 } }));
  const second = record(hello({ psk: { seed: 2 } }));
  assert.equal(compareStreams(first, second).normalizedHandshakeEqual, false);
  const options = { normalizePsk: true };
  assert.equal(compareStreams(first, second, options).normalizedHandshakeEqual, true);
  for (const psk of [{ seed: 2, ticketLength: 9 }, { seed: 2, binderLength: 33 }]) {
    assert.equal(compareStreams(first, record(hello({ psk })), options).normalizedHandshakeEqual, false);
  }
  const detail = inspectStream(first).clientHellos[0].extensions.find(e => e.type === 41).detail;
  assert.deepEqual(detail, { identities: [{ identityLength: 8, ageLength: 4 }], binderLengths: [32] });
});

function cli(args, inputs) {
  let stdout = '', stderr = '';
  const code = runCli(args, {
    readFile: path => { if (!(path in inputs)) throw new Error('sensitive OS details'); return inputs[path]; },
    stdout: { write: text => { stdout += text; } },
    stderr: { write: text => { stderr += text; } },
  });
  return { code, stdout, stderr };
}

test('CLI reports safe summaries, difference/usage codes and sanitized errors', () => {
  const first = record(hello({ psk: { seed: 0x5a } }));
  const inputs = { first, equal: first, changed: record(hello({ psk: { seed: 0x5b } })) };
  const inspected = cli(['first'], inputs);
  assert.equal(inspected.code, 0);
  assert.equal(inspected.stderr, '');
  assert.equal(JSON.parse(inspected.stdout).normalization.preSharedKey, false);
  for (const secret of ['opaque-secret', Buffer.alloc(8, 0x5a).toString('hex'), first.toString('base64')]) {
    assert.equal(inspected.stdout.includes(secret), false);
  }
  assert.equal(cli(['first', 'equal'], inputs).code, 0);
  const changed = cli(['first', 'changed'], inputs);
  assert.equal(changed.code, 1);
  assert.equal(JSON.parse(changed.stdout).normalizedHandshakeEqual, false);
  assert.equal(JSON.parse(changed.stdout).recordBoundariesEqual, true);
  assert.equal(cli(['first', 'changed', '--normalize-psk'], inputs).code, 0);
  const split = record(hello().subarray(0, 2));
  const remainder = record(hello().subarray(2));
  const boundaryChange = cli(['first', 'second'], { first: record(hello()), second: Buffer.concat([split, remainder]) });
  assert.equal(boundaryChange.code, 1);
  assert.equal(JSON.parse(boundaryChange.stdout).normalizedHandshakeEqual, true);
  assert.equal(JSON.parse(boundaryChange.stdout).recordBoundariesEqual, false);
  for (const args of [[], ['--bad'], ['a', 'b', 'c'], ['first', '--normalize-psk', '--normalize-psk']]) {
    assert.equal(cli(args, inputs).code, 2);
  }
  assert.match(cli([], inputs).stderr, /All handshake records must be plaintext; encrypted handshake records are unsupported/);
  const missing = cli(['missing'], inputs);
  assert.deepEqual(missing, { code: 2, stdout: '', stderr: 'Cannot read input file.\n' });
  const invalid = cli(['first'], { first: Buffer.from('{"authorization":"private-token"}') });
  assert.equal(invalid.code, 2);
  assert.equal(invalid.stdout, '');
  assert.match(invalid.stderr, /binary TLS records, not HTTP capture JSON/);
  assert.equal(invalid.stderr.includes('private-token'), false);
});
