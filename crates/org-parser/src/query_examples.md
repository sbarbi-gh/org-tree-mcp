# Tree-sitter query examples for the org grammar

## Note on `expr` predicates

`expr` is the grammar's atomic text token (a whitespace-delimited word). Text
predicates (`#eq?`, `#match?`) on `expr` only work reliably when the `expr`
node appears inside a structural parent. A bare `(expr) @e (#eq? @e "X")`
at the root of a query is split into two internal patterns by the tree-sitter
query compiler, causing predicates to be silently ignored.

**Workaround**: wrap in a wildcard parent `(_ ...)`, or use a known structural
parent. The wildcard costs nothing in practice:

```scheme
; BROKEN — predicate ignored, returns every expr in the document
(expr) @e (#eq? @e "CUSTOM_ID")

; CORRECT — returns only expr nodes equal to "CUSTOM_ID"
(_ (expr) @e (#eq? @e "CUSTOM_ID"))
```

The examples below follow this rule throughout.

---

## All headlines (outline)
Returns every section with its stars (depth), title, and tags.

```scheme
(section
  headline: (headline
    stars: (stars) @stars
    item: (item) @title
    tags: (tag_list (tag) @tag)?))
```

## Headlines with a specific tag
```scheme
(section
  headline: (headline
    item: (item) @title
    tags: (tag_list
      (tag) @tag
      (#eq? @tag "notebook"))))
```

## TODO / DONE headlines
First expr in item is the keyword (anchored with `.`).
```scheme
(section
  headline: (headline
    stars: (stars) @stars
    item: (item
      . (expr) @keyword
      (#any-of? @keyword "TODO" "DONE" "NEXT" "WAITING" "CANCELLED")) @title))
```

## Headlines matching a text pattern
```scheme
(section
  headline: (headline
    item: (item) @title
    (#match? @title "experiment")))
```

## All source blocks (language + body)
`@lang` is the language token (anchored with `.` to avoid capturing
header args like `:tangle`); `@contents` is the block body.
```scheme
(block
  name: (expr) @_kw (#match? @_kw "(?i)^src$")
  . parameter: (expr) @lang
  contents: (contents) @contents)
```

## Source blocks filtered by language
```scheme
(block
  name: (expr) @_kw (#match? @_kw "(?i)^src$")
  . parameter: (expr) @lang (#eq? @lang "python")
  contents: (contents) @contents)
```

## Property drawer entries
```scheme
(property_drawer
  (property
    name: (expr) @prop_name
    value: (value) @prop_value))
```

## Specific property by name (e.g. CUSTOM_ID)
```scheme
(property_drawer
  (property
    name: (expr) @_n (#eq? @_n "CUSTOM_ID")
    value: (value) @custom_id))
```

## File-level directives (#+KEY: value)
```scheme
(directive
  name: (expr) @name
  value: (value)? @value)
```

## Org-mode file links ([[file:...]])
Bracket links are tokenised as a single `expr` spanning `[[file:...][desc`.
Use the wildcard parent so the `#match?` predicate is not silently ignored.
```scheme
(_ (expr) @link (#match? @link "^\\[\\[file:"))
```

## SCHEDULED / DEADLINE / CLOSED timestamps
```scheme
(plan
  (entry
    name: (entry_name) @keyword
    timestamp: (timestamp
      date: (date) @date
      day: (day)? @day
      time: (time)? @time) @ts))
```

## Active timestamps anywhere in the document
```scheme
(timestamp "<"
  date: (date) @date
  day: (day)? @day
  time: (time)? @time) @ts
```

## Checklist items (all)
```scheme
(listitem
  checkbox: (checkbox) @checkbox
  contents: (paragraph) @text)
```

## Unchecked items only
```scheme
(listitem
  checkbox: (checkbox) @checkbox
  (#not-match? @checkbox "x|X|\\-")
  contents: (paragraph) @text)
```
