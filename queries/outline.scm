; outline.scm
; Extract all headlines with their depth (star count), TODO keyword,
; title text, and tags.
;
; Captures:
;   @stars   — the stars node (its text length == heading depth)
;   @keyword — optional TODO/DONE keyword (first expr in item, if a keyword)
;   @title   — the full item node (headline text incl. keyword)
;   @tag     — each individual tag (zero or more per headline)
;
; Usage:
;   tree-sitter query queries/outline.scm <file.org>

(section
  headline: (headline
    stars: (stars) @stars
    item: (item) @title
    tags: (tag_list
      (tag) @tag)?))
