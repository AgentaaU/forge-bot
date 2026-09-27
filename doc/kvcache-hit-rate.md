# Prompt / KV cache hit rate of forge-bot's agents

Research for issue #43: *"check KVCache hit rate of codex/pi session invoked by
forge-bot"*.

## TL;DR

Both backends keep the provider prompt (KV) cache very warm:

| Agent | Sessions | API calls / turns | Cache hit rate |
| --- | ---: | ---: | ---: |
| Codex | 16 threads | 935 calls | **96.2 %** |
| Pi | 2 sessions | 49 turns | **94.4 %** |

The per-thread session reuse added in #31 is doing its job. A brand-new Codex
thread already starts around 82 % because the shared system/developer prompt
prefix is cached provider-side; within a run, later calls average 94.8 %. Pi's
first turn is cold (12–29 %), then every later turn reads 90 %+ from cache.

The numbers are a snapshot taken partway through the run for this issue, so the
Pi totals are still growing. Re-run `contrib/analyze-kvcache.py` to reproduce
them.

Two important qualifications, both raised in review:

* The cache is **per agent and per provider**. The session store is keyed by
  `(agent, conversation)`, so falling back from Codex to Pi starts a brand-new
  Pi session — the Codex conversation and its KV cache are *not* reused.
* `pi` and `pi-rpc` run the same `pi` CLI against the same provider/model, so
  they share one quota. `pi-rpc` is not a capacity fallback for `pi`; the two
  are instead being consolidated on `pi-rpc` (see below).

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
* The pooled `pi-rpc` adapter keeps `pi --mode rpc` processes alive and pins
  each conversation to one process (`src/agent/pi_rpc.rs`).

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

### Codex

* 16 threads, 935 API calls.
* **50,150,144 cached / 52,128,614 prompt tokens = 96.2 %**.
* Per-thread hit rates range from 87.1 % to 98.4 %.

Breakdown by call phase:

| Phase | Meaning | Avg. hit rate | Samples |
| --- | --- | ---: | ---: |
| cold | first call of a brand-new thread | 82.1 % | 16 |
| resume | first call of a new comment in an existing thread | 32.3 % | 11 |
| later | any subsequent call within one run | 94.8 % | 908 |

Two things stand out:

1. **A cold thread is not fully cold.** The first call consistently reports
   ~11,776 cached of ~14,000 prompt tokens (~84 %). That prefix is the shared
   Codex system/developer prompt, which the provider already has cached from
   other sessions. Only the thread-specific part misses.
2. **Resuming is cheap but not free.** The first call of a resumed run averages
   only 32 % and in several runs reports 0 %. Re-rendering the conversation and
   appending the new user turn shifts the prefix boundary, so the first call
   often misses what the previous run cached. It recovers immediately: the run
   as a whole still lands at 80–97 %, and *later* calls are the highest of all
   at 94.8 %. Net effect over a multi-comment thread is still strongly in
   favour of resuming.

### Pi

* 2 sessions, 49 turns.
* **1,528,064 cached / 1,618,447 prompt tokens = 94.4 %**.
* First turn is cold (12 % and 29 %), average turn is 84–92 %, and the
  session-level rate is 93–96 %.
* `cacheWrite` is always 0: DeepSeek reports prompt-cache hits and misses
  directly rather than a separate write charge.

Pi is only exercised as the first fallback when Codex is capacity-limited,
which is why there are so few Pi sessions compared with Codex. The next section
shows what that fallback costs.

### Cross-agent fallback (Codex → Pi)

Because the session store is keyed by `(agent, conversation)`, a fallback is a
full cold start for the model: a different provider/model cannot read the
previous agent's KV cache, and the new agent does not receive the previous
agent's conversation either. The clearest example in the data is conversation
`forgejo:shylock/stock-analysis:452`:

| Phase | Agent | Session | Workspace | First-call hit | Run/session hit |
| --- | --- | --- | --- | ---: | ---: |
| 1 | Codex | `01a0d70a-…` | `…stock-analysis-452` | 43 % | 96.7 % (52 calls) |
| 2 | Pi | `95052b21-…` | `…stock-analysis-453` | 12 % | 96.2 % (25 turns) |

Codex handled the thread first (workspace `452`). When it hit its capacity
limit at 05:57:58Z the bot switched to Pi for the same conversation — the
webhook was a PR whose workspace is `453`, but the conversation key folds it
onto issue `452`. Pi then started a **new** session (`95052b21-…`, model
`deepseek/deepseek-flash`). Its 12 % first-turn hit is the DeepSeek shared
test-prefix baseline, not reuse of Codex's cache: the 52 Codex calls' worth of
context was never visible to Pi.

So each fallback pays a fresh conversation *and* a fresh cache. The agent can
still re-read the forge thread to reconstruct some context, but the model's
own history is gone. Switching back later (e.g. Codex recovers) resumes the
old Codex session, which then does not contain what Pi did — the conversation
forks per agent.

### Do we need both `pi` and `pi-rpc`?

Short answer: **no — they are the same backend twice.** `pi` (67 lines) is an
ordinary `CommandAgent`: it spawns `pi --print --session-id <id>` per comment
and remembers the id in `agent-sessions.json`, so the next comment resumes the
same on-disk session. `pi-rpc` (585 non-test lines, 983 with tests) is a
bespoke pool that keeps `pi --mode rpc` processes alive, pins a conversation to
one process, and evicts it after `idle_ttl_secs` (default 900 s).

| Dimension | `pi` (one-shot) | `pi-rpc` (pool) |
| --- | --- | --- |
| Provider / quota | `deepseek/deepseek-flash` | same, shared |
| Conversation state | session file on disk (`--session-id`) | in-memory, bound to a live process |
| Survives restart / pool eviction | yes | **no** (`no_session = true`) |
| Spawns a process per comment | yes | no, while the process stays warm |
| Resumed first-call cache miss | yes | no, if the process was still warm |
| Concurrency | always able to spawn | bounded by `max_agents`; waits when full |
| Streaming | no | yes (unused by the gateway) |
| Code | 67 lines, generic to every CLI | 983 lines, Pi-specific |
| Automatic fallback | yes (2nd, after Codex) | no — explicit `@agent:pi-rpc` only |

Because Codex is the first choice and `pi` comes before `pi-rpc` in
`BUILTIN_AGENTS`, the automatic fallback reaches `pi-rpc` only after both Codex
*and* `pi` have failed. In practice that is when the shared Pi provider is
already rate-limited, so the extra attempt mostly just fails too. Otherwise
`pi-rpc` is used only when a user asks for it by name. So the current shape is
“a simple, persistent fallback plus an opt-in warm-process mode”. That is
defensible, but it means maintaining two Pi adapters for the same provider and
nearly identical prompts, and the opt-in one is the one that *loses*
conversation state on restart/eviction.

The pool was the original Pi backend (`404848e`) and is still being actively
improved (`204e739`, the HEAD of `main`, reuses idle agents), so this is a
deliberate design choice rather than an accident. The question is whether the
warm-process benefit justifies the second adapter:

* The one-shot `pi` already reaches **94 %** session-level cache hit, and its
  only cache penalty is the first call of each comment. The pool removes that
  penalty for comments spaced closer than `idle_ttl_secs`, which is a real but
  modest win.
* The pool does not provide capacity relief (same provider), and it is not in
  the automatic path anyway, so it is effectively dormant in normal operation.

**Decision (implemented in a follow-up PR):** keep `pi-rpc` as the default Pi backend and
give it a deterministic `--session-id` so it persists across evictions; keep
the one-shot `pi` adapter in the tree but disabled by default (`[agents.pi]
enabled = true` re-enables it). That is the “unify on `pi-rpc`” direction
above: one active Pi backend, one persistent session per conversation, with the
older adapter available but off the default path. The reasoning that made this
the right choice over dropping `pi-rpc` is that the pool is where the recent
work went, it avoids a per-comment process start, and `--session-id` removes
its only real regression (losing the conversation on eviction).

## Caveats

* Cache hit rate is provider-reported and provider-specific. Codex counts
  `cached_input_tokens` as a subset of `input_tokens`; Pi/DeepSeek reports hit
  and miss separately.
* The report is a point-in-time snapshot. The session for issue #43 is still
  running while this was written.
* No cost figure is included: the cached-input discount differs per provider
  and per plan, and the Pi `cost` field is only populated for some providers.
* A high hit rate does not by itself prove the run was cheap — output tokens and
  reasoning tokens are billed separately and are not part of this metric.

## Recommendations

1. **Keep per-thread session reuse.** It is the mechanism that produces the
   96 %/94 % figures; nothing else in the gateway affects caching.
2. **Stop treating `pi-rpc` as a capacity fallback for `pi`.** Both adapters
   drive the same `pi` CLI and both use Pi's own configured default, so here
   both resolve to `deepseek/deepseek-flash`. A quota or rate limit that stops
   `pi` therefore stops `pi-rpc` too. `AgentRegistry::names()` currently returns
   `codex, pi, pi-rpc, claude, kimi`, so a Codex capacity hit tries `pi` and
   then `pi-rpc` — two attempts for one provider, doubling the failure latency.
   The longer-term consolidation is to keep only one Pi adapter at all (see
   “Do we need both `pi` and `pi-rpc`?” above).
3. **Accept that fallback breaks cache and conversation continuity.** There is
   no cross-agent cache to preserve, and the per-agent session store makes the
   conversation fork when a run bounces between agents. If continuity across
   fallbacks matters, the gateway would have to carry a summary or the agents a
   shared transcript; that is a larger design change, not a cache-tuning one.
4. **Optionally surface the metric.** The gateway could parse
   `cached_input_tokens` / `cacheRead` from the CLI output and log a per-job hit
   rate, making regressions visible without opening session files.
5. **No prompt-layout change is warranted yet.** The resumed first-call dip is
   caused by Codex/Pi's own conversation rendering, not by the gateway prompt,
   and it recovers within the same run.

## Reproduce

```bash
python3 contrib/analyze-kvcache.py
```

Environment overrides: `FORGE_BOT_WORKSPACE_ROOT` (default
`~/.local/state/forge-bot/workspaces`). The Codex and Pi transcript locations
follow the CLIs' defaults under `$HOME`.
