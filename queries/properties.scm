; properties.scm
; Extract all property drawer entries across the document.
;
; Captures:
;   @prop_name  — property key (e.g. "CUSTOM_ID", "CREATED")
;   @prop_value — property value (may be absent for boolean properties)
;
; Usage:
;   tree-sitter query queries/properties.scm <file.org>

(property_drawer
  (property
    name: (expr) @prop_name
    value: (value) @prop_value))
