// Node runner for tests/web/unit.mjs (DOM tests are skipped):
//   node --experimental-detect-module crates/workbench-web/tests/web/run-node.mjs
// CI runs the same tests in Chromium (tests/web_ui_chromium.rs).

import { run } from "./unit.mjs";

const result = await run({ hasDom: false });
for (const failure of result.failures) console.error(`FAIL ${failure.name}\n${failure.message}\n`);
console.log(`passed ${result.passed}, skipped ${result.skipped} (need a DOM), failed ${result.failures.length}`);
process.exit(result.failures.length === 0 ? 0 : 1);
