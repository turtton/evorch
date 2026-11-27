// The one comment per pull request: a fixed marker, the bot as author, updated in place.

export const MARKER = '<!-- evorch:core-gui-visual:v1 -->';
export const BOT_LOGIN = 'github-actions[bot]';
const IMAGE_WIDTH = 320;

const short = (sha) => sha.slice(0, 12);

/** The comment of this tool: the bot's own, carrying the marker. A copy of the marker by someone else is not. */
export function isOurs(comment) {
  return comment.user?.login === BOT_LOGIN && comment.user?.type === 'Bot' && typeof comment.body === 'string' && comment.body.startsWith(MARKER);
}

export function findOurs(comments) {
  return comments.filter(isOurs).sort((a, b) => a.id - b.id);
}

function image(url, label) {
  return `<a href="${url}"><img src="${url}" width="${IMAGE_WIDTH}" alt="${label}"></a>`;
}

function comparison({ meta, pr, runUrl, baseRef }) {
  const sides = meta.baseline
    ? `base \`${baseRef}\` の \`${short(meta.before_sha)}\`（変更前）と merge 結果 \`${short(meta.after_sha)}\`（変更後）`
    : `merge 結果 \`${short(meta.after_sha)}\`（base \`${baseRef}\` の \`${short(meta.before_sha)}\` には撮影ツールがありません）`;
  return `比較: ${sides}。PR head \`${short(pr.headSha)}\`。[run](${runUrl})（artifact は run のページにあります）`;
}

/**
 * @param report   the compare report (validated)
 * @param urlFor   (file name) => the public URL of an image
 */
export function renderResult({ report, meta, pr, runUrl, urlFor }) {
  const lines = [MARKER, '### 新 GUI の画面差分', ''];
  const shown = report.entries.filter((entry) => entry.status !== 'unchanged');
  if (!report.baseline) {
    lines.push(`**baseline なし**: base に撮影ツールがないため、全 ${report.entries.length} 画面が新規です（差分なしではありません）。`, '');
  } else if (shown.length === 0) {
    lines.push('画面差分なし', '');
  } else {
    lines.push(`変更のあった画面: ${shown.length} / ${report.entries.length}`, '');
  }
  if (report.baseline && report.fixture_changed) {
    lines.push('撮影 fixture 自体が変わっています（意図した表示内容の変更を含みます）。', '');
  }
  for (const entry of shown) {
    const title = { changed: `${entry.id}（${entry.changed_pixels.toLocaleString('en-US')} px 変更）`, added: `${entry.id}（新規）`, removed: `${entry.id}（削除）` }[entry.status];
    lines.push(`<details open><summary>${title}</summary>`, '');
    if (entry.status === 'changed') {
      lines.push('| 変更前 | 変更後 | 差分 |', '|---|---|---|');
      lines.push(`| ${image(urlFor(entry.before), '変更前')} | ${image(urlFor(entry.after), '変更後')} | ${image(urlFor(entry.diff), '差分')} |`);
    } else if (entry.status === 'added') {
      lines.push(image(urlFor(entry.after), '変更後'));
    } else {
      lines.push(image(urlFor(entry.before), '変更前'));
    }
    lines.push('', '</details>', '');
  }
  lines.push(comparison({ meta, pr, runUrl, baseRef: pr.baseRef }));
  return `${lines.join('\n')}\n`;
}

export function renderIrrelevant({ pr, runUrl }) {
  return `${[MARKER, '### 新 GUI の画面差分', '', '画面差分なし（対象外の変更のみ）', '', `PR head \`${short(pr.headSha)}\`。[run](${runUrl})`].join('\n')}\n`;
}

export function renderClosed({ runUrl }) {
  return `${[
    MARKER,
    '### 新 GUI の画面差分',
    '',
    'この PR は閉じられたため、公開していた画像を削除しました（保存終了）。撮影結果の artifact は保持期間（7 日）のあいだ、各 run のページから参照できます。',
    '',
    `[最後の run](${runUrl})`,
  ].join('\n')}\n`;
}
