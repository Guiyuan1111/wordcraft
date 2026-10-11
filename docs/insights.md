# Writing Insights

Writing Insights answers "how readable is my document, and where can it improve?" with
deterministic text statistics — no network, no language model. It lives in the
`wordcraft-insights` crate (layer L2, like `layout` and `docx`: it reads the document model and
nothing below `engine`).

## Where you find it

- **Desktop app** — Review › Proofing › **Writing Insights** opens the side panel. The panel
  refreshes itself whenever the document changes (the report is cached against the document
  revision, so scrolling and selecting never recompute it).
- **CLI** — `wordcraft-cli insights <file> [--json] [--notes]` prints the report as text or JSON.
- **Command bus** — the `review.insights` command (`{"includeTextBoxes": true|false}`) returns the
  report as JSON, with every finding annotated `pos: {story, path, off}` for jump-to-source.
- **MCP** — `review.insights` is exposed like every other command.

## What the report contains

| Section | Contents |
|---|---|
| Document | words (Latin words + CJK characters), characters, sentences, paragraphs, estimated reading and speaking time |
| Readability | Flesch Reading Ease with a plain-language band, and the school-grade band averaged over Flesch-Kincaid, Gunning Fog, SMOG, Coleman-Liau and ARI |
| Sentence length | a nine-bucket histogram (1–5, 6–10, …, 46+ words) and the five longest sentences, clickable in the panel |
| Top words | the 20 most frequent content words (stop words removed, Latin only) |
| Issues | long sentences, passive voice, wordy phrases, filler adverbs, repeated words, repeated sentence openers — each with a message and a click-to-jump range |

Mixed Latin/CJK text is measured with both traditions: words for the Latin runs, characters per
sentence for CJK (the measure Chinese style guides use). When a document is too short, or CJK
dominates, the Latin formulas return nothing rather than a nonsense grade.

## Design notes

- **Everything is statistics.** Tokenising (UAX-style word bounds, one token per CJK character),
  sentence splitting (abbreviation-aware, CJK full stops) and syllable counting (vowel-group
  method, digits count as one syllable) are all local and deterministic; the same input always
  yields the same report.
- **Findings carry positions, the UI stays dumb.** Detectors report byte ranges inside the
  paragraph's text; the report builder attaches the body block path. The panel only reads the
  JSON — `ui-egui` does not depend on the insights crate.
- **Analysis cannot fail.** Following the crate-wide no-panic discipline, hostile input yields an
  empty report, never a panic; `review.insights` is a pure command, so refreshing the panel never
  touches the undo stack.
- **Tables are analysed recursively.** A sentence inside a table cell is found, labelled with its
  full path (`[block, row, cell, paragraph]`), and jumps correctly. Notes and text boxes add to
  the counts (`--notes`) but never to findings or jump targets, since only the body has stable
  paths.
