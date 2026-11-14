; Kotlin highlights for fwcd/tree-sitter-kotlin. The first pattern that
; captures a node sets its colour, so specific patterns come before general
; ones; a node inside another's range paints over it (an interpolation
; inside a string).

; Comments, strings, numbers

[(line_comment) (multiline_comment) (shebang_line)] @comment

(string_literal) @string
(character_literal) @string
(character_escape_seq) @string.escape
[
  (interpolation_identifier_start)
  (interpolation_expression_start)
  (interpolation_expression_end)
] @punctuation.special
(interpolated_identifier) @none
(interpolated_expression) @none

[
  (integer_literal)
  (long_literal)
  (hex_literal)
  (bin_literal)
  (unsigned_literal)
  (real_literal)
] @number
[(boolean_literal) (null_literal)] @constant.builtin

; Annotations, before types: `@JvmStatic` names a type

(annotation "@" @attribute)
(annotation (use_site_target) @attribute)
(annotation (user_type (type_identifier) @attribute))
(annotation (constructor_invocation (user_type (type_identifier) @attribute)))
(file_annotation ["@" "file" ":"] @attribute)
(file_annotation (user_type (type_identifier) @attribute))
(file_annotation (constructor_invocation (user_type (type_identifier) @attribute)))

; Functions: declared, then called by name or through a receiver

(function_declaration (simple_identifier) @function)
(call_expression . (simple_identifier) @function)
(call_expression
  (navigation_expression (navigation_suffix (simple_identifier) @function) .))

; Types and constants

(type_identifier) @type
(enum_entry (simple_identifier) @constant)
((simple_identifier) @constant
  (#match? @constant "^[A-Z][A-Z0-9_]+$"))

; Properties

(class_parameter (simple_identifier) @property)
(navigation_suffix (simple_identifier) @property)

; Keywords

[
  (class_modifier)
  (member_modifier)
  (function_modifier)
  (property_modifier)
  (platform_modifier)
  (variance_modifier)
  (parameter_modifier)
  (visibility_modifier)
  (reification_modifier)
  (inheritance_modifier)
] @keyword

[
  "package" "import" "typealias"
  "class" "interface" "object" "fun" "val" "var" "constructor" "init"
  "companion" "by" "where" "context"
  "if" "else" "when" "for" "while" "do" "in" "is" "as" "as?"
  "try" "catch" "finally" "throw" "return" "return@" "break" "break@"
  "continue" "continue@"
  "this" "this@" "super" "super@"
] @keyword
(getter "get" @keyword)
(setter "set" @keyword)

; Operators and punctuation

(type_arguments ["<" ">"] @punctuation.bracket)
(type_parameters ["<" ">"] @punctuation.bracket)
[
  "!" "!=" "!==" "=" "==" "===" ">" ">=" "<" "<=" "||" "&&"
  "+" "++" "+=" "-" "--" "-=" "*" "*=" "/" "/=" "%" "%="
  "?." "?:" "!!" ".." "..<" "->" "::"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["." "," ";" ":"] @punctuation.delimiter
