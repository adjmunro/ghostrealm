; Patterns put before tree-sitter-rust's own query. Of the captures of one
; range, a coloured one beats a plain one, then the first pattern wins: a
; method call's name is a function, not a property; an all-caps name is a constant (the shipped pattern's regex
; ends in a stray `'`, so it never matches); numbers are numbers, not
; constants.

(call_expression
  function: (field_expression field: (field_identifier) @function.method))
(generic_function
  function: (field_expression field: (field_identifier) @function.method))
((identifier) @constant
  (#match? @constant "^[A-Z][A-Z\\d_]+$"))
[(integer_literal) (float_literal)] @number
