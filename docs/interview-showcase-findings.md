# What building an interview-prep stack on Riggate showed

`riggate-interview` (an MCP tool server) and `riggate-interview-web` (a browser client) were built
to exercise Riggate end to end. Each item is what was observed, with the evidence, and whether it
was fixed.

## Fixed

| Finding | Evidence | Fix |
|---|---|---|
| The `openai` provider speaks the **Responses API** only, so "OpenAI-compatible" servers that implement just `/chat/completions` (vLLM, LiteLLM and similar) could not be used | The upstream request body had `input`/`input_text`, not `messages` | `wire = "chat"` per agent; default unchanged |
| A valid reply in the wrong dialect became **`200` with an empty answer** | Chat-shaped body returned to a Responses agent: `choices[0].message.content == ""`, status 200 | A non-streaming reply with no text that was not cut off by a token limit is now `502` with a hint to check provider, wire and base URL |
| No Anthropic provider | `grep -i anthropic` over the repo was empty | `provider = "anthropic"` (`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`) |
| No way to test the tool path without a real model | Existing mock returned fixed text | `scripts/mock_provider.py` now speaks Responses, Chat and Messages, scripts tool calls, and takes a `MOCK_POLICY` file; `scripts/e2e_tools.sh` |

## Open

1. **Tool results are invisible to the client, so tool-held state needs a convention.** The
   interview session lives in the MCP server; a model can only find it on a later turn if it
   wrote the id into its own reply. The stack relies on `[session: ...]` marker lines enforced by
   preamble. It works, but it depends on the model complying and the client keeping the lines.
   Options: expose tool events in the stream, or let a request carry an opaque per-conversation
   id that Riggate forwards to MCP servers.
2. **No tool or progress events in the stream**, so a client cannot show "scoring your answer...".
   (`docs/DESIGN.md` already lists this.)
3. **Streamed `usage`** is only recorded on the tracing span; non-streamed `usage.prompt_tokens`
   can be `null`.
4. **Flaky unit test:** `agent::tests::an_absent_allowlist_registers_nothing_and_warns_with_the_server_to_fix`
   failed once in a full run and passed five times in isolation. It likely shares global tracing
   state with concurrent tests.
5. **History grows without bound:** the client resends everything each turn, and an interview of
   a few dozen turns approaches Riggate's 1 MiB default body limit only with very long answers.
   A truncation or summarisation option has not been needed yet.
6. **Real-model behaviour is unverified here.** Every test used a scripted stand-in for the
   model, so whether Claude follows the interviewer preamble reliably (marker lines, one question
   at a time, honest scores) still needs a run with a real key.

## Not a Riggate issue, but worth knowing

Playwright's headless Chromium needs system libraries and at least one font. On a minimal image
without them the browser started but behaved erratically (multi-line text input was truncated and
pages crashed intermittently) until fonts and fontconfig were provided.
