# Search limits, completeness, and continuation

## Observed behavior

Against the Ubergraph release `2026-05-31`, these two requests give apparently
contradictory answers:

```text
GET /ubergraph/v/2026-05-31/search?q=buffalo&labels=true&limit=100
GET /ubergraph/v/2026-05-31/search?q=buffalo&labels=true&limit=1000
```

The first returns exactly 100 entities with `complete: true` and `next: null`. The
second returns 742 entities, also with `complete: true` and `next: null`. In the HTML
representation the first response therefore says "Complete: yes", even though a
larger `limit` exposes another 642 results.

This behavior predates the HTML query forms. The forms only made it convenient to
change `limit` and compare the results.

## What the implementation does

`request::Search` parses `limit` as the number of entity hits to retain. The answer
layer then:

1. asks hdtc for ranked literal hits within `candidate_budget`;
2. resolves each literal through the RDF permutations;
3. deduplicates the resulting occurrences by subject; and
4. stops as soon as `results.len() == limit`.

The completeness calculation does not treat that fourth stop as truncation. It reports
an incomplete response only when the response-byte budget, RDF-resolution candidate
budget, or text-index candidate budget is exhausted. Otherwise it reports
`complete: true`.

The relevant code is `crates/kgf-server/src/answer.rs`: the result-limit stops are in
`search`, while the completeness decision at the end considers only byte and candidate
exhaustion.

## Why there is no continuation cursor

The absence of a cursor is deliberate in the current implementation plan. Unit 17
states that search `limit` is the requested top-k, not a paging boundary. Ranked text
work is bounded by `candidate_budget`; the server does not promise arbitrary-depth
pagination through a global relevance order.

The difficulty is more than Tantivy pagination. The index ranks literal dictionary
terms, but `/search` returns entities. One literal may resolve to many subjects and one
subject may occur under many ranked literals. Subject deduplication therefore happens
after ranking and RDF resolution. A cursor containing only the next literal rank cannot
prevent a subject emitted on an earlier page from being emitted again when one of its
lower-ranked literals is encountered.

Exact continuation would require at least one of:

- rescanning the ranked prefix on every continuation to reconstruct the seen-subject
  set;
- putting the growing seen-subject set in the cursor;
- keeping request state on the server;
- defining a fixed retained ranking window and carrying enough state to traverse it;
  or
- building an entity-level search index whose native ranking unit matches the response
  unit.

These are design and cost choices, not an inherent impossibility of ranked search.
They conflict to different degrees with KGF's small opaque cursors, stateless immutable
version URLs, and bounded per-request work.

## The semantic conflict

The current result is consistent with the implementation plan's top-k interpretation:
`complete: true` means "the requested top-k was computed without exhausting a budget."
It does **not** mean that every matching entity was returned.

That meaning conflicts with the ordinary reading of `complete` and with `../kgf` doc
03 §3.6, which distinguishes a complete result from `page_limit` truncation and says
that silent truncation is prohibited. Elsewhere in the API, `limit` is a page size and
a full page is tested with a `limit + 1` sentinel before completeness is claimed.
Search is currently the exception, but its response does not expose that different
meaning.

Consequently this is not simply a presentation bug, nor is it accurate to call the
answer-layer behavior an accidental implementation bug. It is an unresolved contract
problem: the code follows the recorded top-k design, while the shared completeness
vocabulary suggests exhaustion of the full result set.

## Decisions needed

The API needs to choose and document one meaning:

1. Keep top-k semantics. Clarify that search completeness describes bounded ranking
   execution rather than match exhaustion, and expose a separate indication if callers
   need to know whether more entities matched. Renaming `limit` to `k` would also make
   the distinction clearer, but would change the request surface.
2. Apply the uniform completeness meaning. Probe for an additional distinct entity and
   return `complete: false`, `truncation_reason: "page_limit"`, and `next: null` when
   more matches exist. This is honest but offers no continuation and requires the
   envelope contract to allow a non-resumable page limit explicitly.
3. Make search pageable. Specify a bounded continuation model and accept one of the
   state, cursor-size, rescanning, or indexing costs above.

Until that choice is made, clients must not interpret `complete: true` from `/search`
as evidence that the returned entity count is the total number of matches.
