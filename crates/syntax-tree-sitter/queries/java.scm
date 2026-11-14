; Patterns put before tree-sitter-java's own query. Of the captures of one
; range, a coloured one beats a plain one, then the first pattern wins: an
; annotation's `@` is part of it, not an operator.

(annotation "@" @attribute)
(marker_annotation "@" @attribute)

