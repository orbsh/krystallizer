# Idea: an extractor would be a second model, not a replacement

**Status**: idea, recorded 2026-10-31, narrowed the same day when the model selection
moved to the compiler. Not a decision — no ADR, no PLAN item on its own. Third file in
the ingestion set: [hypergraph-population.md](hypergraph-population.md) is why local
extraction at all; [extraction-group.md](extraction-group.md) is keeping a group's
context; this one is what an extractor would *cost this service*.

The shortlist is not here. Backbone candidates, the three task shapes (sentence embedder /
fixed span head / zero-shot tagger), their data volumes and their quantization story
belong to the compiler that would produce the asset: `~/world/silex/encoder-route.md` §1
and §1b. A model table duplicated in two repositories guarantees the two eventually
disagree, and the compiler is where the choice is actually made.

## 1. What exists today

Extraction is the agent's own model writing facts through a memory tool mid-conversation
(`docs/PROMPTS.md`, ADR-0008 "记忆添加"). No local model, no extra infrastructure, and the
tokens are already being generated. A local extractor is therefore not an acceleration of
that path — it is a different cost profile: near-zero marginal cost per token, but it
needs a labelled set, a fine-tuning loop, an owner, and it writes confident nonsense
whenever an entity boundary is wrong.

The service already runs one encoder: candle BERT, `BAAI/bge-small-zh-v1.5`, 512-dim,
in-process (PLAN 1b). That model produces *sentence embeddings* for retrieval. It is not
an extractor, it does not become one by removing its pooling layer, and it cannot simply be
swapped for a better model: every stored `Vector<f32>` belongs to the space that produced
it, which is why PLAN 1b says the model is locked with the data. A second model therefore
enters either as its own face with its own meaning (extraction runs on it, retrieval does
not), or not at all.

## 2. The prerequisite, and it is not a benchmark

Nothing here is worth starting before someone hand-labels ~200 turns of this project's own
conversations — Chinese prose with English identifiers. That is the actual input shape and
the one no public benchmark measures. Without it, "it works" and "it emits plausible spans"
are indistinguishable; zero-shot taggers especially, since their failure is silent and
there is nothing to score them against.

The metric is boundary F1 and relation precision on that set, for the cheapest available
baseline — not a comparison of models on English GLUE-style data.

## 3. What belongs to this service

Two claims from the outside summaries are worth keeping, both because they are about layers
rather than speeds:

- **The extractor runs here.** Not in okm: okm computes nothing, the supply line ends at
  this process (`mem-server` or a worker beside it).
- **A context limit is not a cost.** An 8K model on a 3K input runs 3K tokens; the
  difference lives in memory and latency curves and has to be measured on the box.

Everything else — which backbone, zero-shot or fixed head, INT8 or not — is the compiler's
call, made against a labelled set that does not exist yet.

## 4. What would make this file irrelevant

If the model that already reads the conversation keeps writing the facts, there is no
extractor to train. That is today's state, and nothing here changes it.