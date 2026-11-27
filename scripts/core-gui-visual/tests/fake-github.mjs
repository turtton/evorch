// An in-memory GitHub with the surface of GitHubApi, for the writer's tests. Hooks let a
// test move the ref, lose a reply or leave a commit unsigned at the exact point it matters.
import { createHash } from 'node:crypto';
import { ApiError, UnknownOutcome, ZERO_OID } from '../github-api.mjs';

const oidOf = (value) => createHash('sha1').update(JSON.stringify(value)).digest('hex');

export class FakeGitHub {
  constructor() {
    this.blobs = new Map();
    this.trees = new Map();
    this.commits = new Map();
    this.refs = new Map();
    this.pulls = new Map();
    this.comments = [];
    this.nextCommentId = 1;
    this.calls = [];
    // behaviour switches
    this.signed = true;
    this.lateVerification = 0; // getCommit reports unverified this many times first
    this.beforeUpdate = null; // (branch, before, after) => void, runs before the compare
    this.afterUpdate = null; // (applied) => void, may throw to lose the reply
    this.refuse = null; // message: every update is refused without the ref moving
    this.pullErrors = new Set();
  }

  #log(name) {
    this.calls.push(name);
  }

  setPull(number, fields) {
    this.pulls.set(number, { number, state: 'open', merged: false, headSha: 'h'.repeat(40), headRepo: 'o/r', baseRef: 'main', ...fields });
  }

  async getRefOid(branch) {
    this.#log('getRefOid');
    return this.refs.get(branch) ?? null;
  }

  async getCommit(sha) {
    this.#log('getCommit');
    const commit = this.commits.get(sha);
    if (!commit) throw new ApiError(404, 'no such commit');
    let verified = commit.verified;
    if (!verified && commit.pending > 0) {
      commit.pending -= 1;
      verified = commit.pending === 0 ? true : false;
      if (commit.pending === 0) commit.verified = true;
    }
    return { sha, tree: commit.tree, parents: [...commit.parents], verified, reason: verified ? 'valid' : 'unsigned' };
  }

  async getTreeBlobs(tree) {
    this.#log('getTreeBlobs');
    const entries = this.trees.get(tree);
    if (!entries) throw new ApiError(404, 'no such tree');
    return new Map(entries);
  }

  async createBlob(buffer) {
    this.#log('createBlob');
    const sha = oidOf(['blob', buffer.toString('base64')]);
    this.blobs.set(sha, buffer);
    return sha;
  }

  async createTree(entries) {
    this.#log('createTree');
    if (entries.length === 0) throw new ApiError(422, 'Invalid tree info');
    const map = new Map(entries.map(({ path, mode, sha }) => [path, { mode, sha }]));
    const sha = oidOf(['tree', [...map]]);
    this.trees.set(sha, map);
    return sha;
  }

  async createCommit({ tree, message }) {
    this.#log('createCommit');
    const sha = oidOf(['commit', tree, message, this.commits.size]);
    const pending = !this.signed ? 0 : this.lateVerification;
    this.commits.set(sha, { tree, parents: [], verified: this.signed && pending === 0, pending });
    return { sha, tree, parents: [], verified: this.signed && pending === 0, reason: this.signed ? 'valid' : 'unsigned' };
  }

  async updateRef({ branch, before, after }) {
    this.#log('updateRef');
    this.beforeUpdate?.(branch, before, after);
    const current = this.refs.get(branch) ?? ZERO_OID;
    if (this.refuse) return { ok: false, message: this.refuse };
    if (current !== before) return { ok: false, message: 'ref moved' };
    if (after === ZERO_OID) this.refs.delete(branch);
    else this.refs.set(branch, after);
    this.afterUpdate?.(true);
    return { ok: true, message: '' };
  }

  async getPull(number) {
    this.#log('getPull');
    if (this.pullErrors.has(number)) throw new ApiError(500, 'boom');
    const pull = this.pulls.get(number);
    if (!pull) throw new ApiError(404, 'no such pull');
    return { ...pull };
  }

  async listComments(number) {
    this.#log('listComments');
    return this.comments.filter((c) => c.issue === number).map((c) => ({ ...c }));
  }

  async createComment(number, body) {
    this.#log('createComment');
    const comment = { id: this.nextCommentId++, issue: number, body, user: { login: 'github-actions[bot]', type: 'Bot' } };
    this.comments.push(comment);
    return { ...comment };
  }

  async updateComment(id, body) {
    this.#log('updateComment');
    const comment = this.comments.find((c) => c.id === id);
    comment.body = body;
    return { ...comment };
  }

  /** A comment written by somebody else, possibly copying the marker. */
  addComment(issue, body, user = { login: 'someone', type: 'User' }) {
    const comment = { id: this.nextCommentId++, issue, body, user };
    this.comments.push(comment);
    return comment;
  }

  /** The paths of the ref's tree, for assertions. */
  async paths(branch) {
    const oid = this.refs.get(branch);
    return oid ? [...this.trees.get(this.commits.get(oid).tree).keys()].sort() : null;
  }
}

export { UnknownOutcome };
