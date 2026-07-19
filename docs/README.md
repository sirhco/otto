# otto documentation

otto is a Rust agentic coding harness — a CLI, a terminal UI, and an opencode-compatible HTTP/SSE server over one shared runtime; see the [project README](../README.md).

## Start here

| If you are... | Read |
| --- | --- |
| New to otto | [getting-started.md](getting-started.md) |
| Using the terminal UI | [guide/tui.md](guide/tui.md) |
| Automating work with workflows | [guide/workflows.md](guide/workflows.md) |
| Wiring up a provider or a local model | [guide/providers.md](guide/providers.md) |
| Controlling what the agent may do | [guide/permissions.md](guide/permissions.md) |
| Building a client against otto | [reference/http-api.md](reference/http-api.md) |
| Hacking on otto itself | [dev/architecture.md](dev/architecture.md) |

## Guides

| File | Contents |
| --- | --- |
| [getting-started.md](getting-started.md) | Install, authenticate, first prompt. |
| [guide/cli.md](guide/cli.md) | Driving otto from the command line, including how to debug a failed turn. |
| [guide/tui.md](guide/tui.md) | The terminal UI: layout, keybindings, overlays, modes. |
| [guide/workflows.md](guide/workflows.md) | The sdd, tdd, and plan workflows and how to run them. |
| [guide/permissions.md](guide/permissions.md) | Permission modes, rule syntax, and what the agent is allowed to do. |
| [guide/providers.md](guide/providers.md) | Configuring Anthropic, OpenAI, Gemini, Vertex, Copilot, and any OpenAI-compatible gateway or local model. |

## Reference

| File | Contents |
| --- | --- |
| [reference/config.md](reference/config.md) | Every `otto.jsonc` field, with merge order. |
| [reference/env.md](reference/env.md) | Environment variables otto reads. |
| [reference/cli.md](reference/cli.md) | Every command, subcommand, and flag. |
| [reference/tools.md](reference/tools.md) | The built-in tools and their parameters. |
| [reference/agents.md](reference/agents.md) | Built-in agents, their prompts, step caps, and rulesets. |
| [reference/http-api.md](reference/http-api.md) | The `otto serve` route surface and SSE event shapes. |

## Development

| File | Contents |
| --- | --- |
| [dev/architecture.md](dev/architecture.md) | Crate layering, the turn pipeline, and the invariants that keep it correct. |
| [dev/contributing.md](dev/contributing.md) | Build and test commands, testing seams, release process, code conventions, known warts. |

`docs/superpowers/` holds internal planning and design artifacts (dated plans and specs), not user documentation.
