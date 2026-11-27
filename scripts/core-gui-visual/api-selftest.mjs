// TEMPORARY (removed before the PR is final): drives the evidence branch writer
// against the real API with the workflow's GITHUB_TOKEN, on a separate branch.
import { appendFileSync } from 'node:fs';
import { GitHubApi, ZERO_OID } from './github-api.mjs';
import { replacePrDirectory } from './evidence-store.mjs';

const branch = process.env.SELFTEST_BRANCH ?? 'gui-visual-evidence-selftest';
const api = new GitHubApi({ token: process.env.GH_TOKEN, repo: process.env.GITHUB_REPOSITORY });
const rows = [];
const say = (step, detail) => {
  rows.push(`| ${step} | ${detail} |`);
  console.log(`${step}: ${detail}`);
};
const png = (text) => Buffer.from(`not really a png: ${text}`);
const describe = async () => {
  const oid = await api.getRefOid(branch);
  if (!oid) return 'ref absent';
  const commit = await api.getCommit(oid);
  const tree = [...(await api.getTreeBlobs(commit.tree)).keys()].join(', ');
  return `ref ${oid.slice(0, 12)} parents=${commit.parents.length} verified=${commit.verified} (${commit.reason}) tree=[${tree}]`;
};
const run = async (step, pr, files) => {
  const result = await replacePrDirectory({ api, branch, pr, files, log: console.log });
  say(step, `${result.outcome} attempts=${result.attempts} commit=${result.commit?.sha ?? '-'} verified=${result.commit?.verified ?? '-'} reason=${result.commit?.reason ?? '-'} parents=${result.commit?.parents.length ?? '-'} | ${await describe()}`);
  return result;
};

try {
  // start clean: the branch of an earlier run is deleted with the same compare-and-swap
  const left = await api.getRefOid(branch);
  if (left) say('cleanup', JSON.stringify(await api.updateRef({ branch, before: left, after: ZERO_OID })));

  try {
    const tree = await api.createTree([]);
    const commit = await api.createCommit({ tree, message: 'selftest: empty tree' });
    say('empty-tree probe', `tree ${tree} commit ${commit.sha} verified=${commit.verified} (${commit.reason}) parents=${commit.parents.length}`);
  } catch (error) {
    say('empty-tree probe', `refused: ${error.message}`);
  }

  const created = await run('1 create pr-1', 1, new Map([['a-before-1.png', png('a1')]]));
  const afterCreate = await api.getRefOid(branch);
  await run('2 update pr-1', 1, new Map([['a-after-2.png', png('a2')]]));
  await run('3 add pr-2', 2, new Map([['b-after-1.png', png('b1')]]));
  const head = await api.getRefOid(branch);

  // an update whose expectation is the older value must be refused and leave the ref alone
  const stale = await api.updateRef({ branch, before: afterCreate, after: created.commit.sha });
  say('4 stale beforeOid', `ok=${stale.ok} message=${JSON.stringify(stale.message)} ref unchanged=${(await api.getRefOid(branch)) === head}`);
  const exists = await api.updateRef({ branch, before: ZERO_OID, after: created.commit.sha });
  say('5 create-only on existing ref', `ok=${exists.ok} message=${JSON.stringify(exists.message)} ref unchanged=${(await api.getRefOid(branch)) === head}`);

  await run('6 remove pr-1 (pr-2 stays)', 1, null);
  await run('7 remove pr-1 again (no-op)', 1, null);
  await run('8 remove pr-2 (last directory)', 2, null);
  await run('9 create pr-3 from nothing', 3, new Map([['c-after-1.png', png('c1')]]));
  await run('10 remove pr-3 (last directory)', 3, null);
} catch (error) {
  say('FAILED', String(error.stack ?? error));
  process.exitCode = 1;
} finally {
  const text = `### evidence branch self-test (${branch})\n\n| step | result |\n|---|---|\n${rows.join('\n')}\n`;
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, text);
}
