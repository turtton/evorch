#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const usage = 'Usage: node inspect-client-hello.mjs INPUT.bin [COMPARE.bin] [--normalize-psk]\nInput must be a client-direction TLS record binary stream for one connection containing unencrypted ClientHello handshakes, ending at complete record and handshake boundaries. All handshake records must be plaintext; encrypted handshake records are unsupported. HTTP capture JSON is not supported.';
const fail = (message) => { throw new Error(message); };

class Cursor {
  constructor(bytes, label) { this.bytes = bytes; this.position = 0; this.label = label; }
  get remaining() { return this.bytes.length - this.position; }
  take(length) {
    if (length > this.remaining) fail(`Truncated ${this.label}.`);
    const start = this.position;
    this.position += length;
    return this.bytes.subarray(start, this.position);
  }
  u8() { return this.take(1)[0]; }
  u16() { return this.take(2).readUInt16BE(); }
  vector(width, label) {
    const length = width === 1 ? this.u8() : this.u16();
    return new Cursor(this.take(length), label);
  }
  end() { if (this.remaining) fail(`Unexpected trailing bytes in ${this.label}.`); }
}

function numberVector(cursor, width, label) {
  const vector = cursor.vector(width, label);
  if (!vector.remaining || vector.remaining % 2) fail(`Malformed ${label}.`);
  const values = [];
  while (vector.remaining) values.push(vector.u16());
  cursor.end();
  return values;
}

function extensionDetail(type, data, normalizePsk) {
  const cursor = new Cursor(data, `extension ${type}`);
  if ([10, 13, 50].includes(type)) return { values: numberVector(cursor, 2, `extension ${type} vector`) };
  if (type === 43) return { values: numberVector(cursor, 1, 'supported versions') };
  if (type === 11 || type === 45) {
    const vector = cursor.vector(1, `extension ${type} vector`);
    if (!vector.remaining) fail(`Empty extension ${type} vector.`);
    const values = Array.from(vector.take(vector.remaining));
    cursor.end();
    return { values };
  }
  if (type === 0 || type === 16) {
    const vector = cursor.vector(2, type === 0 ? 'server names' : 'ALPN protocols');
    if (!vector.remaining) fail(`Empty extension ${type} vector.`);
    const entries = [];
    while (vector.remaining) {
      const nameType = type === 0 ? vector.u8() : undefined;
      const entry = vector.vector(type === 0 ? 2 : 1, `extension ${type} entry`);
      if (!entry.remaining) fail(`Empty extension ${type} entry.`);
      entries.push({ ...(nameType === undefined ? {} : { type: nameType }), length: entry.remaining });
    }
    cursor.end();
    return { entries };
  }
  if (type === 51) {
    const shares = cursor.vector(2, 'key shares');
    const entries = [];
    while (shares.remaining) {
      const group = shares.u16();
      const key = shares.vector(2, 'key share');
      if (!key.remaining) fail('Empty key share.');
      entries.push({ group, length: key.remaining });
      key.take(key.remaining).fill(0);
    }
    cursor.end();
    return { entries };
  }
  if (type === 41) {
    const identities = cursor.vector(2, 'PSK identities');
    const entries = [];
    while (identities.remaining) {
      const identity = identities.vector(2, 'PSK identity');
      if (!identity.remaining) fail('Empty PSK identity.');
      entries.push({ identityLength: identity.remaining, ageLength: 4 });
      const ticket = identity.take(identity.remaining);
      const age = identities.take(4);
      if (normalizePsk) { ticket.fill(0); age.fill(0); }
    }
    const binders = cursor.vector(2, 'PSK binders');
    const lengths = [];
    while (binders.remaining) {
      const binder = binders.vector(1, 'PSK binder');
      if (binder.remaining < 32) fail('Malformed PSK binder length.');
      lengths.push(binder.remaining);
      const bytes = binder.take(binder.remaining);
      if (normalizePsk) bytes.fill(0);
    }
    if (!entries.length || entries.length !== lengths.length) fail('Malformed PSK identity/binder count.');
    cursor.end();
    return { identities: entries, binderLengths: lengths };
  }
  return undefined;
}

export function normalizeHello(message, { normalizePsk = false } = {}) {
  const bytes = Buffer.from(message);
  const header = new Cursor(bytes, 'ClientHello handshake');
  if (header.u8() !== 1) fail('Expected a ClientHello handshake.');
  const length = header.take(3).readUIntBE(0, 3);
  if (length !== header.remaining) fail('Malformed ClientHello handshake length.');
  const body = new Cursor(header.take(length), 'ClientHello');
  const version = body.u16();
  body.take(32).fill(0);
  const session = body.vector(1, 'session ID');
  const sessionLength = session.remaining;
  if (sessionLength > 32) fail('Malformed session ID length.');
  session.take(sessionLength).fill(0);
  const cipherVector = body.vector(2, 'cipher suites');
  if (!cipherVector.remaining || cipherVector.remaining % 2) fail('Malformed cipher suite vector.');
  const ciphers = [];
  while (cipherVector.remaining) ciphers.push(cipherVector.u16());
  const compressionVector = body.vector(1, 'compression methods');
  if (!compressionVector.remaining) fail('Empty compression methods.');
  const compression = Array.from(compressionVector.take(compressionVector.remaining));
  const extensions = [];
  if (body.remaining) {
    const vector = body.vector(2, 'extensions');
    const seen = new Set();
    while (vector.remaining) {
      const type = vector.u16();
      if (seen.has(type)) fail('Duplicate ClientHello extension.');
      seen.add(type);
      const extension = vector.vector(2, `extension ${type}`);
      const data = extension.take(extension.remaining);
      const detail = extensionDetail(type, data, normalizePsk);
      extensions.push({ type, length: data.length, ...(detail === undefined ? {} : { detail }) });
    }
  }
  body.end();
  return { byteLength: bytes.length, normalizedSha256: createHash('sha256').update(bytes).digest('hex'), version, sessionLength, ciphers, compression, extensions };
}

// Accept arbitrary TCP chunks; handshake records must be plaintext.
// Encrypted handshake records are unsupported; finish() rejects incomplete tails.
export function createRecordParser(options = {}) {
  let incoming = Buffer.alloc(0), pending = Buffer.alloc(0), total = 0, consumed = 0;
  let segments = [], finished = false;
  const records = [], clientHellos = [];
  return {
    push(chunk) {
      if (finished) fail('Parser already finished.');
      if (!(chunk instanceof Uint8Array)) fail('Expected binary TLS bytes.');
      incoming = Buffer.concat([incoming, chunk]);
      while (incoming.length >= 5) {
        const type = incoming[0], version = incoming.readUInt16BE(1), length = incoming.readUInt16BE(3);
        if (type < 20 || type > 24 || version < 0x0300 || version > 0x0304 || length > 18432) {
          fail('Malformed TLS record header. Input must be binary TLS records, not HTTP capture JSON.');
        }
        if (incoming.length < 5 + length) break;
        const payload = incoming.subarray(5, 5 + length);
        incoming = incoming.subarray(5 + length);
        const index = records.length;
        records.push({ type, version, length, offset: total });
        total += 5 + length;
        if (type !== 22) {
          if (pending.length) fail('Non-handshake record interrupts a fragmented handshake.');
          continue;
        }
        const start = consumed + pending.length;
        segments.push({ index, start, end: start + length });
        pending = Buffer.concat([pending, payload]);
        while (pending.length >= 4) {
          const size = 4 + pending.readUIntBE(1, 3);
          if (pending.length < size) break;
          if (pending[0] === 1) {
            const recordBoundaries = segments.filter(s => s.start < consumed + size && s.end > consumed).map(s => ({
              index: s.index, ...records[s.index],
              offsetInRecord: Math.max(consumed - s.start, 0),
              helloOffset: Math.max(s.start - consumed, 0),
              helloLength: Math.min(s.end, consumed + size) - Math.max(s.start, consumed),
            }));
            clientHellos.push({ ...normalizeHello(pending.subarray(0, size), options), recordBoundaries });
          }
          consumed += size;
          pending = pending.subarray(size);
          segments = segments.filter(s => s.end > consumed);
        }
      }
    },
    finish() {
      if (finished) fail('Parser already finished.');
      finished = true;
      if (incoming.length) fail('Truncated TLS record. Input must be binary TLS records, not HTTP capture JSON.');
      if (pending.length) fail('Truncated TLS handshake.');
      if (!clientHellos.length) fail('No ClientHello found in client-direction TLS record stream.');
      return { byteLength: total, records, clientHellos };
    },
  };
}

export function inspectStream(bytes, options = {}) {
  const parser = createRecordParser(options);
  parser.push(bytes);
  return parser.finish();
}

export function compareStreams(leftBytes, rightBytes, options = {}) {
  const left = inspectStream(leftBytes, options), right = inspectStream(rightBytes, options);
  return {
    left, right,
    normalizedHandshakeEqual: JSON.stringify(left.clientHellos.map(h => h.normalizedSha256)) === JSON.stringify(right.clientHellos.map(h => h.normalizedSha256)),
    recordBoundariesEqual: JSON.stringify(left.clientHellos.map(h => h.recordBoundaries)) === JSON.stringify(right.clientHellos.map(h => h.recordBoundaries)),
  };
}

export function runCli(args, { readFile = readFileSync, stdout = process.stdout, stderr = process.stderr } = {}) {
  try {
    const paths = [], options = { normalizePsk: false };
    for (const arg of args) {
      if (arg === '--normalize-psk' && !options.normalizePsk) options.normalizePsk = true;
      else if (arg.startsWith('-')) fail(usage);
      else paths.push(arg);
    }
    if (paths.length < 1 || paths.length > 2) fail(usage);
    const inputs = paths.map(path => {
      try { return readFile(path); } catch { fail('Cannot read input file.'); }
    });
    const result = inputs.length === 1 ? { input: inspectStream(inputs[0], options) } : compareStreams(inputs[0], inputs[1], options);
    stdout.write(`${JSON.stringify({ normalization: { random: true, sessionId: true, keyShares: true, preSharedKey: options.normalizePsk }, ...result }, null, 2)}\n`);
    return inputs.length === 2 && (!result.normalizedHandshakeEqual || !result.recordBoundariesEqual) ? 1 : 0;
  } catch (error) {
    stderr.write(`${error.message}\n`);
    return 2;
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = runCli(process.argv.slice(2));
}
