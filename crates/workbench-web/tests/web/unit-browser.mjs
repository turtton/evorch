// Runs tests/web/unit.mjs in the browser and publishes the result as a body
// attribute, which the end-to-end test waits for with a MutationObserver.

import { run } from "./unit.mjs";

const result = await run();
document.body.dataset.result = JSON.stringify(result);
