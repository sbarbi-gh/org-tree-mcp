; src-blocks.scm
; Extract all source blocks with their language and body.
;
; Captures:
;   @lang     — the language identifier (first parameter: field)
;   @contents — the raw block contents (code)
;
; The `.` anchor is critical: without it, header args like :tangle and
; :results are also returned as @lang captures because the grammar emits
; one parameter: field per token in the header line.
;
; Usage (run from grammar/tree-sitter-org):
;   tree-sitter query queries/src-blocks.scm <file.org>

(block
  name: (expr) @_src_kw
  (#eq? @_src_kw "src")
  . parameter: (expr) @lang
  contents: (contents) @contents)
