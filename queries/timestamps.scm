; timestamps.scm
; Extract all timestamps (active and inactive) with their date components.
;
; Active timestamps use '<', inactive use '['.
; Plan entries (SCHEDULED, DEADLINE, CLOSED) wrap timestamps in (entry) nodes;
; bare timestamps appear inside paragraphs or headlines.
;
; Captures:
;   @ts       — the full timestamp node
;   @date     — date string (YYYY-MM-DD)
;   @day      — day-of-week abbreviation
;   @time     — time string HH:MM (optional)
;   @keyword  — SCHEDULED / DEADLINE / CLOSED (when inside a plan)
;
; Usage:
;   tree-sitter query queries/timestamps.scm <file.org>

; Planned entries (SCHEDULED / DEADLINE / CLOSED)
; The keyword is the entry_name field, not a keyword: field.
(plan
  (entry
    name: (entry_name) @keyword
    timestamp: (timestamp
      date: (date) @date
      day: (day)? @day
      time: (time)? @time) @ts))

; Active timestamps elsewhere (headline, paragraph, etc.)
(timestamp "<"
  date: (date) @date
  day: (day)? @day
  time: (time)? @time) @ts

; Inactive timestamps
(timestamp "["
  date: (date) @date
  day: (day)? @day
  time: (time)? @time) @ts
