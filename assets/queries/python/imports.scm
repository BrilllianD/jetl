; jetl import edges: from-imports join each name to the module (M.n).
(import_statement name: (dotted_name) @import.path)
(import_statement name: (aliased_import name: (dotted_name) @import.path))
(import_from_statement module_name: (_) @import.path name: (dotted_name) @import.name)
(import_from_statement module_name: (_) @import.path name: (aliased_import name: (dotted_name) @import.name))
(import_from_statement module_name: (_) @import.path (wildcard_import))
