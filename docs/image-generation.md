# Image generation

`generate_image` creates one PNG or edits it using up to five reference images.
It is an optional provider capability, not a hosted chat tool and not `codex exec`
([ADR 0038](../intents/evorch/decisions/0038-image-generation.md)).

## Availability and routing

Only General-purpose visual Workers and the conversation's root Worker
(`conversation` category) receive it, exactly like `render_artifact`, and only
when `agents.roles.image_generator.models` is nonempty at run start. A child
conversation Worker and any Orchestrator do not receive it. A root Orchestrator
should delegate materials and HTML mocks to a visual Worker and `present` the
returned artifact IDs. Dispatch rechecks the conditions. The image chain and
tool definition are fixed for that run; live configuration reloads affect new
runs, not this tool's availability or routing.

Configure existing authenticated profiles, then add image models and routes,
for example (merge these entries with your existing chat configuration):

```toml
# Both profiles may also contain chat models. Image models cannot serve chat roles.
[providers.codex]
type = "openai-codex"
models = ["gpt-6-astra", { id = "gpt-image-2", enabled = true, kind = "image_generation" }]
default_model = "gpt-6-astra"
# Retain the credential setting created by ChatGPT browser sign-in.

[providers.openai]
type = "openai"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-6-astra", { id = "gpt-image-2", enabled = true, kind = "image_generation" }]
default_model = "gpt-6-astra"

[[routing.routes.codex-image]]
profile = "codex"
model = "gpt-image-2"
[[routing.routes.openai-image]]
profile = "openai"
model = "gpt-image-2"

[agents.roles.image_generator]
models = [{ route = "codex-image" }, { route = "openai-image" }]
```

Use the profile's actual supported image model. The `gpt-image-`,
`chatgpt-image-`, and `dall-e-` prefixes infer `image_generation`; explicit `kind`
is useful for custom IDs. An empty chain disables generation without an implicit
route. All candidates must have the image model kind and an image capability.
Only `openai-codex` and `openai` clients expose that capability; an
`openai-compatible` profile does not.

Codex uses the existing ChatGPT session, account ID, refreshed tokens, and shared
Codex headers with `/backend-api/codex/images/generations` or `/images/edits`.
OpenAI uses its API key with `{base_url}/images/generations` or `/images/edits`
(default base `https://api.openai.com/v1`) and requests PNG output. References
select edits; no references select generation. No masks or multiple-image
requests are supported.

Authentication, quota, rate limiting, temporary failures and unsupported
endpoints (404/405) advance the chain. Routing health and Retry-After handling
are shared with chat: auth pauses the profile until recomposition; quota and
429 use the existing cooldown policy (including Retry-After for quota).
Other image failures, including server/timeout/transport errors and unsupported
404/405 endpoints, pause only the image candidate `(profile, model)` for **60
seconds**. This additional in-memory health registry uses the existing router
and fallback scan, expires at the deadline, and resets on runtime recomposition.
It is never attached to the chat router: even the same profile/model's healthy
chat path remains available. A rejected or invalid request stops immediately
and returns its reason; it neither cools down nor attempts to evade safety decisions by falling
back. Image generation does not change the chat chain's pin. Usage is recorded
once by the provider under `ImageGeneration`, never again by the runtime.

## Arguments and files

| Argument | Meaning |
| --- | --- |
| `prompt` | Required nonempty instruction |
| `output_path` | Required `.png` path; existing regular files are overwritten |
| `reference_image_paths` | Optional array, up to five PNG/JPEG/GIF/WebP images |
| `transparent_background` | Optional boolean, default false |
| `variant` | Optional `{ "max_side": 64..2048, "format": "jpeg" or "png" }` |
| `size`, `quality` | Optional provider strings; Codex may ignore them |

Both output and reference files must resolve inside the run's workspace or its
thread's `$TMPDIR`. Relative paths start at the workspace; use the announced
absolute `$TMPDIR` path for temporary material. Other threads' scratch and
allowed directories are not eligible. Parents must already exist. Reads reuse
the artifact openat boundary, bound the read to 8 MiB per reference, and check
extension/magic and decode validity. References larger than 2048 pixels on the
longest side are downscaled to PNG before transmission. Output writes validate
the mutation ownership permit and writable boundary, and use a temporary file
and rename in the destination directory. Symlinks cannot redirect writes.
Team Workers must have their own unexpired path claims for both the original
and the derived variant, just as for `write`. Claims are checked before the
paid generation request and again immediately before publication. Absolute
workspace paths use the same workspace-relative claims; thread scratch paths
are not claimable by team Workers.

The variant is `<stem>.<max_side>.jpg` or `.png` beside the original, never
upscaled, with JPEG quality 85. JPEG with `transparent_background: true` is
rejected. A result contains paths, original width/height, selected profile/model,
a reference-transmission flag and note, and a PNG preview at most 1024 pixels on
its longest side. Image-capable chat models receive the preview through the
normal tool-result image path; non-image models get a saved-path note and can
delegate inspection. The saved original is retained either way. If the original
is published but the variant rename fails, the tool returns an error with
`status: "partial_save"`, the saved original `path`, and `variant_failure`
containing the failed path and reason; `variant_path` is null. Generation
metadata and the preview are still included. The original is retained (including
when overwriting), and any previous variant is unchanged.

Cancellation aborts an in-flight request. Ordinary run cancellation and subtree
cancellation share a per-run publication gate: cancellation that wins the gate
prevents both files from being published and removes staged files. If publication
wins, it finishes before cancellation takes effect; already published files are
not rolled back. Provider responses are limited to 48 MiB and five minutes.

## Sending, quota, and HTML mocks

**Generation/editing requests and all supplied reference images are sent to
OpenAI**, including on the Codex login path. No additional approval is requested.
Use references only when suitable for sending there. The tool description and
result disclose the transmission. Image generation consumes substantial usage
quota; there is no additional per-tool budget cap.

Choose whether to adopt the output or merely use it as reference. Build the UI
itself in HTML; never substitute a generated screenshot for a UI mock. For
example, generate `hero.png` with a 512px JPEG variant, reference
`<img src="hero.512.jpg">` in `mock.html` in the same directory, then call
`render_artifact` on `mock.html`. It embeds the relative image so the captured
artifact is self-contained. Reuse the previous image as a reference for a
consistent series. Keep reference-only experiments in `$TMPDIR`.
See [Conversation artifacts](conversation-artifacts.md).

## Unverified live-account behavior

Offline tests do not establish actual account access, billing/quota changes,
or legal permission. Codex depends on a private endpoint that can change.
Manual checks remain necessary for generation/editing with ChatGPT login and
OpenAI API keys; whether evorch's Codex headers are accepted; whether Codex
honors `model`, `size`, and `quality`; and the terms governing use of the ChatGPT
login image endpoint by clients other than Codex. Do not infer these from mock
successes.
