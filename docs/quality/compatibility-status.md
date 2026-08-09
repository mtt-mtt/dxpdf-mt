# Compatibility Status

Status: candidate, not released.

The branch contains substantial compatibility work beyond upstream dxpdf 0.4,
including package hardening, East Asian font selection, controlled font packs,
field handling, paragraph and document-grid behavior, table pagination,
sections, headers and footers, floating-object wrapping, VML fallbacks, emoji
routing and deterministic font subsetting.

## Current confidence

Higher-confidence areas include:

- direct conversion of the original Chinese compatibility fixture;
- East Asian theme-font and run-slot resolution;
- common fields, headers and footers;
- explicit page breaks and odd/even section separators;
- common fixed-height and split-table cases;
- controlled monochrome business symbols and color emoji routing;
- bounded DOCX package extraction;
- repeatable rendering with a controlled font pack.

Partial or long-tail areas include:

- Word-level AutoFit column solving;
- combinations of repeated table headers, vertical merges and row splitting;
- complex VML paths, grouped diagrams and nested text boxes;
- Tight/Through polygon wrapping and multi-object feedback layout;
- East Asian vertical-writing variants;
- exact layout under missing or version-different fonts.

## Latest documented laboratory evidence

The most recent fully narrated broad-corpus run before this repository import
converted 200/200 documents with no recorded crash or timeout. Its physical
page-count tolerance was 156/200 and non-empty page tolerance was 160/200; P95
was 1033 ms in that test environment. These numbers describe a historical lab
run, not the current Git commit, and therefore are not a release certification.

The same laboratory work found DOCX-source semantic coverage of at least 98% in
200/200 documents in a prior identified run. PDF extracted-character totals are
kept as anomaly signals because reference revisions, field errors, repeated
headers and missing `ToUnicode` mappings can distort them.

Before release, the current Git candidate must be rebuilt and requalified with a
recorded binary hash, controlled fonts, the broad corpus and the approved
business corpus. Until that report exists, this branch remains a draft
compatibility candidate.
