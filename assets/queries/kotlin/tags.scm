; From https://github.com/ast-grep/tree-sitter-kotlin queries/tags.scm at tree-sitter-kotlin-sg 0.4.1, MIT; see LICENSE.
; jetl changes: interface, enum class and method kinds (before the generic patterns, the first pattern for a node wins); properties only at top level and in class bodies, kind property (upstream: every property, locals included, as constant); supertype references.

; Classes
(class_declaration "interface" (type_identifier) @name) @definition.interface
(class_declaration "enum" (type_identifier) @name) @definition.enum
(class_declaration (type_identifier) @name) @definition.class

; Objects
(object_declaration (type_identifier) @name) @definition.class

; Functions (members, then top-level and local)
(class_body (function_declaration (simple_identifier) @name) @definition.method)
(enum_class_body (function_declaration (simple_identifier) @name) @definition.method)
(function_declaration (simple_identifier) @name) @definition.function

; Properties
(source_file (property_declaration (variable_declaration (simple_identifier) @name)) @definition.property)
(class_body (property_declaration (variable_declaration (simple_identifier) @name)) @definition.property)
(enum_class_body (property_declaration (variable_declaration (simple_identifier) @name)) @definition.property)

; Enum entries
(enum_entry (simple_identifier) @name) @definition.constant

; Type aliases
(type_alias (type_identifier) @name) @definition.type

; Companion objects (only named ones)
(companion_object (type_identifier) @name) @definition.class

; Function calls
(call_expression (simple_identifier) @name) @reference.call

; Method calls via navigation
(call_expression (navigation_expression (navigation_suffix (simple_identifier) @name))) @reference.call

; Constructor invocations (class references)
(constructor_invocation (user_type (type_identifier) @name)) @reference.class

; Supertypes
(delegation_specifier (user_type (type_identifier) @name)) @reference.implementation
