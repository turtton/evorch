import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ApiError, UnknownOutcome } from '../github-api.mjs';
import { EvidenceError, replacePrDirectory } from '../evidence-store.mjs';
import { FakeGitHub } from './fake-github.mjs';

const BRANCH = 'evidence';
const files = (name, text = name) => new Map([[name, Buffer.from(text)]]);
const noSleep = async () => {};
const put = (api, pr, map, extra = {}) => replacePrDirectory({ api, branch: BRANCH, pr, files: map, sleep: noSleep, ...extra });
const everyCommitIsParentless = (api) => [...api.commits.values()].every((c) => c.parents.length === 0);

// Given: an empty store / When: PRs are added, replaced and removed
// Then: each PR's directory is replaced as a whole, the others stay, every commit is
//       parentless and verified, and the last removal deletes the ref
test('a PR directory is replaced as a whole and the last removal deletes the ref', async () => {
  const api = new FakeGitHub();
  assert.equal((await put(api, 1, files('a-1.png'))).outcome, 'created');
  assert.equal((await put(api, 2, files('b-1.png'))).outcome, 'updated');
  await put(api, 1, files('a-2.png'));
  assert.deepEqual(await api.paths(BRANCH), ['pr-1/a-2.png', 'pr-2/b-1.png']);
  assert.equal((await put(api, 1, null)).outcome, 'updated');
  assert.deepEqual(await api.paths(BRANCH), ['pr-2/b-1.png']);
  assert.equal((await put(api, 1, null)).outcome, 'noop', 'removing what is not there changes nothing');
  assert.equal((await put(api, 2, null)).outcome, 'deleted');
  assert.equal(await api.getRefOid(BRANCH), null);
  assert.equal((await put(api, 3, files('c-1.png'))).outcome, 'created', 'a deleted ref is created again');
  assert.ok(everyCommitIsParentless(api));
});

// Given: the same content / Then: nothing is committed
test('an unchanged directory is a no-op', async () => {
  const api = new FakeGitHub();
  await put(api, 1, files('a.png'));
  const head = await api.getRefOid(BRANCH);
  const result = await put(api, 1, files('a.png'));
  assert.equal(result.outcome, 'noop');
  assert.equal(await api.getRefOid(BRANCH), head);
});

// Given: another writer moves the ref between our read and our update
// Then: the compare-and-swap fails, we rebuild from the new tree and keep their directory
test('a lost race is retried from the new tree', async () => {
  const api = new FakeGitHub();
  await put(api, 1, files('a.png'));
  let raced = false;
  api.beforeUpdate = () => {
    if (raced) return;
    raced = true;
    // the other writer's whole update, made directly on the store
    const head = api.refs.get(BRANCH);
    const tree = new Map(api.trees.get(api.commits.get(head).tree));
    tree.set('pr-9/other.png', { mode: '100644', sha: 'f'.repeat(40) });
    api.trees.set('theirs', tree);
    api.commits.set('theirs-commit', { tree: 'theirs', parents: [], verified: true, pending: 0 });
    api.refs.set(BRANCH, 'theirs-commit');
  };
  const logs = [];
  const result = await put(api, 2, files('b.png'), { log: (line) => logs.push(line) });
  assert.equal(result.attempts, 2);
  assert.deepEqual(await api.paths(BRANCH), ['pr-1/a.png', 'pr-2/b.png', 'pr-9/other.png']);
  assert.ok(logs.some((line) => line.includes('moved')));
});

// Given: the ref keeps moving / Then: it gives up after the limit and changes nothing of its own
test('endless contention fails after the attempt limit', async () => {
  const api = new FakeGitHub();
  await put(api, 1, files('a.png'));
  let n = 0;
  api.beforeUpdate = () => {
    n += 1;
    const tree = new Map([[`pr-5/x${n}.png`, { mode: '100644', sha: String(n).padStart(40, '0') }]]);
    api.trees.set(`t${n}`, tree);
    api.commits.set(`c${n}`, { tree: `t${n}`, parents: [], verified: true, pending: 0 });
    api.refs.set(BRANCH, `c${n}`);
  };
  await assert.rejects(put(api, 2, files('b.png'), { maxAttempts: 3 }), /gave up after 3 attempts/);
  assert.equal(n, 3);
});

// Given: an update refused while the ref did not move (rules, permissions)
// Then: it is a failure, not a lost race to retry
test('a refused update that did not move the ref is an error', async () => {
  const api = new FakeGitHub();
  api.refuse = 'branch is protected';
  await assert.rejects(put(api, 1, files('a.png')), (error) => error instanceof EvidenceError && /refused/.test(error.message) && /protected/.test(error.message));
  assert.equal(api.calls.filter((c) => c === 'updateRef').length, 1, 'not retried');
});

// Given: GitHub does not sign the commit / Then: the ref is never pointed at it
test('an unverified commit is never published', async () => {
  const api = new FakeGitHub();
  api.signed = false;
  await assert.rejects(put(api, 1, files('a.png')), /not verified/);
  assert.equal(api.calls.includes('updateRef'), false);
  assert.equal(await api.getRefOid(BRANCH), null);
});

// Given: the verification arrives a little late / Then: it is waited for a bounded number of times
test('a late verification is polled, a missing one is not waited for forever', async () => {
  const api = new FakeGitHub();
  api.lateVerification = 2;
  const sleeps = [];
  await put(api, 1, files('a.png'), { sleep: async (ms) => sleeps.push(ms) });
  assert.equal(sleeps.length, 2);
  assert.ok(await api.getRefOid(BRANCH));

  const stuck = new FakeGitHub();
  stuck.lateVerification = 100;
  await assert.rejects(put(stuck, 1, files('a.png'), { verifyAttempts: 3 }), /not verified/);
  assert.equal(await stuck.getRefOid(BRANCH), null);
});

// Given: a commit with a parent or the wrong tree / Then: it is refused before the ref
test('a commit with a parent or another tree is refused', async () => {
  const withParent = new FakeGitHub();
  const original = withParent.createCommit.bind(withParent);
  withParent.createCommit = async (input) => ({ ...(await original(input)), parents: ['a'.repeat(40)] });
  await assert.rejects(put(withParent, 1, files('a.png')), /parents/);
  assert.equal(withParent.calls.includes('updateRef'), false);

  const wrongTree = new FakeGitHub();
  const make = wrongTree.createCommit.bind(wrongTree);
  wrongTree.createCommit = async (input) => ({ ...(await make(input)), tree: 'e'.repeat(40) });
  await assert.rejects(put(wrongTree, 1, files('a.png')), /expected/);
});

// Given: the reply of the update is lost / Then: the ref is read again; an applied update
//       is not sent twice, an unapplied one is rebuilt
test('an update with an unknown outcome is checked, not resent blindly', async () => {
  const applied = new FakeGitHub();
  applied.afterUpdate = () => {
    throw new UnknownOutcome('timeout');
  };
  // the fake applies the update, then loses the reply
  const apply = applied.updateRef.bind(applied);
  applied.updateRef = async (input) => {
    await apply(input).catch(() => {});
    throw new UnknownOutcome('timeout');
  };
  const result = await put(applied, 1, files('a.png'));
  assert.equal(result.outcome, 'created');
  assert.equal(applied.calls.filter((c) => c === 'updateRef').length, 1);
  assert.ok(await applied.getRefOid(BRANCH));

  const lost = new FakeGitHub();
  let first = true;
  const real = lost.updateRef.bind(lost);
  lost.updateRef = async (input) => {
    if (first) {
      first = false;
      throw new ApiError(502, 'bad gateway');
    }
    return real(input);
  };
  const retried = await put(lost, 1, files('a.png'));
  assert.equal(retried.attempts, 2);
  assert.deepEqual(await lost.paths(BRANCH), ['pr-1/a.png']);
});

// Given: a hard API failure / Then: it propagates
test('an API failure is not reported as success', async () => {
  const api = new FakeGitHub();
  api.getRefOid = async () => {
    throw new ApiError(403, 'forbidden');
  };
  await assert.rejects(put(api, 1, files('a.png')), /forbidden/);
});

// Given: a directory of a closed PR / When: any writer runs
// Then: closed ones are removed; open ones, and ones whose state cannot be read, stay
test('directories of closed PRs are cleaned up, unknown ones kept', async () => {
  const api = new FakeGitHub();
  await put(api, 5, files('x.png'));
  await put(api, 6, files('y.png'));
  await put(api, 7, files('z.png'));
  const states = { 5: true, 6: false };
  const logs = [];
  const isClosed = async (n) => {
    if (n === 7) throw new Error('rate limited');
    return states[n];
  };
  await replacePrDirectory({ api, branch: BRANCH, pr: 8, files: files('w.png'), isClosed, sleep: noSleep, log: (l) => logs.push(l) });
  assert.deepEqual(await api.paths(BRANCH), ['pr-6/y.png', 'pr-7/z.png', 'pr-8/w.png']);
  assert.ok(logs.some((l) => /keeping pr-7/.test(l)));
});

// Given: unsafe file names / Then: refused before any request
test('file names are limited to a safe set', async () => {
  const api = new FakeGitHub();
  for (const name of ['../x.png', 'a/b.png', '', '.hidden/..', 'a b.png']) {
    await assert.rejects(put(api, 1, files(name)), /unsafe file name/);
  }
  assert.equal(api.calls.length, 0);
});
