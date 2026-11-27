// A minimal GitHub REST/GraphQL client for the evidence writer. Node built-ins
// only: the writer job runs no cargo, no PR build output and installs no
// third-party package.

export class ApiError extends Error {
  constructor(status, message, body) {
    super(`GitHub API ${status}: ${message}`);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
  }
}

/** The request may or may not have been applied (timeout, connection loss). */
export class UnknownOutcome extends Error {
  constructor(message) {
    super(message);
    this.name = 'UnknownOutcome';
  }
}

export const ZERO_OID = '0'.repeat(40);

export class GitHubApi {
  constructor({ token, repo, apiUrl = 'https://api.github.com', fetchImpl = globalThis.fetch, timeoutMs = 30000 }) {
    if (!token) throw new Error('a GitHub token is required');
    if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) throw new Error(`bad repository name: ${repo}`);
    this.token = token;
    this.repo = repo;
    this.apiUrl = apiUrl.replace(/\/$/, '');
    this.fetchImpl = fetchImpl;
    this.timeoutMs = timeoutMs;
  }

  async #send(method, url, body) {
    let response;
    try {
      response = await this.fetchImpl(url, {
        method,
        headers: {
          Authorization: `Bearer ${this.token}`,
          Accept: 'application/vnd.github+json',
          'X-GitHub-Api-Version': '2022-11-28',
          'Content-Type': 'application/json',
          'User-Agent': 'evorch-core-gui-visual',
        },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(this.timeoutMs),
      });
    } catch (error) {
      throw new UnknownOutcome(`${method} ${url}: ${error.message}`);
    }
    const text = await response.text();
    let data = null;
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
    }
    return { status: response.status, data };
  }

  /** Returns `{status, data}`; a status outside 2xx throws unless it is listed in `allow`. */
  async rest(method, path, body, { allow = [] } = {}) {
    const { status, data } = await this.#send(method, `${this.apiUrl}/repos/${this.repo}${path}`, body);
    if (status >= 200 && status < 300) return { status, data };
    if (allow.includes(status)) return { status, data };
    throw new ApiError(status, (data && data.message) || String(data), data);
  }

  async graphql(query, variables) {
    const { status, data } = await this.#send('POST', `${this.apiUrl}/graphql`, { query, variables });
    if (status < 200 || status >= 300) throw new ApiError(status, (data && data.message) || String(data), data);
    return data;
  }

  async repositoryNodeId() {
    const { data } = await this.rest('GET', '');
    return data.node_id;
  }

  async getRefOid(branch) {
    const { status, data } = await this.rest('GET', `/git/ref/heads/${encodeBranch(branch)}`, undefined, { allow: [404] });
    return status === 404 ? null : data.object.sha;
  }

  async getCommit(sha) {
    const { data } = await this.rest('GET', `/git/commits/${sha}`);
    return commitView(data);
  }

  /** Every blob of a tree as `Map<path, {mode, sha}>`. A truncated listing is an error, not a partial tree. */
  async getTreeBlobs(treeSha) {
    const { data } = await this.rest('GET', `/git/trees/${treeSha}?recursive=1`);
    if (data.truncated) throw new Error(`tree ${treeSha} listing was truncated`);
    const blobs = new Map();
    for (const entry of data.tree) {
      if (entry.type === 'blob') blobs.set(entry.path, { mode: entry.mode, sha: entry.sha });
      else if (entry.type !== 'tree') throw new Error(`unexpected tree entry ${entry.type} at ${entry.path}`);
    }
    return blobs;
  }

  async createBlob(buffer) {
    const { data } = await this.rest('POST', '/git/blobs', { content: buffer.toString('base64'), encoding: 'base64' });
    return data.sha;
  }

  /** `entries` is `[{path, mode, sha}]`, every one a blob; the tree has no base. */
  async createTree(entries) {
    const tree = entries.map(({ path, mode, sha }) => ({ path, mode, type: 'blob', sha }));
    const { data } = await this.rest('POST', '/git/trees', { tree });
    return data.sha;
  }

  /** A parentless commit with no author or committer, so that GitHub signs it as the token's identity. */
  async createCommit({ tree, message }) {
    const { data } = await this.rest('POST', '/git/commits', { message, tree, parents: [] });
    return commitView(data);
  }

  /**
   * Compare-and-swap of one ref through `updateRefs`: `before` is the value the ref
   * must have now (ZERO_OID: it must not exist), `after` the new value (ZERO_OID:
   * delete). `force` is only the permission for a non-fast-forward move; the
   * expectation is `beforeOid`. Returns `{ok, message}`.
   */
  async updateRef({ branch, before, after }) {
    this.repositoryId ??= await this.repositoryNodeId();
    const result = await this.graphql(
      'mutation($input: UpdateRefsInput!) { updateRefs(input: $input) { clientMutationId } }',
      {
        input: {
          repositoryId: this.repositoryId,
          refUpdates: [{ name: `refs/heads/${branch}`, beforeOid: before, afterOid: after, force: true }],
        },
      },
    );
    if (result.errors?.length) return { ok: false, message: result.errors.map((e) => e.message).join('; ') };
    return { ok: true, message: '' };
  }

  async getPull(number) {
    const { data } = await this.rest('GET', `/pulls/${number}`);
    return {
      number: data.number,
      state: data.state,
      merged: data.merged === true,
      headSha: data.head.sha,
      headRepo: data.head.repo?.full_name ?? null,
      baseRef: data.base.ref,
    };
  }

  async listComments(number) {
    const all = [];
    for (let page = 1; ; page += 1) {
      const { data } = await this.rest('GET', `/issues/${number}/comments?per_page=100&page=${page}`);
      all.push(...data);
      if (data.length < 100) return all;
    }
  }

  async createComment(number, body) {
    const { data } = await this.rest('POST', `/issues/${number}/comments`, { body });
    return data;
  }

  async updateComment(id, body) {
    const { data } = await this.rest('PATCH', `/issues/comments/${id}`, { body });
    return data;
  }
}

function commitView(data) {
  return {
    sha: data.sha,
    tree: data.tree.sha,
    parents: data.parents.map((p) => p.sha),
    verified: data.verification?.verified === true,
    reason: data.verification?.reason ?? 'unknown',
  };
}

export function encodeBranch(branch) {
  return branch.split('/').map(encodeURIComponent).join('/');
}
