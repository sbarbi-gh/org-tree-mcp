; directives.scm
; Extract all #+KEY: value directives (file-level and section-level).
;
; Captures:
;   @name  — directive keyword (e.g. "TITLE", "AUTHOR", "PROPERTY")
;   @value — directive value string (optional)
;
; Usage:
;   tree-sitter query queries/directives.scm <file.org>

(directive
  name: (expr) @name
  value: (value)? @value)
