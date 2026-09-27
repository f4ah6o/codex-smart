# codex-smart

Usage-aware launcher for the OpenAI Codex CLI.

`codex-smart` routes each new Codex process between two isolated credential homes:

- **API** — use an OpenAI API project while its complimentary daily-token budget is comfortably available.
- **ChatGPT** — fall back to your normal ChatGPT subscription before reaching the API quota edge.

The router checks the organization Usage API at launch. It is intentionally fail-closed for paid API usage: if usage cannot be read, `OPENAI_ADMIN_KEY` is missing, or `OPENAI_PROJECT_ID` is missing, auto mode chooses ChatGPT rather than guessing.

## Why Rust

Shell is attractive for a tiny prototype, but this wrapper needs HTTP, JSON, UTC quota boundaries, secret isolation, process replacement, and Windows/macOS/Linux behavior. Rust keeps those in one binary without requiring `curl` + `jq` + shell-specific code.

MoonBit can implement the policy too, but Rust currently has the better fit for a small security-sensitive cross-platform CLI.

## Important limitation

OpenAI evaluates the complimentary quota per request. If one request crosses the quota, that entire request is billed. `codex-smart` therefore leaves a configurable reserve instead of trying to consume the last token.

The check happens when Codex starts. A long-lived interactive Codex process can keep making API requests after the threshold. This first version is therefore a **session-start guard**, not a per-request proxy. Use a conservative reserve and restart through `codex-smart` between work sessions when you want tighter control.

OpenAI says the complimentary-token counter resets at **00:00 UTC** each day.

## Setup

Keep the normal ChatGPT subscription login in the default Codex home:

```sh
CODEX_HOME="$HOME/.codex" codex login
```

Create a separate API-key Codex home:

```sh
mkdir -p "$HOME/.codex-api-free"
printf '%s' "$OPENAI_API_KEY" |
  CODEX_HOME="$HOME/.codex-api-free" codex login --with-api-key
unset OPENAI_API_KEY
```

Use a **dedicated API project** for codex-smart. Enable the eligible input/output sharing setting on that project, then export an organization Admin API key for the Usage API lookup and the project ID:

```sh
export OPENAI_ADMIN_KEY='sk-admin-...'
export OPENAI_PROJECT_ID='proj_...'
```

The Admin key, project ID, `OPENAI_API_KEY`, and `CODEX_API_KEY` are removed from the child Codex environment. The selected `CODEX_HOME/auth.json` remains the credential source.

Install:

```sh
cargo install --git https://github.com/f4ah6o/codex-smart
```

Use it like Codex:

```sh
codex-smart
codex-smart exec "fix the failing tests"
codex-smart --smart-status
```

Ordinary arguments are passed through. codex-smart-only options use the `--smart-` prefix.

## Default policy

The conservative default targets usage tiers 1-2 of the large-model complimentary pool:

```text
quota   = 250,000 tokens/day
reserve =  50,000 tokens
cutoff  = 200,000 tokens
```

At launch:

```text
eligible project usage < cutoff
    -> API CODEX_HOME

eligible project usage >= cutoff
    -> ChatGPT CODEX_HOME

Usage API unavailable
    -> ChatGPT CODEX_HOME
```

For higher usage tiers or the small-model pool, set your quota explicitly.

## Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `OPENAI_ADMIN_KEY` | Organization Admin key used only to read Usage API | required for auto |
| `OPENAI_PROJECT_ID` | Dedicated API project whose usage is counted | required for auto |
| `CODEX_SMART_API_HOME` | API-key Codex home | `~/.codex-api-free` |
| `CODEX_SMART_CHATGPT_HOME` | ChatGPT Codex home | `$CODEX_HOME` or `~/.codex` |
| `CODEX_SMART_QUOTA_TOKENS` | Daily token quota | `250000` |
| `CODEX_SMART_RESERVE_TOKENS` | Safety margin before quota | `50000` |
| `CODEX_SMART_MODELS` | Optional comma-separated model filter | all completion models in project |
| `CODEX_SMART_CODEX_BIN` | Codex executable | `codex` |
| `CODEX_SMART_QUIET` | Suppress route message | unset |

Examples:

```sh
# See today's decision without launching Codex
codex-smart --smart-status

# Force either route
codex-smart --smart-mode api
codex-smart --smart-mode chatgpt

# Higher-tier example
codex-smart --smart-quota 1_000_000 --smart-reserve 100_000

# Only count specific models if the project is not completely dedicated
export CODEX_SMART_MODELS='gpt-5.6-sol,gpt-5.1-codex,gpt-5-codex'
```

## Safety properties

- Usage lookup failure -> ChatGPT.
- Missing usage credentials -> ChatGPT.
- Admin key is never inherited by Codex.
- API-key environment overrides are removed before launching Codex.
- API and ChatGPT auth files are not copied or symlinked.
- `--smart-mode api` is an explicit escape hatch and bypasses the usage guard.
- The default reserve intentionally leaves free quota unused to reduce crossing risk.

## API

The router queries:

```text
GET /v1/organization/usage/completions
```

for the current UTC day and filters by `OPENAI_PROJECT_ID`. It sums input + output tokens. If `CODEX_SMART_MODELS` is set, the request is additionally filtered to those models.

Because the no-model-filter default counts all completion usage in the dedicated project, unrelated completion traffic can only make the router switch to ChatGPT *earlier*. For accurate routing, keep the project dedicated.

Official references:

- OpenAI Usage API: https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/usage/methods/completions
- Complimentary tokens/data sharing: https://help.openai.com/en/articles/10306912-sharing-feedback-evaluation-and-fine-tuning-data-and-api-inputs-and-outputs-with-openai

## License

MIT
