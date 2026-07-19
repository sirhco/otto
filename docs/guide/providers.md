# Providers

How otto turns a `provider/model` string into a live route, how each provider
authenticates, and how to point otto at a gateway or a local model server.

## How routing works

`AuthRouteFactory::route_for` matches on the provider id and builds a native
route for five ids. **Every other id falls through to an OpenAI-compatible
route** at `{baseURL}/chat/completions`.

| Provider id | Route |
| --- | --- |
| `anthropic` | Native Anthropic Messages |
| `openai` | Native OpenAI Chat |
| `google`, `gemini` | Native Gemini |
| `vertex` | Vertex AI (requires `provider.vertex.options.project`) |
| `github-copilot` | Copilot |
| *anything else* | OpenAI-compatible |

<!-- src: crates/otto-app/src/route_factory.rs, impl RouteFactory for AuthRouteFactory — match provider { "anthropic" | "openai" | "google" | "gemini" | "vertex" | "github-copilot" | other => OpenAICompatible::new(other, base_url, key, transport) } -->

The fall-through arm is the path for litellm, Ollama, vLLM, OpenRouter, or any
other OpenAI-compatible endpoint. `config.provider.<id>.options.baseURL` supplies
the endpoint; with no override the base URL is empty and the route still carries
the correct protocol and model metadata (but will not reach anything).

### Model ids with slashes

`ModelRef::parse` splits on the **first** `/` only: everything before is the
provider id, everything after is the model id. Gateway-style model names survive
intact.
<!-- src: crates/otto-agent/src/agent.rs, ModelRef::parse — spec.split_once('/') -->

| Input | Provider | Model |
| --- | --- | --- |
| `anthropic/claude-opus-4-8` | `anthropic` | `claude-opus-4-8` |
| `litellm/github_copilot/claude-opus-4.8` | `litellm` | `github_copilot/claude-opus-4.8` |
| `llama3.1` | `""` (empty) | `llama3.1` |

A bare string with no `/` yields an empty provider id, which falls through to the
OpenAI-compatible arm with no configured base URL — always write
`provider/model`.

## Wire protocols

Five protocol modules decode provider SSE into `LLMEvent`s, over one shared HTTP
transport.

| Module | Used by |
| --- | --- |
| `anthropic_messages` | `anthropic` |
| `openai_chat` | `openai` |
| `openai_compatible` | every fall-through provider id |
| `openai_responses` | OpenAI Responses API models |
| `gemini` | `google`, `gemini`, `vertex` |

<!-- src: crates/otto-llm/src/protocols/{anthropic_messages,openai_chat,openai_compatible,openai_responses,gemini}.rs (plus copilot_cache.rs and utils/); transport in crates/otto-llm/src/transport/{mod,sse}.rs -->

## Auth per provider

| Provider | Login method | Refresh on load |
| --- | --- | --- |
| `anthropic` | API key **or** OAuth (PKCE S256, paste-code) | Yes |
| `github-copilot` | OAuth device code (polls, backs off on `SlowDown`) | No — token is long-lived and used directly |
| everything else | API key paste | n/a |

<!-- src: crates/otto-cli/src/commands.rs, login() — the anthropic [1]/[2] branch, the github-copilot device loop with `DevicePoll::SlowDown => interval += 5`, and the trailing `prompt(&format!("{provider} API key: "))` -->
<!-- src: crates/otto-auth/src/providers/mod.rs, Resolver::refresh() — match provider { "anthropic" => Ok(Some(client.refresh(...).await?)), _ => Ok(None) } -->

OAuth exists for exactly two providers. `Resolver::resolve` refreshes an OAuth
credential whose `expires` is at or near the current time and persists the new
tokens before use; only `anthropic` has a refresh flow, so a Copilot credential
is always handed back as-is.

An Anthropic OAuth credential is sent as a bearer token rather than `x-api-key`.
<!-- src: crates/otto-app/src/route_factory.rs — Anthropic::new_oauth for Credential::Oauth -->

```bash
otto auth login anthropic
otto auth login github-copilot
otto auth login openai
otto auth list                  # provider + [api key | oauth | wellknown | not logged in]
otto auth logout <provider>
```

`otto providers <cmd>` is the same implementation under a different name.
<!-- src: crates/otto-cli/src/commands.rs — cmd_auth and cmd_providers both call render_providers/login/logout -->

## Non-interactive / CI setup

`otto auth login` bails without a TTY, so CI has three options.

**1. Environment variables.** With no stored credential a provider falls back to
its env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …). Simplest option.

**2. `OTTO_AUTH_CONTENT`.** Set it to the JSON contents of `auth.json`; it
short-circuits the store as a read-only source, and nothing is written to disk.
<!-- src: crates/otto-auth/src/store.rs — AUTH_CONTENT_ENV = "OTTO_AUTH_CONTENT", read-only short-circuit ahead of the on-disk file -->

**3. Write `auth.json` directly** at `<data_dir>/otto/auth.json` with mode `0600`.
otto sets those permissions itself on every write; match them.
<!-- src: crates/otto-auth/src/store.rs — default path `dirs::data_dir()/otto/auth.json`, `Permissions::from_mode(0o600)` -->

```json
{
  "anthropic": { "type": "api", "key": "sk-ant-..." }
}
```

Resolution order is: an explicit in-memory override, then `OTTO_AUTH_CONTENT`,
then the on-disk file (a missing file yields an empty map).

Full variable list in [../reference/env.md](../reference/env.md).

## GitHub Copilot

Copilot needs no provider configuration. Log in, then name a model — the
models.dev registry already carries the context and output windows, so unlike
[local models](#local-models-ollama) no `limits` block is required.

```bash
otto auth login github-copilot     # device code: opens a URL, you paste a code
```

```json
{ "model": "github-copilot/claude-opus-4.8" }
```

Or per run: `otto --model github-copilot/gpt-5.4 run "..."`.

The provider id is `github-copilot`, with a hyphen. The underscore form
`github_copilot/...` is a *gateway* model name — see
[Model ids with slashes](#model-ids-with-slashes).

### Available models

`otto models github-copilot` prints the live list (25 entries at time of
writing). A sample:

| model | context | $/M in → out |
| --- | --- | --- |
| `claude-sonnet-5` | 1000k | 2 → 10 |
| `claude-opus-4.8` | 200k | 5 → 25 |
| `claude-haiku-4.5` | 200k | 1 → 5 |
| `gpt-5.4` | 400k | 2.5 → 15 |
| `gpt-5.4-nano` | 400k | 0.2 → 1.25 |
| `gemini-3.1-pro-preview` | 200k | 2 → 12 |

Also available: `claude-fable-5`, `claude-sonnet-4`/`4.5`/`4.6`,
`claude-opus-4.5`/`4.6`/`4.7`, `gpt-4.1`, `gpt-5-mini`, `gpt-5.2`, `gpt-5.5`,
several `*-codex` variants, `gemini-2.5-pro`, `gemini-3-flash-preview`,
`gemini-3.5-flash`, `kimi-k2.7-code`, `mai-code-1-flash-picker`.

### Three protocols behind one provider

Copilot fronts models from three vendors, so otto picks the wire protocol from
the model id. This is automatic and not configurable.

| model id | protocol | endpoint |
| --- | --- | --- |
| starts with `claude` | Anthropic Messages | `POST {base}/v1/messages` |
| `gpt-N` where N ≥ 5, **except** the `gpt-5-mini` family | OpenAI Responses | `POST {base}/responses` |
| everything else | OpenAI Chat | `POST {base}/chat/completions` |

<!-- src: crates/otto-llm/src/providers/copilot.rs — is_claude() is starts_with("claude"); route() branches on is_claude then should_use_responses; crates/otto-llm/src/protocols/openai_responses.rs, should_use_responses() -->

So `gpt-5-mini` deliberately takes the Chat route while `gpt-5.4` takes
Responses. All three are wrapped in `CopilotCache`, which applies the
`copilot_cache_control` markers in the shape the active protocol expects and
strips `max_tokens` for any `gpt*` model, because Copilot rejects it there.
<!-- src: crates/otto-llm/src/protocols/copilot_cache.rs — strip_max_tokens() gated on model_id.starts_with("gpt"); BodyShape::{OpenAi,Anthropic} -->

Requests carry the GitHub token as a `Bearer` header plus Copilot's required
static headers (`X-GitHub-Api-Version`); claude models additionally get an
`anthropic-beta: interleaved-thinking-2025-05-14` header.

### Enterprise

Pass your GitHub Enterprise domain at login:

```bash
otto auth login github-copilot --enterprise acme.ghe.com
```

The flag drives **both** halves of an enterprise deployment:

| | public | `--enterprise acme.ghe.com` |
| --- | --- | --- |
| device flow authenticates against | `github.com` | `acme.ghe.com` |
| Copilot API base | `api.githubcopilot.com` | `copilot-api.acme.ghe.com` |

A full URL works too — `https://acme.ghe.com` and `acme.ghe.com` normalize to
the same host.

Without the flag the credential records no domain and **every request goes to
the public host** — which an enterprise network typically cannot reach. The
failure surfaces as a connect error that the retry layer treats as transient,
so it retries rather than telling you the endpoint is wrong.
<!-- src: crates/otto-cli/src/cli.rs, the --enterprise arg on Login; crates/otto-auth/src/providers/copilot.rs, CopilotOAuth::enterprise() + normalize_domain(); crates/otto-llm/src/providers/copilot.rs, with_enterprise() -->

> [!NOTE]
> Copilot API access is gated by GitHub org policy, separately from having
> Copilot seats. If your organization has not enabled it, requests fail at
> connect no matter how otto is configured — ask an org admin before
> debugging further.

If the derived host is still wrong — a proxy-fronted Copilot, or a domain that
does not follow the `copilot-api.<domain>` convention — override it explicitly.
Config wins over the credential-derived host:

```json
{
  "provider": {
    "github-copilot": { "options": { "baseURL": "https://copilot.internal.acme" } }
  }
}
```
<!-- src: crates/otto-app/src/route_factory.rs, the "github-copilot" arm applies override_base after with_enterprise -->

> [!NOTE]
> Copilot OAuth has **no refresh flow**. Unlike Anthropic OAuth, the token is
> not renewed on load — when it lapses, re-run `otto auth login github-copilot`.
<!-- src: crates/otto-auth/src/providers/mod.rs — only the anthropic arm refreshes -->

## Local models (Ollama)

`ollama` is not a native arm, so it takes the OpenAI-compatible route. Ollama
serves an OpenAI-shaped API at `/v1`.

```json
{
  "model": "ollama/llama3.1",
  "provider": {
    "ollama": {
      "options": { "baseURL": "http://localhost:11434/v1" },
      "models": {
        "llama3.1": { "limits": { "context": 131072, "output": 8192 } }
      }
    }
  }
}
```

### Why `limits` is not optional in practice

otto reads context and output windows from the embedded models.dev registry.
That registry has no entry for local models, so `model.limits.context` resolves
to `None` — and the run loop's compaction check reads exactly that field. With no
context window, **compaction can never trigger**, the prompt grows without bound,
and the endpoint silently truncates it. The symptom is an agent that abruptly
forgets the start of a long session.

`provider.<id>.models.<model>.limits.{context,output}` is overlaid onto the
resolved model for **any** provider, native or not, which is the fix.
<!-- src: crates/otto-app/src/route_factory.rs — ModelLimitsOverride and the trailing overlay: `if let Some(ov) = self.providers.get(provider) && let Some(l) = ov.model_limits.get(model_id) { model.limits.context = l.context; ... }` -->

Set `context` to the value the server is actually configured for (Ollama's
`num_ctx`), not the model's theoretical maximum.

## Gateways (litellm)

Two shapes, depending on which wire protocol you want.

### (a) As an OpenAI-compatible provider id

```json
{
  "model": "litellm/claude-opus-4-8",
  "provider": {
    "litellm": {
      "options": {
        "baseURL": "http://localhost:4000/v1",
        "apiKey": "sk-litellm-..."
      },
      "models": {
        "claude-opus-4-8": { "limits": { "context": 200000, "output": 32000 } }
      }
    }
  }
}
```

Requests go to `http://localhost:4000/v1/chat/completions`. A stored credential
for the id wins over `options.apiKey`; the config key is the fallback.
<!-- src: crates/otto-app/src/route_factory.rs, the `other` arm — `key.or_else(|| ov.api_key.clone().map(Secret::literal))` -->

### (b) Native Anthropic protocol pointed at the gateway

The `anthropic` and `openai` arms honor `options.baseURL` and `options.apiKey`
while keeping their native wire protocol. Setting the Anthropic base URL to
litellm's Anthropic-compatible mount sends native Messages traffic through the
gateway.

```json
{
  "model": "anthropic/claude-opus-4-8",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "http://localhost:4000/v1",
        "apiKey": "sk-litellm-..."
      }
    }
  }
}
```

<!-- src: crates/otto-app/src/route_factory.rs — override_base/override_key computed from self.providers, applied via `p.with_base_url(base)` in the "anthropic" and "openai" arms -->

Prefer (b) when you want features the Anthropic Messages protocol carries and the
OpenAI Chat shape flattens or drops — extended thinking blocks, the native tool
result shape, and Anthropic-style cache control. Prefer (a) when the gateway is
fronting a mix of vendors behind one OpenAI-shaped API and you want one code path.

**`options.baseURL` is honored by the `anthropic`, `openai`, and
`github-copilot` arms.** The `google`/`gemini` and `vertex` arms still ignore
it — to route those through a gateway, use shape (a) with a custom provider id.
<!-- src: crates/otto-app/src/route_factory.rs — the "google" | "gemini" and "vertex" arms never read override_base -->

## Bedrock and Azure

**Both were removed as native providers in v0.12–v0.13.** There is no `bedrock`
or `azure` arm in the route factory, and no Bedrock SigV4 signing or Azure
deployment-URL handling anywhere in the tree.

Reach them through an OpenAI-compatible gateway (litellm, Bedrock Access Gateway,
or Azure's own OpenAI-compatible endpoint) configured as in the gateway section
above:

```json
{
  "model": "bedrock-gw/anthropic.claude-opus-4-8",
  "provider": {
    "bedrock-gw": {
      "options": { "baseURL": "http://localhost:4000/v1" },
      "models": {
        "anthropic.claude-opus-4-8": { "limits": { "context": 200000, "output": 32000 } }
      }
    }
  }
}
```

## Decoder tolerance

Protocol decoding is deliberately lenient, because gateways vary from the vendor
specs they proxy. Useful when debugging a flaky gateway:

- **Error `code` may be a string or a number.** litellm and OpenRouter commonly
  send the bare numeric HTTP status; strict decoding used to turn the whole frame
  into a fatal decode error.
  <!-- src: crates/otto-llm/src/protocols/openai_chat.rs — `enum ErrorCode { Num(i64), Str(String) }` with `#[serde(untagged)]` -->
- **`error` may be a bare string.** Both `{"error": {...}}` and
  `{"error": "boom"}` decode.
  <!-- src: crates/otto-llm/src/protocols/openai_chat.rs — `enum ErrorField { Struct(OpenAIChatError), Text(String) }`, untagged -->
- **Undecodable frames are skipped with a `tracing` warning**, not fatal. The
  stream fails only when garbage dominates: too many skipped frames, or a stream
  that decoded zero events. Look for `skipping undecodable stream frame` in the
  logs.
  <!-- src: crates/otto-llm/src/route.rs — skipped counter, MAX_SKIPPED_FRAMES, and the `skipped > 0 && decoded == 0` guard -->
- **Reasoning arrives under two names.** DeepSeek sends `reasoning_content`;
  OpenRouter and vLLM send `reasoning`. Both map to the same reasoning part.
  <!-- src: crates/otto-llm/src/protocols/openai_chat.rs — reasoning_content field plus the alias comment "mapped identically to reasoning_content" -->

A stream that yields zero recognized events surfaces a retryable `EmptyStream`
error rather than looping. See [../reference/env.md](../reference/env.md) for
`OTTO_LOG` filters (`OTTO_LOG=otto_llm=debug` shows decode-level detail).

## Introspection

```bash
otto models                 # every model in the installed registry
otto models openai          # filter to one provider
otto models --refresh       # force a fresh models.dev fetch first
otto providers list         # providers + credential status
```

`otto models` prints each model as `provider/model` with its capabilities
(`tools`, `reasoning`, `vision`), context window, and cost hint. The registry is
embedded in the binary and cached at `<cache_dir>/otto/models.json`.
<!-- src: crates/otto-cli/src/commands.rs, render_models(); cache path from crates/otto-app/src/runtime.rs and crates/otto-cli/src/lib.rs — global_cache_dir().join("models.json") -->

Models supplied only by `config.provider.<id>.models` do not appear in
`otto models` — that listing reflects the models.dev registry, not config.

> [!WARNING]
> `otto providers list` enumerates the **whole models.dev catalog** (~149 providers) with credential status — it is not a list of providers otto can route to. Entries such as `amazon-bedrock` and `azure` appear there despite having no native route in otto; logging into one does not make it reachable. The providers otto routes natively are `anthropic`, `openai`, `google`/`gemini`, `vertex`, and `github-copilot`; everything else needs an OpenAI-compatible `baseURL` as described above.
<!-- verified by running `otto providers list`: amazon-bedrock listed [not logged in] on v0.13.0, which has no Bedrock route -->

See [Bedrock and Azure](#bedrock-and-azure) for those two specifically.

## See also

- [../getting-started.md](../getting-started.md)
- [../reference/config.md](../reference/config.md)
- [../reference/env.md](../reference/env.md)
- [../reference/cli.md](../reference/cli.md)
