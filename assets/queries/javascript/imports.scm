; jetl import edges: static imports, re-exports, import() and require() of string literals.
(import_statement source: (string) @import.path)
(export_statement source: (string) @import.path)
(call_expression function: (import) arguments: (arguments . (string) @import.path))
(call_expression function: (identifier) @_req arguments: (arguments . (string) @import.path .) (#eq? @_req "require"))
