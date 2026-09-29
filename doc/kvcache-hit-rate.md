# Prompt / KV cache hit rate of forge-bot's agents

Research for issue #43: *"check KVCache hit rate of codex/pi session invoked by
forge-bot"*. Re-measured for issue #126 (same operation, larger sample).

## TL;DR

Both backends keep the provider prompt (KV) cache very warm:

| Agent | Sessions | API calls / turns | Cache hit rate |
| --- | ---: | ---: | ---: |
| Codex | 87 threads | 4,941 calls | **97.1 %** |
| Pi | 72 sessions | 5,171 turns | **97.0 %** |

The per-thread session reuse added in #31 is doing its job. A brand-new Codex
thread starts around 74 % on average because the shared system/developer prompt
prefix is cached provider-side, but the spread is wide (0–96 %); within a run,
later calls average 95.8 %. Pi's first turn is usually fully cold (0–33 %,
mean ~5 %) because the provider does not share a cached prefix across sessions,
then every later turn reads ~92 % from cache and the session as a whole lands
at ~96 %.

This is a snapshot for #126 taken on 2026-09-29. The earlier #43 figures, taken
2026-09-25 with 16 Codex threads and 2 Pi sessions, were 96.2 % and 94.4 %.
Re-run `contrib/analyze-kvcache.py` to reproduce the current numbers.

Two important qualifications, both raised in review:

* The cache is **per agent and per provider**. The session store is keyed by
  `(agent, conversation)`, so falling back from Codex to Pi starts a brand-new
  Pi session — the Codex conversation and its KV cache are *not* reused.
* All Pi sessions in this sample ran the same CLI against the same
  provider/model (`opencode-go/deepseek-v4.1-flash`), so every Pi adapter
  shares one quota. Since #46 the default Pi backend is the pooled `pi-rpc`
  adapter and the one-shot `pi` adapter is disabled by default (see below).

## How the cache is meant to be used

The gateway itself never builds model context. It hands an agent a *location*
and a *message*, and remembers one backend session id per
`(agent, conversation)` in `agent-sessions.json` (`src/agent/session.rs`).
The next comment in the same issue or pull request resumes that backend
session:

* `codex exec` reports a `thread.started` id; a later comment runs
  `codex exec resume <id>` (`src/agent/codex.rs`).
* `pi --print` is handed a deterministic `--session-id`, which Pi creates on
  first use and resumes afterwards (`src/agent/pi.rs`).
* The pooled `pi-rpc` adapter keeps `pi --mode rpc` processes alive, pins each
  conversation to one process, and persists the conversation with a
  deterministic `--session-id` so an evicted or restarted process can resume it
  (`src/agent/pi_rpc.rs`).
* A pull request is folded onto the issue it closes and each thread is pinned
  to one agent, so comments on either side share one session instead of
  scattering across agents (`src/forge/mod.rs`, `src/agent/pi_rpc.rs`).
* A same-thread follow-up that arrives while a `pi-rpc` run is in flight is
  steered into the live session rather than starting a second cold run
  (`src/agent/prompt.rs`, `src/agent/pi_rpc.rs`).

When the backend keeps the same conversation, the provider can serve the
unchanged prefix (system prompt, repository instructions, prior turns) from its
prompt cache instead of re-billing it as fresh input.

## Method

`contrib/analyze-kvcache.py` reads the token accounting both CLIs already write:

* **Codex** – `~/.codex/sessions/**/rollout-*.jsonl`. Every
  `token_usage_record` has `usage.input_tokens` (whole prompt) and
  `usage.cached_input_tokens` (the part served from cache). Hit rate is
  `cached_input_tokens / input_tokens`.
* **Pi** – `~/.pi/agent/sessions/*/*.jsonl`. Every assistant message has a
  `usage` block where `input` is the cache-miss prompt and `cacheRead` is the
  cache-hit prompt. Hit rate is `cacheRead / (input + cacheRead)`.

Only sessions whose recorded `cwd` is under
`~/.local/state/forge-bot/workspaces` are counted, so unrelated interactive
Codex/Pi use is excluded. Codex runs are grouped by thread id: the first run of
a thread is *cold*, later runs in the same thread are *resumes* (i.e. a new
forge comment), and calls after the first inside one run are *later*.

## Findings

### Compared with #43

The re-measurement raises the headline rate, but the rise is mostly the
larger, longer-lived sample rather than a change in caching behaviour:

| Metric | #43 (2026-09-25) | #126 (2026-09-29) | Δ |
| --- | ---: | ---: | ---: |
| Codex, all calls (token-weighted) | 96.2 % | 97.1 % | +0.9 pp |
| Codex, cold first call | 82.1 % | 74.0 % | −8.1 pp |
| Codex, resumed first call | 32.3 % | 27.3 % | −5.0 pp |
| Codex, later calls | 94.8 % | 95.8 % | +1.0 pp |
| Pi, whole session | 94.4 % | 97.0 % | +2.6 pp |

The all-call figure is token-weighted while the phase figures are per-call
averages, and later calls grow with each turn while a cold/resume first call is
a single small prompt. Later calls are 99.5 % of the current prompt tokens
(99.2 % in #43), so the headline is essentially the warm-call rate. Warm calls
did rise (94.8 % → 95.8 % per call, 96.5 % → 97.2 % token-weighted), but the
first calls did not: both cold and resume are *lower* than in the smaller #43
sample, and 5 of the 87 cold starts now report 0 %. That is the expected effect
of more distinct threads competing for the shared Codex system/developer
prefix, not a gateway regression.

The Pi delta is not a like-for-like comparison. The #43 sessions ran
`deepseek/deepseek-flash`; every #126 session runs
`opencode-go/deepseek-v4.1-flash`. The pre-switch sessions kept under
`~/.pi/backups/sessions-deepseek-*` show that the old provider reaches 99.2 %
overall with a ~26 % mean first turn, while the current provider reaches 97.0 %
with a ~5 % mean first turn. The #43 figure simply captured two young sessions
(49 turns) while their long tails were still unrecorded; the #126 figure is the
completed picture, and the provider change *lowered* the first-turn hit rate.

Net: per-thread reuse keeps the cache very warm and the mechanism is stable,
but the data show no per-request improvement over #43.

### Codex

* 87 threads, 4,941 API calls.
* **316,781,952 cached / 326,256,982 prompt tokens = 97.1 %**.
* Per-thread hit rates range from 86.7 % to 98.7 % (mean 95.2 %).

Breakdown by call phase:

| Phase | Meaning | Avg. hit rate | Samples |
| --- | --- | ---: | ---: |
| cold | first call of a brand-new thread | 74.0 % | 87 |
| resume | first call of a new comment in an existing thread | 27.3 % | 13 |
| later | any subsequent call within one run | 95.8 % | 4,841 |

Two things stand out:

1. **A cold thread is not fully cold, but the spread is wide.** The first call
   averages ~10.8k cached of ~14.8k prompt tokens (~73 %). That prefix is the
   shared Codex system/developer prompt, which the provider already has cached
   from other sessions; only the thread-specific part misses. The distribution
   is broad, though: 5 of 87 cold calls report 0 %, while 58 report 75 % or
   more.
2. **Resuming is cheap but not free — and the current sample is contaminated.**
   The first call of a resumed run averages only 27 %, but 11 of the 13 resumed
   runs used a *different model* than the run before them (`gpt-6-sol` →
   `gpt-6-astra` → `gpt-6-luna`), and a provider cache is per model, so those
   misses are expected rather than a rendering artefact. The two same-model
   resumes were mid-turn restarts (the same `turn_id` continued after an
   interruption), not a new comment, and both report 0 %. There is no clean
   sample of a *new comment* resuming on the same model — the case that matters
   now that the agent layer no longer selects a model (`9f6cb7a`). It recovers
   within the run, and *later* calls average 95.8 %. It does not follow that
   resuming raises the cache hit rate: on the eight threads with more than one
   run, the token-weighted rate is 96.7 % for the cold first run but 95.3 % for
   the resumed runs, so the value of resuming is the model context it
   preserves, not a larger cached prefix.

### Pi

* 72 sessions, 5,171 turns, all on `opencode-go/deepseek-v4.1-flash`.
* **492,855,156 cached / 508,141,498 prompt tokens = 97.0 %**.
* Session-level rates range from 86.3 % to 99.1 % (mean 96.1 %).
* The first turn is cold in most sessions: 43 of 72 report 0 %, the maximum is
  33 %, and the mean is ~5 %. After that the average turn is ~92 %.
* `cacheWrite` is always 0: the provider reports prompt-cache hits and misses
  directly rather than a separate write charge.

Pi handles the automatic fallback when Codex is capacity-limited, which is why
there are fewer Pi sessions than Codex threads. The next section shows what
that fallback costs.

### Cross-agent fallback (Codex → Pi → Codex)

Because the session store is keyed by `(agent, conversation)`, a fallback is a
full cold start for the model: a different provider/model cannot read the
previous agent's KV cache, and the new agent does not receive the previous
agent's conversation either. The clearest example in the current data is
conversation `forgejo:shylock/stock-analysis:478`:

| Phase | Agent | Session | First-call hit | Run/session hit |
| --- | --- | --- | ---: | ---: |
| 1 | Codex | `01a0eac1-…` | 0 % | 98.7 % (148 calls) |
| 2 | Pi | `7afed147-…` | 0 % | 92.7 % (27 turns) |
| 3 | Codex | `01a0ead9-…` | 0 % | 97.9 % (90 calls) |

Codex handled the thread first. When it hit its capacity limit the bot switched
to Pi for the same conversation; Pi started a **new** session (`7afed147-…`,
model `opencode-go/deepseek-v4.1-flash`). Its 0 % first-turn hit is a genuine
cold start, not reuse of Codex's cache: the 148 Codex calls' worth of context
was never visible to Pi. When Codex recovered and took the next comment, it
resumed its *old* thread (`01a0ead9-…`), which does not contain what Pi did.

So each fallback pays a fresh conversation *and* a fresh cache. The agent can
still re-read the forge thread to reconstruct some context, but the model's own
history is gone, and the conversation forks per agent.

### One Pi backend now

At the time of the #43 research there were two Pi adapters (`pi` and `pi-rpc`)
driving the same CLI and provider, and the fallback order tried both. This
measurement confirms there is nothing to gain from keeping both in the
automatic path: they share a quota, and `pi-rpc` now persists its session with
a deterministic `--session-id`, so it no longer loses the conversation on
eviction or restart. #46 therefore made `pi-rpc` the default Pi backend and
disabled the one-shot `pi` adapter by default (`[agents.pi] enabled = true`
re-enables it). The duplicate fallback attempt is gone, so the current agent
sequence is `codex, agy, pi-rpc, claude`: one Pi backend, one persistent
session per conversation.

## Caveats

* Cache hit rate is provider-reported and provider-specific. Codex counts
  `cached_input_tokens` as a subset of `input_tokens`; the Pi provider reports
  hit and miss separately.
* The report is a point-in-time snapshot (2026-09-29). The session for the
  issue that requested this run is still being appended while the numbers are
  read.
* No cost figure is included: the cached-input discount differs per provider
  and per plan, and the Pi `cost` field is only populated for some providers.
* A high hit rate does not by itself prove the run was cheap — output tokens and
  reasoning tokens are billed separately and are not part of this metric.

## Recommendations

1. **Keep per-thread session reuse.** It preserves the model context across
   comments and lets each resumed run recover to a high hit rate, but it does
   not by itself raise the cache hit rate. The 97 % headline comes from long
   runs, where *later* calls are 99.5 % of the tokens; the other gateway
   changes described above (pinning/folding, `pi-rpc` persistence, follow-up
   steering) are what keep those calls warm.
2. **Keep one Pi backend.** `pi` and `pi-rpc` are the same CLI and provider, so
   a quota or rate limit that stops one stops the other. Since #46 only
   `pi-rpc` is enabled by default, which avoids doubling the fallback latency
   for a single provider.
3. **Accept that fallback breaks cache and conversation continuity.** There is
   no cross-agent cache to preserve, and the per-agent session store makes the
   conversation fork when a run bounces between agents. If continuity across
   fallbacks matters, the gateway would have to carry a summary or the agents a
   shared transcript; that is a larger design change, not a cache-tuning one.
4. **Surface the metric (implemented).** The gateway parses the provider
   accounting from the CLI output, logs a per-job hit rate, stores it on the
   run record, and shows it on the status page. Coverage follows what each CLI
   reports: Codex (`turn.completed.usage`), `pi-rpc` (`message_end` usage),
   Antigravity (`--output-format json`, `cache_read_tokens`), and Claude Code
   (`--output-format json`, `cache_read_input_tokens`). The one-shot `pi` and
   `kimi` adapters print plain text; set `output_format = "json"` plus the
   CLI's own flag to opt a JSON-capable CLI in. This is the way to confirm
   whether a same-model resume stays warm now that model selection is out of
   the agent layer.
5. **No prompt-layout change is warranted yet.** Once the model-change
   confound is removed, the remaining resumed first-call dip is Codex/Pi's own
   conversation rendering, not the gateway prompt, and it recovers within the
   same run.

## Reproduce

```bash
python3 contrib/analyze-kvcache.py
```

Environment overrides: `FORGE_BOT_WORKSPACE_ROOT` (default
`~/.local/state/forge-bot/workspaces`). The Codex and Pi transcript locations
follow the CLIs' defaults under `$HOME`.
