---
name: humanizer
description: "Rewrite text so it reads like a person wrote it: strip the stock phrases, puffery, filler, and rhythm tics of machine-written prose, and match the user's own voice when they give a sample. Use on drafts, emails, posts, docs, and release notes."
category: productivity
triggers:
  - "humanize this"
  - "make this sound less like AI"
  - "remove the AI tone"
  - "make it sound natural"
  - "rewrite this in my voice"
---

# Humanizer

Edit prose so it sounds like one specific person with a point of view. Keep
the meaning, facts, names, numbers, and links; change the delivery.

## Input

- Pasted text: rewrite it and reply with the result.
- A file: `read_file`, then `apply_patch` section by section. Show what
  changed; never overwrite silently.
- A voice sample: read it first. Note sentence length, word level, how
  paragraphs open, punctuation habits, and pet phrases, then copy those
  habits. If they write "stuff", do not upgrade it to "elements".

## What to cut or fix

**Puffery and fake depth**
- Significance claims: "a testament to", "pivotal moment", "plays a vital
  role", "marks a shift", "evolving landscape". State the plain fact.
- Trailing clauses that add a fake insight: "..., highlighting its
  importance", "..., reflecting a broader trend".
- Ad copy: "nestled", "vibrant", "boasts", "breathtaking", "seamless",
  "world-class", "game-changer".
- Vague sources: "experts say", "observers note", "studies show". Name the
  source or drop the claim.
- The "despite its challenges, it continues to thrive" closer.

**Stock vocabulary**: delve, tapestry, underscore, showcase, foster, garner,
intricate, interplay, crucial, pivotal, robust, leverage, navigate (a
challenge), unpack, deep dive, double down, lean into, "when it comes to",
"in today's fast-paced world", "at the end of the day".

**Structure tics**
- "It's not just X, it's Y" and "not only... but also".
- Lists of three by reflex.
- Synonym cycling (the protagonist, the main character, the hero). Repeat the noun.
- False ranges: "from the Big Bang to dark matter".
- "serves as", "stands as", "features", "boasts" where "is" or "has" works.
- A question answered at once: "What makes it fast? Caching."
- Signposts: "Let's dive in", "Here's what you need to know".
- A heading followed by a sentence that repeats the heading.
- Staccato fragments and poster closers: "It just works. Every time."
- Unasked reassurance: "And that's okay."
- Opener tics: "Interestingly,", "Importantly,", "Ultimately,", "So,", "Look,".
- A metaphor that then needs explaining. Say the literal thing.

**Formatting tics**
- Em dashes everywhere: use commas, periods, or parentheses.
- Bold on random phrases, and "**Label:** sentence" bullets where a plain
  sentence works.
- Title Case Headings: use sentence case.
- Emoji as bullets or heading decoration.
- Curly quotes in text that otherwise uses straight ones.

**Chat leftovers**: "Great question!", "Certainly!", "I hope this helps",
"Let me know if...", "As of my last update", "While details are limited".

**Filler and hedges**: "in order to" becomes "to", "due to the fact that"
becomes "because", "it is important to note that" goes away, "could
potentially" becomes "may". Drop upbeat generic endings ("the future looks
bright").

## Add a pulse

Clean but lifeless text also reads as machine-made. Where the genre allows:

- Vary sentence length. Some short. Some that take their time.
- Take a position, and say where you are unsure or of two minds.
- Use "I" when the piece is personal.
- Prefer one concrete detail to a summary adjective.
- Allow a small aside; perfect symmetry reads assembled.

Match the register. A contract, an API reference, or a policy stays neutral
and exact: fix the tics, do not add jokes or opinions.

## Process

1. Mark every pattern above in the text.
2. Rewrite. Never invent quotes, studies, people, or numbers to sound specific.
3. Ask: "What still makes this read as machine-written?" Note the tells left.
4. Revise once more.

## Output

The final rewrite, then (unless the user only wants the text) two to four
bullets on what changed and any tell you chose to keep.
