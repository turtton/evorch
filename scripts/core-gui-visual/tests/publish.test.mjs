import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { MARKER, findOurs, renderClosed, renderIrrelevant } from '../comment.mjs';
import { canWrite, decideMode } from '../policy.mjs';
import { ArtifactError, execute, loadArtifact } from '../publish.mjs';
import { FakeGitHub } from './fake-github.mjs';

const REPO = 'o/r';
const BRANCH = 'evidence';
const HEAD = 'a'.repeat(40);
const RUN = 'https://github.com/o/r/actions/runs/1';
const PNG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const noSleep = async () => {};

/** A PNG-looking file and its content-addressed name. */
function image(id, role, text) {
  const bytes = Buffer.concat([PNG, Buffer.from(text)]);
  const hash = createHash('sha256').update(bytes).digest('hex').slice(0, 12);
  return { name: `${id}-${role}-${hash}.png`, bytes };
}

/** A capture artifact on disk: `screens` is [{id, status, text}]. */
function artifact(screens, { baseline = true, mutate = (report) => report } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'visual-artifact-'));
  mkdirSync(join(dir, 'compare'));
  const entries = screens.map(({ id, status, text }) => {
    const entry = { id, status, changed_pixels: status === 'changed' ? 12 : 0, bounds: null, before: null, after: null, diff: null };
    const roles = { changed: ['before', 'after', 'diff'], added: ['after'], removed: ['before'], unchanged: [] }[status];
    for (const role of roles) {
      const file = image(id, role, `${text}-${role}`);
      writeFileSync(join(dir, 'compare', file.name), file.bytes);
      entry[role] = file.name;
    }
    return entry;
  });
  const report = mutate({ schema: 1, baseline, fixture_changed: false, entries });
  writeFileSync(join(dir, 'compare', 'report.json'), JSON.stringify(report));
  writeFileSync(join(dir, 'meta.json'), JSON.stringify({ before_sha: 'b'.repeat(40), after_sha: 'c'.repeat(40), baseline }));
  return dir;
}

function setup(number = 5) {
  const api = new FakeGitHub();
  api.setPull(number, { headSha: HEAD });
  return api;
}

const run = (api, mode, dir, overrides = {}) =>
  execute({
    api,
    repo: REPO,
    branch: BRANCH,
    pr: { number: 5, headSha: HEAD, baseRef: 'main' },
    mode,
    loadCapture: () => loadArtifact(dir),
    runUrl: RUN,
    sleep: noSleep,
    ...overrides,
  });

// ---------------------------------------------------------------- policy

// Given: pull requests from a fork, this repository, a deleted fork / Then: only the own repo writes
test('only a pull request from this repository may write', () => {
  assert.equal(canWrite({ head: { repo: { full_name: 'o/r', fork: false } } }, 'o/r'), true);
  assert.equal(canWrite({ head: { repo: { full_name: 'someone/r', fork: true } } }, 'o/r'), false);
  assert.equal(canWrite({ head: { repo: { full_name: 'o/r', fork: true } } }, 'o/r'), false);
  assert.equal(canWrite({ head: { repo: null } }, 'o/r'), false);
  assert.equal(canWrite({ head: {} }, 'o/r'), false);
  assert.equal(canWrite(undefined, 'o/r'), false);
  assert.equal(canWrite({ head: { repo: { full_name: 'o/r2' } } }, 'o/r'), false);
});

// Given: events / Then: closed never depends on the capture or on the paths
test('a closed event is a cleanup whatever the paths say', () => {
  assert.equal(decideMode('closed', true), 'cleanup');
  assert.equal(decideMode('closed', false), 'cleanup');
  assert.equal(decideMode('closed', undefined), 'cleanup');
  assert.equal(decideMode('synchronize', false), 'irrelevant');
  assert.equal(decideMode('opened', true), 'publish');
  assert.equal(decideMode('reopened', undefined), 'publish');
});

// ---------------------------------------------------------------- artifact

test('a good artifact loads with its changed screens only', () => {
  const dir = artifact([
    { id: 'attention', status: 'changed', text: 'a' },
    { id: 'calm', status: 'unchanged', text: 'c' },
  ]);
  const { files, report } = loadArtifact(dir);
  assert.equal(files.size, 3);
  assert.equal(report.entries.length, 2);
  rmSync(dir, { recursive: true });
});

// Given: artifacts a hostile PR could make / Then: each is refused
test('a malformed artifact is refused', () => {
  const bad = (mutate, pattern, screens = [{ id: 'a', status: 'changed', text: 'x' }]) => {
    const dir = artifact(screens, { mutate });
    assert.throws(() => loadArtifact(dir), (error) => error instanceof ArtifactError && pattern.test(error.message), String(pattern));
    rmSync(dir, { recursive: true });
  };
  bad((r) => ({ ...r, schema: 2 }), /malformed/);
  bad((r) => ({ ...r, entries: [] }), /lists 0 screens/);
  bad((r) => ({ ...r, entries: Array.from({ length: 33 }, (_, i) => ({ ...r.entries[0], id: `s${i}` })) }), /33 screens/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], id: '../x' }] }), /bad or repeated/);
  bad((r) => ({ ...r, entries: [r.entries[0], r.entries[0]] }), /repeated/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], before: '../../etc/passwd' }] }), /bad before file name/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], diff: 'a-diff-000000000000.png' }] }), /does not match|cannot read/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], status: 'weird' }] }), /unknown status/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], changed_pixels: 0 }] }), /pixel count/);
  bad((r) => ({ ...r, baseline: false }), /disagree/);
  bad((r) => ({ ...r, entries: [{ ...r.entries[0], status: 'added' }] }), /must not have/);
  // a file that is not a PNG, under a name that matches its own hash
  const dir = artifact([{ id: 'a', status: 'added', text: 'x' }]);
  const text = Buffer.from('<script>alert(1)</script>');
  const hash = createHash('sha256').update(text).digest('hex').slice(0, 12);
  const report = JSON.parse(Buffer.from(JSON.stringify({ schema: 1, baseline: true, fixture_changed: false, entries: [{ id: 'a', status: 'added', changed_pixels: 0, after: `a-after-${hash}.png` }] })));
  writeFileSync(join(dir, 'compare', `a-after-${hash}.png`), text);
  writeFileSync(join(dir, 'compare', 'report.json'), JSON.stringify(report));
  assert.throws(() => loadArtifact(dir), /not a PNG/);
  rmSync(dir, { recursive: true });
  assert.throws(() => loadArtifact(join(tmpdir(), 'no-such-artifact-dir')), ArtifactError);
});

// ---------------------------------------------------------------- the flows

// Given: a changed screen / When: published, run again, pushed again
// Then: one comment, the same one updated; the images are under the PR's directory
test('a changed screen is published and the one comment is updated in place', async () => {
  const api = setup();
  const first = artifact([{ id: 'attention', status: 'changed', text: 'v1' }, { id: 'calm', status: 'unchanged', text: 'c' }]);
  const created = await run(api, 'publish', first);
  assert.equal(created.outcome, 'published');
  assert.equal(created.comment.action, 'created');
  const paths = await api.paths(BRANCH);
  assert.equal(paths.length, 3);
  assert.ok(paths.every((p) => p.startsWith('pr-5/attention-')));
  const body = api.comments[0].body;
  assert.ok(body.startsWith(MARKER));
  assert.match(body, /<details open><summary>attention（12 px 変更）/);
  assert.match(body, new RegExp(`https://raw.githubusercontent.com/o/r/${BRANCH}/pr-5/attention-before-[0-9a-f]{12}.png`));
  assert.doesNotMatch(body, /calm/, 'an unchanged screen is not shown');
  const id = api.comments[0].id;

  const again = await run(api, 'publish', first);
  assert.equal(again.comment.action, 'unchanged');
  assert.equal(again.stored.outcome, 'noop');

  const second = artifact([{ id: 'attention', status: 'changed', text: 'v2' }]);
  const updated = await run(api, 'publish', second);
  assert.equal(updated.comment.action, 'updated');
  assert.equal(api.comments.length, 1);
  assert.equal(api.comments[0].id, id, 'the same comment');
  assert.notDeepEqual(await api.paths(BRANCH), paths, 'the new images have new names');
  assert.equal((await api.paths(BRANCH)).length, 3, 'the old ones are gone');
});

// Given: a PR that had a diff and now has none / Then: "no difference" and its images are removed
test('no difference updates the comment and removes the images', async () => {
  const api = setup();
  api.setPull(6, { headSha: 'f'.repeat(40) });
  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  await run(api, 'publish', artifact([{ id: 'a', status: 'unchanged', text: 'x' }]), {});
  assert.match(api.comments[0].body, /^画面差分なし$/m);
  assert.equal(await api.getRefOid(BRANCH), null, 'the last directory is gone with the ref');
  assert.doesNotMatch(api.comments[0].body, /<img/);
});

// Given: no baseline / Then: all screens new, said so, not "no difference"
test('no baseline is said and shows the new screens', async () => {
  const api = setup();
  await run(api, 'publish', artifact([{ id: 'a', status: 'added', text: 'x' }, { id: 'b', status: 'added', text: 'y' }], { baseline: false }));
  const body = api.comments[0].body;
  assert.match(body, /baseline なし/);
  assert.doesNotMatch(body, /^画面差分なし$/m);
  assert.equal((body.match(/<img/g) ?? []).length, 2);
});

// Given: added and removed screens / Then: shown as such, with only the image that exists
test('added and removed screens are shown without an invented side', async () => {
  const api = setup();
  await run(api, 'publish', artifact([{ id: 'n', status: 'added', text: 'x' }, { id: 'o', status: 'removed', text: 'y' }]));
  const body = api.comments[0].body;
  assert.match(body, /n（新規）/);
  assert.match(body, /o（削除）/);
  assert.equal((body.match(/<img/g) ?? []).length, 2);
});

// Given: the PR changes nothing relevant / Then: no new comment; an existing one is updated and images removed
test('an irrelevant PR gets no new comment but fixes up an existing one', async () => {
  const api = setup();
  const none = await run(api, 'irrelevant', null);
  assert.equal(none.comment, null);
  assert.equal(api.comments.length, 0);

  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  assert.ok(await api.getRefOid(BRANCH));
  const fixed = await run(api, 'irrelevant', null);
  assert.equal(fixed.comment.action, 'updated');
  assert.equal(api.comments[0].body, renderIrrelevant({ pr: { headSha: HEAD }, runUrl: RUN }));
  assert.match(api.comments[0].body, /画面差分なし（対象外の変更のみ）/);
  assert.equal(await api.getRefOid(BRANCH), null);
});

// Given: a closed PR (merged or not) with images and a comment / Then: images removed, comment says so,
//       other PRs stay
test('closing removes the PR images, keeps the others and marks the comment', async () => {
  const api = setup();
  api.setPull(9, { headSha: 'd'.repeat(40) });
  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  await execute({ api, repo: REPO, branch: BRANCH, pr: { number: 9, headSha: 'd'.repeat(40), baseRef: 'main' }, mode: 'publish', loadCapture: () => loadArtifact(artifact([{ id: 'z', status: 'added', text: 'q' }], { baseline: false })), runUrl: RUN, sleep: noSleep });
  for (const merged of [false, true]) {
    api.setPull(5, { state: 'closed', merged, headSha: HEAD });
    const closed = await run(api, 'cleanup', null);
    assert.equal(closed.outcome, 'closed');
    const dirs = (await api.paths(BRANCH)).map((p) => p.split('/')[0]);
    assert.ok(dirs.every((d) => d === 'pr-9') && dirs.length === 1);
    const ours = findOurs(api.comments.filter((c) => c.issue === 5));
    assert.equal(ours.length, 1);
    assert.equal(ours[0].body, renderClosed({ runUrl: RUN }));
  }
  // closing a PR that never had anything creates nothing
  api.setPull(11, { state: 'closed' });
  const before = api.comments.length;
  const result = await execute({ api, repo: REPO, branch: BRANCH, pr: { number: 11, headSha: 'e'.repeat(40), baseRef: 'main' }, mode: 'cleanup', loadCapture: () => null, runUrl: RUN, sleep: noSleep });
  assert.equal(result.comment, null);
  assert.equal(api.comments.length, before);
});

// Given: a PR reopened before the closed run got to work / Then: its images stay
test('a reopened PR keeps its images', async () => {
  const api = setup();
  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  const paths = await api.paths(BRANCH);
  api.setPull(5, { state: 'open', headSha: HEAD });
  const result = await run(api, 'cleanup', null);
  assert.equal(result.outcome, 'reopened');
  assert.deepEqual(await api.paths(BRANCH), paths);
});

// Given: a result captured for an older head, or for a PR closed meanwhile / Then: not published
test('a stale or closed PR is not published', async () => {
  const stale = setup();
  stale.setPull(5, { headSha: 'f'.repeat(40) });
  const result = await run(stale, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  assert.equal(result.outcome, 'stale');
  assert.equal(stale.calls.includes('createBlob'), false);
  assert.equal(stale.comments.length, 0);

  const closed = setup();
  closed.setPull(5, { state: 'closed', headSha: HEAD });
  const gone = await run(closed, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  assert.equal(gone.outcome, 'closed');
  assert.equal(closed.calls.includes('createBlob'), false);
});

// Given: comments by others, including a copy of the marker, and more than one page
// Then: only the bot's marked comment is touched
test('only the bot comment carrying the marker is ours', async () => {
  const api = setup();
  const copy = api.addComment(5, `${MARKER}\nfake`);
  const human = api.addComment(5, 'hello');
  const botWithoutMarker = api.addComment(5, 'a bot note', { login: 'github-actions[bot]', type: 'Bot' });
  const lookalike = api.addComment(5, `${MARKER}\nfake`, { login: 'github-actions', type: 'User' });
  for (let i = 0; i < 120; i += 1) api.addComment(5, `noise ${i}`);
  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  const ours = api.comments.filter((c) => c.body.startsWith(MARKER) && c.user.type === 'Bot' && c.user.login === 'github-actions[bot]' && c.body.includes('新 GUI'));
  assert.equal(ours.length, 1);
  for (const untouched of [copy, human, botWithoutMarker, lookalike]) {
    assert.equal(api.comments.find((c) => c.id === untouched.id).body, untouched.body);
  }
});

// Given: the comment list is paged by the real client / Then: the client reads every page
test('the client lists every page of comments', async () => {
  const { GitHubApi } = await import('../github-api.mjs');
  const pages = [Array.from({ length: 100 }, (_, i) => ({ id: i })), Array.from({ length: 3 }, (_, i) => ({ id: 100 + i }))];
  const urls = [];
  const fetchImpl = async (url) => {
    urls.push(url);
    return new Response(JSON.stringify(pages[Number(new URL(url).searchParams.get('page')) - 1]), { status: 200 });
  };
  const api = new GitHubApi({ token: 't', repo: 'o/r', fetchImpl });
  assert.equal((await api.listComments(5)).length, 103);
  assert.equal(urls.length, 2);
});

// Given: API failures / Then: the run fails and no success comment is made
test('an API failure fails the run', async () => {
  const api = setup();
  api.signed = false;
  await assert.rejects(run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }])), /not verified/);
  assert.equal(api.comments.length, 0, 'no comment for images that were not published');

  const down = setup();
  down.getPull = async () => {
    throw new Error('network down');
  };
  await assert.rejects(run(down, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }])), /network down/);
});

// Given: the comment update fails after the images were stored / Then: the run fails, and a rerun converges
test('a failed comment update is repaired by a rerun', async () => {
  const api = setup();
  const dir = artifact([{ id: 'a', status: 'changed', text: 'x' }]);
  api.createComment = async () => {
    throw new Error('comment API down');
  };
  await assert.rejects(run(api, 'publish', dir), /comment API down/);
  assert.equal((await api.paths(BRANCH)).length, 3, 'the images are stored');
  delete api.createComment;
  const again = await run(api, 'publish', dir);
  assert.equal(again.stored.outcome, 'noop');
  assert.equal(again.comment.action, 'created');
});

// Given: a second writer for another PR while we write / Then: both directories survive
test('two PRs written one after the other both stay', async () => {
  const api = setup();
  api.setPull(6, { headSha: 'f'.repeat(40) });
  await run(api, 'publish', artifact([{ id: 'a', status: 'changed', text: 'x' }]));
  await execute({ api, repo: REPO, branch: BRANCH, pr: { number: 6, headSha: 'f'.repeat(40), baseRef: 'main' }, mode: 'publish', loadCapture: () => loadArtifact(artifact([{ id: 'a', status: 'changed', text: 'other' }])), runUrl: RUN, sleep: noSleep });
  const dirs = new Set((await api.paths(BRANCH)).map((p) => p.split('/')[0]));
  assert.deepEqual([...dirs].sort(), ['pr-5', 'pr-6']);
});
