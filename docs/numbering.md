# Numbering resolution

DXPDF keeps OOXML numbering in two stages. The parse model preserves each
`w:abstractNum`, its local `w:lvl` definitions, an optional
`w:numStyleLink`, and every concrete `w:num` instance with its
`w:lvlOverride` entries. The resolve stage then produces the flat
`NumId -> levels` map consumed by layout and list-label formatting.

## Numbering-style links

`w:numStyleLink` does not name another abstract numbering definition
directly. It names a numbering style from `styles.xml`; that style's resolved
`w:pPr/w:numPr/w:numId` names the concrete numbering instance that supplies
the levels. Style inheritance is resolved before numbering, so a numbering
reference inherited through `w:basedOn` is available to this lookup.

The resolver follows this chain recursively and memoizes completed concrete
numbering instances. A per-chain visiting set detects cycles. Cycle failures
propagate to the outermost lookup before fallback, so the selected fallback
cannot depend on hash-map iteration order.

After the linked levels are copied, the current (outer) `w:num` instance's
full-level and start overrides are applied. The result is stored under that
outer `numId`; list counters therefore retain the paragraph's original
numbering identity rather than sharing the linked style's counter.

## Invalid links and the zero sentinel

The current abstract definition's local levels are the conservative fallback
when a link cannot be followed. This includes:

- a missing style or a resolved style without `w:numPr/w:numId`;
- a missing concrete numbering instance;
- a concrete instance whose `abstractNumId` is missing; and
- a link cycle.

`numId="0"` in paragraph properties explicitly disables numbering. It is
never followed as a numbering-style link, even if malformed input also
declares a concrete `w:num` whose ID is zero. Outer overrides still apply to
fallback levels.

This resolution path is intentionally limited to `w:numStyleLink`.
`w:styleLink` has separate semantics and is not inferred from this field.
