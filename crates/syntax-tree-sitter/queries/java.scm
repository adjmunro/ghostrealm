; Patterns put before tree-sitter-java's own query, whose first pattern
; capturing a node sets its colour: an annotation's `@` is part of it, not
; an operator.

(annotation "@" @attribute)
(marker_annotation "@" @attribute)

