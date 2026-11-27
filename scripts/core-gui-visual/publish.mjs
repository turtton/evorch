// The writer of the screen-diff evidence: publishes the PNGs of the changed screens to
// the evidence branch and keeps the pull request's one comment up to date.
//
//   node publish.mjs --artifact <dir> --relevant true|false    (opened/synchronize/reopened)
//   node publish.mjs                                           (closed)
//
// Reads the event from GITHUB_EVENT_PATH; GH_TOKEN, GITHUB_REPOSITORY, GITHUB_SERVER_URL
// and GITHUB_RUN_ID come from the job. It runs no cargo and nothing from the PR: it
// reads the artifact's JSON and PNG bytes as data, after checking each file.
import { createHash } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { GitHubApi } from './github-api.mjs';
import { replacePrDirectory, prDirName } from './evidence-store.mjs';
import { findOurs, renderClosed, renderIrrelevant, renderResult } from './comment.mjs';
import { canWrite, decideMode } from './policy.mjs';

export const DEFAULT_BRANCH = 'gui-visual-evidence';
const MAX_FILE_BYTES = 16 * 1024 * 1024;
const MAX_ENTRIES = 32;
const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const SHA1 = /^[0-9a-f]{40}$/;
const ROLES = { changed: ['before', 'after', 'diff'], added: ['after'], removed: ['before'], unchanged: [] };

export class ArtifactError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ArtifactError';
  }
}

function readBounded(path) {
  let size;
  try {
    size = statSync(path).size;
  } catch (error) {
    throw new ArtifactError(`cannot read ${path}: ${error.message}`);
  }
  if (size > MAX_FILE_BYTES) throw new ArtifactError(`${path} is larger than ${MAX_FILE_BYTES} bytes`);
  return readFileSync(path);
}

/**
 * Reads the capture artifact as data. Nothing is taken from it that a fixed rule does
 * not allow: file names are `<id>-<role>-<12 hex of the content's sha256>.png`, checked
 * against the bytes; the report's shape is checked; counts are bounded.
 * @returns {{report, meta, files: Map<string, Buffer>}} `files` holds the PNGs to publish
 */
export function loadArtifact(dir) {
  let meta;
  let report;
  try {
    meta = JSON.parse(readBounded(join(dir, 'meta.json')));
    report = JSON.parse(readBounded(join(dir, 'compare', 'report.json')));
  } catch (error) {
    if (error instanceof ArtifactError) throw error;
    throw new ArtifactError(`the artifact's JSON is unreadable: ${error.message}`);
  }
  if (!SHA1.test(meta.before_sha ?? '') || !SHA1.test(meta.after_sha ?? '') || typeof meta.baseline !== 'boolean') {
    throw new ArtifactError('meta.json is malformed');
  }
  if (report.schema !== 1 || typeof report.baseline !== 'boolean' || !Array.isArray(report.entries)) {
    throw new ArtifactError('report.json is malformed or of an unknown schema');
  }
  if (report.baseline !== meta.baseline) throw new ArtifactError('meta.json and report.json disagree about the baseline');
  if (report.entries.length === 0 || report.entries.length > MAX_ENTRIES) {
    throw new ArtifactError(`report.json lists ${report.entries.length} screens`);
  }
  const files = new Map();
  const seen = new Set();
  for (const entry of report.entries) {
    if (!/^[a-z0-9-]{1,64}$/.test(entry.id ?? '') || seen.has(entry.id)) throw new ArtifactError(`bad or repeated screen id ${JSON.stringify(entry.id)}`);
    seen.add(entry.id);
    const roles = ROLES[entry.status];
    if (!roles) throw new ArtifactError(`unknown status ${JSON.stringify(entry.status)} for ${entry.id}`);
    if (entry.status === 'changed' && !(Number.isInteger(entry.changed_pixels) && entry.changed_pixels > 0)) {
      throw new ArtifactError(`${entry.id} is changed without a pixel count`);
    }
    for (const role of ['before', 'after', 'diff']) {
      const name = entry[role];
      if (!roles.includes(role)) {
        if (name != null) throw new ArtifactError(`${entry.id} (${entry.status}) must not have a ${role} image`);
        continue;
      }
      const match = typeof name === 'string' && new RegExp(`^${entry.id}-${role}-([0-9a-f]{12})\\.png$`).exec(name);
      if (!match) throw new ArtifactError(`${entry.id} has a bad ${role} file name ${JSON.stringify(name)}`);
      const bytes = readBounded(join(dir, 'compare', name));
      if (!bytes.subarray(0, 8).equals(PNG_SIGNATURE)) throw new ArtifactError(`${name} is not a PNG`);
      if (createHash('sha256').update(bytes).digest('hex').slice(0, 12) !== match[1]) throw new ArtifactError(`${name} does not match its hash`);
      files.set(name, bytes);
    }
  }
  return { report, meta, files };
}

/**
 * One event's work. `api` is a GitHubApi (or a fake). Returns a description of what happened.
 */
export async function execute({ api, repo, branch = DEFAULT_BRANCH, pr, mode, loadCapture, runUrl, log = () => {}, sleep }) {
  const urlFor = (name) => `https://raw.githubusercontent.com/${repo}/${branch}/${prDirName(pr.number)}/${name}`;
  const isClosed = async (number) => (await api.getPull(number)).state === 'closed';
  const store = (files) => replacePrDirectory({ api, branch, pr: pr.number, files, isClosed, log, sleep });

  const live = await api.getPull(pr.number);
  let effective = mode;
  // `publish` and `irrelevant` both change the public images and the comment, so both are
  // only for the head this run was started for. `cleanup` (a closed event) is not.
  const changesState = mode === 'publish' || mode === 'irrelevant';
  if (changesState && live.state !== 'open') {
    log(`#${pr.number} is ${live.state}: nothing is published`);
    effective = 'cleanup';
  }
  if (effective === 'cleanup' && live.state === 'open') {
    log(`#${pr.number} was reopened: its images stay`);
    return { outcome: 'reopened', comment: null };
  }
  if (changesState && live.headSha !== pr.headSha) {
    log(`#${pr.number} head is ${live.headSha}, this run captured ${pr.headSha}: not published`);
    return { outcome: 'stale', comment: null };
  }

  const existing = findOurs(await api.listComments(pr.number));
  const comment = existing[0] ?? null;
  const upsert = async (body, { create }) => {
    if (comment) {
      if (comment.body === body) return { id: comment.id, action: 'unchanged' };
      await api.updateComment(comment.id, body);
      return { id: comment.id, action: 'updated' };
    }
    if (!create) return null;
    const made = await api.createComment(pr.number, body);
    return { id: made.id, action: 'created' };
  };

  if (effective === 'cleanup') {
    const stored = await store(null);
    return { outcome: 'closed', stored, comment: await upsert(renderClosed({ runUrl }), { create: false }) };
  }
  if (effective === 'irrelevant') {
    const stored = await store(null);
    return { outcome: 'irrelevant', stored, comment: await upsert(renderIrrelevant({ pr, runUrl }), { create: false }) };
  }

  const { report, meta, files } = loadCapture();
  const hasShown = report.entries.some((entry) => entry.status !== 'unchanged');
  // The directory is replaced before the comment is: a comment never points at images that are not there.
  const stored = await store(hasShown ? files : null);
  const body = renderResult({ report, meta, pr: { ...pr, baseRef: live.baseRef }, runUrl, urlFor });
  return { outcome: hasShown ? 'published' : 'no-difference', stored, comment: await upsert(body, { create: true }) };
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith('--')) throw new Error(`unexpected argument ${argv[i]}`);
    args[argv[i].slice(2)] = argv[i + 1];
  }
  return args;
}

export async function main(argv, env) {
  const args = parseArgs(argv);
  const event = JSON.parse(readFileSync(env.GITHUB_EVENT_PATH, 'utf8'));
  const repo = env.GITHUB_REPOSITORY;
  const pull = event.pull_request;
  if (!canWrite(pull, repo)) {
    console.log('This pull request does not come from this repository: nothing is written.');
    return 0;
  }
  const relevant = { true: true, false: false }[args.relevant];
  const mode = decideMode(event.action, relevant);
  const api = new GitHubApi({ token: env.GH_TOKEN, repo, apiUrl: env.GITHUB_API_URL });
  const runUrl = `${env.GITHUB_SERVER_URL}/${repo}/actions/runs/${env.GITHUB_RUN_ID}`;
  const result = await execute({
    api,
    repo,
    branch: env.EVIDENCE_BRANCH || DEFAULT_BRANCH,
    pr: { number: pull.number, headSha: pull.head.sha, baseRef: pull.base.ref },
    mode,
    loadCapture: () => loadArtifact(args.artifact),
    runUrl,
    log: (line) => console.log(line),
  });
  console.log(JSON.stringify(result, (key, value) => (key === 'commit' && value ? { sha: value.sha, parents: value.parents.length, verified: value.verified, reason: value.reason } : value), 2));
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2), process.env).then(
    (code) => process.exit(code),
    (error) => {
      console.error(`publish: ${error.stack ?? error}`);
      process.exit(1);
    },
  );
}
