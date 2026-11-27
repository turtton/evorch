// Replaces one PR's directory on the evidence branch.
//
// Every change is a new parentless commit made by the Git Data API with no author
// or committer (GitHub signs it as the token's identity), checked for zero parents
// and `verified` before any ref points at it, and then a compare-and-swap of the
// ref (`updateRefs` with `beforeOid`). When the last directory goes away the ref is
// deleted by the same compare-and-swap, so there is no empty-tree commit to keep.
import { ApiError, UnknownOutcome, ZERO_OID } from './github-api.mjs';

export class EvidenceError extends Error {
  constructor(message) {
    super(message);
    this.name = 'EvidenceError';
  }
}

export const prDirName = (number) => `pr-${number}`;

/** The PR numbers whose directories a tree holds. */
export function prNumbersIn(paths) {
  const numbers = new Set();
  for (const path of paths) {
    const match = /^pr-(\d+)\//.exec(path);
    if (match) numbers.add(Number(match[1]));
  }
  return [...numbers];
}

/**
 * @param api            a GitHubApi (or a fake with the same methods)
 * @param branch         the evidence branch
 * @param pr             the PR number whose directory is replaced
 * @param files          Map<name, Buffer> for the new content, or null/empty to remove the directory
 * @param isClosed       async (number) => true | false; a throw or non-boolean keeps that directory
 * @param log            (line) => void
 * @param sleep          async (ms) => void, for the bounded wait on a late verification
 * @returns {outcome: 'noop'|'created'|'updated'|'deleted', commit, attempts}
 */
export async function replacePrDirectory({
  api,
  branch,
  pr,
  files,
  isClosed = async () => null,
  log = () => {},
  sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  maxAttempts = 5,
  verifyAttempts = 4,
}) {
  const dir = prDirName(pr);
  const wanted = new Map();
  for (const [name, buffer] of files ?? []) {
    if (!/^[A-Za-z0-9._-]+$/.test(name)) throw new EvidenceError(`unsafe file name: ${name}`);
    wanted.set(`${dir}/${name}`, { mode: '100644', sha: await api.createBlob(buffer) });
  }

  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    const head = await api.getRefOid(branch);
    const current = head ? await api.getTreeBlobs((await api.getCommit(head)).tree) : new Map();
    const next = new Map();
    for (const [path, entry] of current) {
      if (!path.startsWith(`${dir}/`)) next.set(path, entry);
    }
    // A directory of another PR the API reports closed is removed too (a closed run
    // that never started leaves one). An unanswered check keeps it.
    for (const number of prNumbersIn(next.keys())) {
      let closed = null;
      try {
        closed = await isClosed(number);
      } catch (error) {
        log(`keeping ${prDirName(number)}: state of #${number} unknown (${error.message})`);
      }
      if (closed === true) {
        log(`removing ${prDirName(number)}: #${number} is closed`);
        for (const path of [...next.keys()]) if (path.startsWith(`${prDirName(number)}/`)) next.delete(path);
      }
    }
    for (const [path, entry] of wanted) next.set(path, entry);

    if (sameBlobs(current, next)) return { outcome: 'noop', commit: null, attempts: attempt };

    let update;
    if (next.size === 0) {
      update = { after: ZERO_OID, commit: null };
    } else {
      const entries = [...next].sort(([a], [b]) => (a < b ? -1 : 1)).map(([path, e]) => ({ path, ...e }));
      const tree = await api.createTree(entries);
      let commit = await api.createCommit({ tree, message: `evidence: update ${dir}` });
      for (let waits = 0; !commit.verified && waits < verifyAttempts; waits += 1) {
        await sleep(2000);
        commit = await api.getCommit(commit.sha);
      }
      if (commit.parents.length !== 0) throw new EvidenceError(`commit ${commit.sha} has ${commit.parents.length} parents`);
      if (commit.tree !== tree) throw new EvidenceError(`commit ${commit.sha} has tree ${commit.tree}, expected ${tree}`);
      if (!commit.verified) throw new EvidenceError(`commit ${commit.sha} is not verified (${commit.reason}); the ref is unchanged`);
      update = { after: commit.sha, commit };
    }

    const before = head ?? ZERO_OID;
    let result;
    try {
      result = await api.updateRef({ branch, before, after: update.after });
    } catch (error) {
      if (!(error instanceof UnknownOutcome) && !(error instanceof ApiError && error.status >= 500)) throw error;
      // The outcome is unknown: read the ref again instead of resending.
      const now = await api.getRefOid(branch);
      if ((now ?? ZERO_OID) === update.after) result = { ok: true, message: '' };
      else result = { ok: false, message: `outcome unknown (${error.message})`, unknown: true };
    }
    if (result.ok) {
      const outcome = update.after === ZERO_OID ? 'deleted' : head ? 'updated' : 'created';
      return { outcome, commit: update.commit, attempts: attempt, previous: head };
    }
    // Rejected: it is a lost race only when the ref moved. Otherwise the update itself is refused.
    const now = await api.getRefOid(branch);
    if ((now ?? ZERO_OID) === before && !result.unknown) {
      throw new EvidenceError(`the ref update was refused although ${branch} did not move: ${result.message}`);
    }
    log(`attempt ${attempt}: ${branch} moved (${result.message}); rebuilding from its current tree`);
  }
  throw new EvidenceError(`${branch} kept changing; gave up after ${maxAttempts} attempts`);
}

function sameBlobs(a, b) {
  if (a.size !== b.size) return false;
  for (const [path, entry] of a) {
    const other = b.get(path);
    if (!other || other.sha !== entry.sha || other.mode !== entry.mode) return false;
  }
  return true;
}
