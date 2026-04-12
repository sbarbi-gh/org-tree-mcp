; src-blocks.scm
; Extract all source blocks with their language and body.
;
; Captures:
;   @lang     — the language identifier (first expr in the block name)
;   @contents — the raw block contents (code)
;
; Note: the block name node holds the full "src python :session ..." line.
; The first expr within it is the language token.
;
; Usage:
;   tree-sitter query queries/src-blocks.scm <file.org>

(block
  name: (expr) @_src_kw
  (#eq? @_src_kw "src")
  contents: (contents) @contents)
