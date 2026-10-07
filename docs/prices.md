# Vendor prices

The vendors' published list prices, written as Portway `[prices]` tables to
copy into `portway.toml`. Checked against
[OpenAI's API pricing](https://developers.openai.com/api/docs/pricing) on
2026-10-02 and
[Anthropic's pricing](https://platform.claude.com/docs/en/about-claude/pricing)
on 2026-10-07. Prices change, so check those pages before relying on a
figure. Portway's costs are estimates, not a bill; behind a ChatGPT or Claude
subscription they are what the same tokens would cost on the API.

Rates are USD per million tokens. Each table is keyed by the model name the
client sends, and each tier by the value its request names; see
[prices](configuration.md#root-settings-and-prices) and
[service tiers](configuration.md#service-tiers).

## Which tier each client sends

| Client | Setting | Tier Portway records |
| --- | --- | --- |
| Codex 0.160 | standard speed | none |
| Codex 0.160 | `service_tier = "fast"` | `priority` |
| Codex 0.160 | `service_tier = "ultrafast"` | `ultrafast` |
| OpenAI API | `service_tier` | the value sent: `default`, `priority` or `fast`, `flex`, `ultrafast` |
| Claude Code 2.1.287 | standard | none |
| Claude Code 2.1.287 | fast mode (`/fast`) | `fast`, from `"speed": "fast"` |

`auto` is not priced here: it lets the vendor pick the class, so a request that
sends it stays unpriced.

## What these tables leave out

- **Cache writes.** Portway bills a cache write at the input rate. OpenAI lists
  cache writes at 1.25 times input; Anthropic at 1.25 times (5-minute cache) or
  twice (1-hour cache).
- **Regional pricing.** OpenAI's data residency endpoints and Anthropic's
  US-only inference (`inference_geo: "us"`) cost 10% more.
- **Unpublished models.** Codex's `codex-auto-review` is not on OpenAI's
  pricing page. Price it yourself or leave it unpriced.
- **Batch.** Batch API discounts are a separate API, not a request tier.

## OpenAI

Past 272K input tokens OpenAI bills the whole request at its long-context
rates, in every class: the `long_context` line on each table below, taken from
the long-context column of the same page. `gpt-5.3-codex` lists none.

`priority` and `fast` are one class under two names: Codex sends `priority`,
and the API accepts either since OpenAI renamed Priority processing to Fast
mode. `default` repeats the standard rates for clients that name it.
Ultrafast is published for `gpt-6-astra` only, and `gpt-5.6-sol`'s rates are
promotional "at least through November 21, 2026".

```toml
[prices."gpt-6-astra"]
input = 10.0
output = 50.0
cache_read = 1.0
long_context = { above = 272000, input = 20.0, output = 75.0, cache_read = 2.0 }

[prices."gpt-6-astra".tiers.default]
input = 10.0
output = 50.0
cache_read = 1.0
long_context = { above = 272000, input = 20.0, output = 75.0, cache_read = 2.0 }

[prices."gpt-6-astra".tiers.priority]
input = 20.0
output = 100.0
cache_read = 2.0
long_context = { above = 272000, input = 40.0, output = 150.0, cache_read = 4.0 }

[prices."gpt-6-astra".tiers.fast]
input = 20.0
output = 100.0
cache_read = 2.0
long_context = { above = 272000, input = 40.0, output = 150.0, cache_read = 4.0 }

[prices."gpt-6-astra".tiers.ultrafast]
input = 60.0
output = 300.0
cache_read = 6.0
long_context = { above = 272000, input = 120.0, output = 450.0, cache_read = 12.0 }

[prices."gpt-6-astra".tiers.flex]
input = 5.0
output = 25.0
cache_read = 0.5
long_context = { above = 272000, input = 10.0, output = 37.5, cache_read = 1.0 }

[prices."gpt-6.1-sol"]
input = 2.0
output = 10.0
cache_read = 0.1
long_context = { above = 272000, input = 4.0, output = 15.0, cache_read = 0.2 }

[prices."gpt-6.1-sol".tiers.default]
input = 2.0
output = 10.0
cache_read = 0.1
long_context = { above = 272000, input = 4.0, output = 15.0, cache_read = 0.2 }

[prices."gpt-6.1-sol".tiers.priority]
input = 4.0
output = 20.0
cache_read = 0.2
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.4 }

[prices."gpt-6.1-sol".tiers.fast]
input = 4.0
output = 20.0
cache_read = 0.2
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.4 }

[prices."gpt-6.1-sol".tiers.flex]
input = 1.0
output = 5.0
cache_read = 0.05
long_context = { above = 272000, input = 2.0, output = 7.5, cache_read = 0.1 }

[prices."gpt-6-sol"]
input = 2.0
output = 10.0
cache_read = 0.2
long_context = { above = 272000, input = 4.0, output = 15.0, cache_read = 0.4 }

[prices."gpt-6-sol".tiers.default]
input = 2.0
output = 10.0
cache_read = 0.2
long_context = { above = 272000, input = 4.0, output = 15.0, cache_read = 0.4 }

[prices."gpt-6-sol".tiers.priority]
input = 4.0
output = 20.0
cache_read = 0.4
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.8 }

[prices."gpt-6-sol".tiers.fast]
input = 4.0
output = 20.0
cache_read = 0.4
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.8 }

[prices."gpt-6-sol".tiers.flex]
input = 1.0
output = 5.0
cache_read = 0.1
long_context = { above = 272000, input = 2.0, output = 7.5, cache_read = 0.2 }

[prices."gpt-6-luna"]
input = 0.1
output = 0.5
cache_read = 0.01
long_context = { above = 272000, input = 0.2, output = 0.75, cache_read = 0.02 }

[prices."gpt-6-luna".tiers.default]
input = 0.1
output = 0.5
cache_read = 0.01
long_context = { above = 272000, input = 0.2, output = 0.75, cache_read = 0.02 }

[prices."gpt-6-luna".tiers.priority]
input = 0.2
output = 1.0
cache_read = 0.02
long_context = { above = 272000, input = 0.4, output = 1.5, cache_read = 0.04 }

[prices."gpt-6-luna".tiers.fast]
input = 0.2
output = 1.0
cache_read = 0.02
long_context = { above = 272000, input = 0.4, output = 1.5, cache_read = 0.04 }

[prices."gpt-6-luna".tiers.flex]
input = 0.05
output = 0.25
cache_read = 0.005
long_context = { above = 272000, input = 0.1, output = 0.375, cache_read = 0.01 }

[prices."gpt-5.6-sol"]
input = 4.0
output = 20.0
cache_read = 0.4
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.8 }

[prices."gpt-5.6-sol".tiers.default]
input = 4.0
output = 20.0
cache_read = 0.4
long_context = { above = 272000, input = 8.0, output = 30.0, cache_read = 0.8 }

[prices."gpt-5.6-sol".tiers.priority]
input = 8.0
output = 40.0
cache_read = 0.8
long_context = { above = 272000, input = 16.0, output = 60.0, cache_read = 1.6 }

[prices."gpt-5.6-sol".tiers.fast]
input = 8.0
output = 40.0
cache_read = 0.8
long_context = { above = 272000, input = 16.0, output = 60.0, cache_read = 1.6 }

[prices."gpt-5.6-sol".tiers.flex]
input = 2.0
output = 10.0
cache_read = 0.2
long_context = { above = 272000, input = 4.0, output = 15.0, cache_read = 0.4 }

[prices."gpt-5.3-codex"]
input = 1.75
output = 14.0
cache_read = 0.175

[prices."gpt-5.3-codex".tiers.default]
input = 1.75
output = 14.0
cache_read = 0.175

[prices."gpt-5.3-codex".tiers.priority]
input = 3.5
output = 28.0
cache_read = 0.35

[prices."gpt-5.3-codex".tiers.fast]
input = 3.5
output = 28.0
cache_read = 0.35```

## Anthropic

Anthropic draws no long-context line for most of these models: "Claude 4.6 and
later models (except Claude Haiku 5.5) … include the full 1M token context
window at standard pricing", and fast mode's rates apply across the whole
window too. Claude Haiku 5.5 is the exception: a prompt of over 100,000 tokens
pays five times the rates on every line, so its table carries
`above = 100000`. Fast mode is a research preview for Opus models. Its cache
reads use the model's cache multiplier on the fast input rate (0.05 times on
Opus 5.5, 0.1 times on Opus 5). The other models on Anthropic's page take the
same shape.

Anthropic's page disagrees with itself on Sonnet 5.5's cache reads: the model
table lists $0.20, while the caching section gives 0.05 times input ($0.10).
The table below keeps the model table's $0.20.

```toml
[prices."claude-fable-5-1"]
input = 10.0
output = 50.0
cache_read = 0.25

[prices."claude-opus-5-5"]
input = 4.0
output = 20.0
cache_read = 0.2

[prices."claude-opus-5-5".tiers.fast]
input = 8.0
output = 40.0
cache_read = 0.4

[prices."claude-opus-5"]
input = 5.0
output = 25.0
cache_read = 0.5

[prices."claude-opus-5".tiers.fast]
input = 10.0
output = 50.0
cache_read = 1.0

[prices."claude-sonnet-5-5"]
input = 2.0
output = 10.0
cache_read = 0.2

[prices."claude-haiku-5-5"]
input = 0.1
output = 0.5
cache_read = 0.01
long_context = { above = 100000, input = 0.5, output = 2.5, cache_read = 0.05 }

[prices."claude-haiku-4-5-20251001"]
input = 1.0
output = 5.0
cache_read = 0.1```
