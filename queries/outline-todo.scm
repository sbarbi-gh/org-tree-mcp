; outline-todo.scm
; Extract headlines whose item starts with a recognised TODO keyword.
;
; Captures:
;   @stars   — stars node (depth)
;   @keyword — the TODO/DONE/NEXT/WAITING/... keyword
;   @title   — full item node
;
; The anchor (.) ensures @keyword is the FIRST expr inside the item,
; matching org-mode convention where the keyword precedes the title text.
;
; Usage:
;   tree-sitter query queries/outline-todo.scm <file.org>

(section
  headline: (headline
    stars: (stars) @stars
    item: (item
      . (expr) @keyword
      (#any-of? @keyword "TODO" "DONE" "NEXT" "WAITING" "CANCELLED")) @title))
