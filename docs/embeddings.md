# Embeddings for LAN services (EmbeddingGemma-2)

koblem serves [EmbeddingGemma-2](https://huggingface.co/google/embeddinggemma-2) text and image
embeddings to other services on your network. Text and images share one 768-d space, so a text query can be
scored against image vectors as well as against other text.

| | |
|---|---|
| Base URL | `http://<koblem-host>:8090` (koblem) |
| Auth | none on `/v1/*`; `/api/v1/*` are the same routes behind `Authorization: Bearer <KOBLEM_API_KEY>` |
| Model id | `google/embeddinggemma-2` (store it next to every vector) |
| Vectors | 768 floats, L2-normalised; compare with cosine (= dot product) |
| Text limit | 1024 tokens per input, prefix included (≈ 4,500 characters of English) |

## Text: `POST /v1/text_embeddings`

The common case. koblem adds the model's task prefix for you; send plain text.

```bash
curl -s http://<koblem-host>:8090/v1/text_embeddings -H 'content-type: application/json' -d '{
  "input":  ["Invoice #4411 from Acme for March hosting, due 30 April",
             "Your flight to Melbourne is confirmed for Friday"],
  "task":   "document",
  "titles": ["Acme invoice", "Qantas booking"]
}'
```

```json
{"embeddings": [[0.0123, -0.0456, ...], [...]], "model": "google/embeddinggemma-2", "dim": 768, "duration_ms": 9}
```

The text is served by `embed-text`, an always-on instance (~0.5 GB of VRAM). It never waits on
the GPU gate. One request costs a few milliseconds (about 4 ms for a short query, 2 ms per
summary-sized document in a batch), so batching helps but isn't required.

| Field | Meaning |
|---|---|
| `input` | list of strings |
| `task` | which prefix to apply (table below); omitted = embed the text exactly as sent |
| `titles` | `document` only: one title per input (`null` or a missing entry → untitled) |
| `chunk` | `{"max_tokens": 512, "overlap_tokens": 64}`: split long inputs instead of rejecting them (see below) |

### Pick the right `task`

**Queries and the documents they search must use the matching pair.** Mixing conventions (e.g.
embedding stored emails with `query`) quietly degrades retrieval; nothing errors.

| `task` | Prefix koblem writes | Use for |
|---|---|---|
| `document` | `title: {title or none} \| text: …` | anything you store and search over: emails, summaries, notes |
| `query` | `task: search result \| query: …` | search box input, matched against `document` vectors |
| `question_answering` | `task: question answering \| query: …` | a question whose answer is in `document`s |
| `fact_checking` | `task: fact checking \| query: …` | a claim to find evidence for |
| `classification` | `task: classification \| query: …` | inputs to a classifier you train on the vectors |
| `clustering` | `task: clustering \| query: …` | grouping texts with each other |
| `similarity` | `task: sentence similarity \| query: …` | symmetric "are these two the same" comparisons |
| `code_retrieval` | `task: code retrieval \| query: …` | natural-language query against code `document`s |
| *(omitted)* | none | you are writing the prefix yourself |

A worked check (the example above plus a lunch note, then the query "when do I need to pay
the hosting bill"): the invoice scores 0.73, the flight 0.58, the lunch 0.57. Absolute cosine
values sit in a narrow band (about 0.5 to 0.85), so rank results, or calibrate a threshold on
your own data. Don't reuse a number from another model.

### Long text

Anything over 1024 tokens is refused, so you never silently embed only the first part:

```json
413 {"error": "input is 5608 tokens; the limit is 1024 — shorten it or set `chunk`", "tokens": 5608, "max_tokens": 1024, "input_index": 0}
```

With `chunk`, koblem splits on token boundaries and returns every piece:

```json
{"embeddings": [[...first chunk of input 0...]],
 "chunks": [{"input_index": 0, "chunk_index": 0, "char_start": 0,    "char_end": 2292, "embedding": [...]},
            {"input_index": 0, "chunk_index": 1, "char_start": 1999, "char_end": 4287, "embedding": [...]}, ...],
 "model": "google/embeddinggemma-2", "dim": 768}
```

- `max_tokens` includes the prefix and title; `overlap_tokens` must be smaller than what's left.
- `char_start`/`char_end` count Unicode code points (Python `str` indexing, not bytes or UTF-16).
- `embeddings` holds only each input's *first* chunk. Store the `chunks` if you want the rest.
- Chunks cost the most when a fact sits across a boundary, and when one long email turns into
  ten vectors that crowd the results. Overlap fixes the first. For the second, keep one row per
  chunk, then collapse hits per source document (take the max score) before ranking.

For email, a good default is to embed subject + body (with the subject as the `title`),
trimmed of quoted replies and signatures. Most mail then fits in one chunk. Use
`chunk: {"max_tokens": 1024, "overlap_tokens": 128}` for the rest.

## Images (and mixed input): `POST /v1/embeddings`

The OpenAI `/v1/embeddings` shape that llama-server speaks. Each `input` item is a string, or
a `content` list holding an image as a data URL (or an http URL the server can fetch):

```json
{"input": [
  {"content": [{"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ..."}}]},
  "task: search result | query: a dog on a beach"
]}
```

```json
{"object": "list", "data": [{"embedding": [...], "index": 0}, {"embedding": [...], "index": 1}],
 "model": "google/embeddinggemma-2", "usage": {"prompt_tokens": 70, "total_tokens": 70}}
```

- **This route adds no prefixes.** Write them yourself on text items (same strings as the
  table above), or embed the text through `/v1/text_embeddings` instead. Images take no prefix.
- Sort `data` by `index`; don't rely on its order.
- Images go to `embed-vision`. It is GPU-gated: when it isn't loaded, the first
  request waits for it to load (1–4 s), and it can also wait behind a heavy job holding the
  card. After that, about 25 ms per image. Send 4 images per request with up to 4 requests in
  flight; one big request is slower.
- Each image costs 70 tokens and is resized to fit, so a 768-px-wide JPEG is plenty. Bigger
  uploads only cost bandwidth.

## Storing vectors

- Keep `model` with every vector. If the model ever changes, old vectors are meaningless
  against new ones and must be re-embedded, not compared.
- Matryoshka: you may keep only the first 512, 256 or 128 floats to save space, **then
  re-normalise** (divide by the new length). Quality drops gradually; 256 is usually fine for
  search. Truncate queries and documents identically.
- Vectors from the GPU and from a CPU llama-server agree to cosine ≥ 0.9998, so mixing them
  is safe.
- pgvector: `vector(768)` with `vector_cosine_ops`; Meilisearch: a `userProvided` embedder of
  768 dimensions (you send `_vectors` and query with `vector`).

## Talking to llama-server directly

With kobbler's `docker-compose.gpu.yml`, `embed-text` is also published at
`http://<gpu-host>:18895` and `embed-vision` at `:18894` (`EMBED_TEXT_PORT` /
`EMBED_VISION_PORT`), both on the same `/v1/embeddings` shape. Prefer koblem. Going direct means:

- no prefixes: write them yourself;
- an over-long input is a 500 (`input (N tokens) is too large`), not a 413, and `truncate` does
  nothing;
- `embed-vision` direct bypasses the GPU gate, so it can load beside a job that needed the card.

## Operations

- `embed-text` loads at boot and stays loaded (~0.5 GB). If it's down, koblem falls back to
  `embed-vision`, and `gate_wait_ms` shows up in responses.
- To free its VRAM: `curl -X POST http://<gpu-host>:18895/unload`. The next request reloads
  it in about 1 s.
- Health: `GET :18895/health` → `{"status":"ok","loaded":true,...}`.
