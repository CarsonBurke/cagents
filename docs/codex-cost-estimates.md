# Native Codex cost estimates

Codex reports token counts rather than a monetary cost. The native chat footer displays an API-equivalent text token estimate for supported models. This is not a ChatGPT subscription charge or an invoice. Provider-reported costs take precedence.

The estimator records cumulative token increments using the model and service tier confirmed for each turn. Cached reads and cache writes are portions of input, not additional tokens; reasoning output is already included in output. Repeated and late cumulative notifications cannot charge an earlier request twice. Long-context pricing uses the request's input count, not the conversation's cumulative input.

When a resumed thread, missed notification, unknown rate, or revised cache classification prevents complete accounting, the footer marks the estimate as partial. It does not apply the current model's rate to unseen historical requests. A cache-only correction retains the approximate amount and marks it partial instead of silently claiming complete accounting. Tool fees and regional premiums are excluded.

## Pricing sources

Rates were checked on **2026-10-12** against these primary sources:

- [OpenAI API pricing](https://developers.openai.com/api/docs/pricing): standard and faster service tiers, current GPT-6 prices, and GPT-5.3 Codex prices. Fast mode is the current name for Priority processing.
- [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching): input consists of ordinary, cached-read, and cache-write categories; GPT-5.6 and later cache writes cost 1.25 times ordinary input.
- [GPT-6 Sol](https://developers.openai.com/api/docs/models/gpt-6-sol), [GPT-6.1 Sol](https://developers.openai.com/api/docs/models/gpt-6.1-sol), and [GPT-6 Luna](https://developers.openai.com/api/docs/models/gpt-6-luna): model rates and tariffs above 272,000 request input tokens.
- [GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol), [Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra), and [Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna): previous-generation rates, cache writes, and long-context tariffs. The `gpt-5.6` alias maps to Sol.
- [GPT-5.5](https://developers.openai.com/api/docs/models/gpt-5.5), [GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4), [GPT-5.4 Mini](https://developers.openai.com/api/docs/models/gpt-5.4-mini), and [GPT-5.4 Nano](https://developers.openai.com/api/docs/models/gpt-5.4-nano): older model rates and applicable long-context rules.

The rate table is maintained in `src/chat/pricing.rs`; new model names and unknown service tiers do not receive guessed fallback prices. Refresh both the table and verification date when prices change. In particular, the documented GPT-5.6 Sol promotional price is guaranteed only at least through November 21, 2026.
