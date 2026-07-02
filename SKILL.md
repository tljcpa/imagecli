---
name: imagecli
description: >-
  Unified multi-provider image generation CLI. Use when an agent needs to
  generate images from text prompts across providers (fal, google/Gemini,
  agnes/OpenAI-compatible), track async jobs, and download artifacts, with a
  stable --json contract and meaningful exit codes. Image-to-image, video, and
  PRC providers are planned, not yet implemented.
---

# imagecli (agent guide)

`imagecli` is a Rust CLI that puts many image-generation backends behind one set
of commands. The orchestration layer (batching, bounded concurrency, exponential
backoff with jitter, SQLite job persistence, stable `--json`, exit-code contract)
is the value; model quality comes from each provider's public API.

## When to use
- Generate an image from a text prompt (`text2image`) via a chosen provider.
- Run small batches with bounded concurrency.
- Track an async job across processes (`status`, `list`) and fetch outputs
  (`download`) — the fal queue path is async; google and agnes are synchronous
  and return a terminal job immediately.

Do NOT assume image-to-image with local files, video, or mainland-China
providers work yet — they are planned (see "Limits").

## Default workflow
1. Discover capabilities first. Never guess flags.
   - `imagecli providers --json` — registered providers and their capabilities.
   - `imagecli models --provider <p> --json` — known/default models.
   - `imagecli generate --help` — authoritative flag list.
2. Generate (always pass `--json` for machine-parseable output):
   - `imagecli generate --provider agnes --prompt "a red fox in snow" --json`
   - Default provider is `fal` (needs a paid key). For zero-friction generation
     prefer `agnes` (free tier) or `google` (Gemini, free tier on AI Studio key).
   - Default model is chosen per provider/capability if `--model` is omitted.
   - `--concurrency N` (default 4) controls bounded parallelism; batching is by
     multiple requests through the orchestrator, not a single-request `n`.
3. For async jobs that are not yet terminal, follow up across processes:
   - `imagecli status <job_id> --provider <p> --json` (refreshes from provider,
     writes back to the local store).
   - `imagecli list --status running --json` to enumerate.
4. Fetch artifacts:
   - By default `generate` already downloads into `--out-dir` (default `./out`).
     Pass `--no-download` to skip.
   - `imagecli download --job-id <id> --out-dir ./out --json` pulls a succeeded
     job's outputs from the store; or `--url <URL>` to download a link directly.

## Credentials (environment variables only)
Keys are read from env vars (project-namespaced var has priority), with a system
keyring fallback. The agent must never write keys into files or pass them on the
command line; set them in the environment of the process.

| provider | env var candidates (highest priority first) |
| -------- | -------------------------------------------- |
| fal      | `IMAGECLI_FAL_KEY`, `FAL_KEY`                 |
| google   | `IMAGECLI_GOOGLE_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| agnes    | `AGNES_API_KEY`, `IMAGECLI_AGNES_KEY`        |

A missing key produces a clear (Chinese) error and a non-zero exit — do not
retry blindly; surface the missing-key message.

## Cost awareness
Generation can consume provider quota/credits. There is no `--dry-run` or budget
guard yet (planned). Before any larger run: generate ONE image first, confirm the
provider/model/params are right, then scale up. Do not fan out a big batch
unconfirmed. `agnes` free tier is rate-limited (~30 RPM), so keep `--concurrency`
modest; the orchestrator already backs off and retries on the polling path.

## Judging success (do not parse prose)
- Exit code is the contract: `0` = all jobs succeeded; non-zero = at least one
  job failed or errored (including a provider returning a terminal `failed` job).
- With `--json`, read the structured fields, not stdout text:
  - `generate` -> `{ "results": [ { "status", "saved": [paths], "error", ... } ] }`
  - `status` -> a job object with `status` in `queued|running|succeeded|failed`.
  - `list` -> `{ "jobs": [...] }`; `download` -> `{ "saved": [paths] }`.
- Treat `status == "succeeded"` plus a non-empty `saved` array as done.

## Limits (current MVP — do not invent capabilities)
- Only `text2image` is implemented; all three providers declare just that. The
  `capability` enum lists image2image/video/upscale but no provider serves them.
- `--input` accepts a local image path OR an http(s) URL. Local files are read
  and base64-inlined per provider: jimeng / volcengine (Seedream) serve
  `image2image`, kling serves `image2video` with a local image. fal / replicate
  still need a pre-uploaded URL (storage upload not implemented).
- No mainland-China providers (Volcengine/SiliconFlow/etc.) yet — planned.
- New OpenAI-compatible services are added by config, not code, but only ones
  already registered are usable now.

Defer to `imagecli <command> -h` for exact, current flags.
