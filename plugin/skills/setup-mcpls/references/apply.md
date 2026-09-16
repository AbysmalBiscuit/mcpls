# Letting mcpls write files: `[apply]`

`[apply]` is the only table that lets mcpls write to the source tree. Without it mcpls is read-only: every tool returns an edit to read, and nothing on disk changes. Every field defaults to `false`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `rename` | boolean | `false` | `rename_symbol` may write when called with `apply: true`. A rename rewrites every file referencing the symbol, not just the one named. |
| `format_document` | boolean | `false` | `format_document` may write when called with `apply: true`. Confined to the file named. |
| `code_actions` | boolean | `false` | Enables the `apply_code_action` tool. The widest of the three: an action can create, move, or delete files as well as edit them, and can carry a command the server runs itself, which may send further edits back while it runs. |
| `allow_file_deletion` | boolean | `false` | Permits any operation that destroys a file's content: an explicit delete, a create that overwrites an existing file, or a rename onto an existing destination. It gates all three for every tool above. With it `false`, an edit containing any of them is refused whole and nothing is written. |

Turning a key on hands the write to the language server: it decides which files the edit touches and what goes in them. mcpls confines every path to `workspace.roots` (resolving symlinks first), applies the whole edit or none of it, and reports what it wrote. It does not review the content.

There is no undo beyond the run itself. A step failing partway through one apply reverses the completed steps, and the error names any file it could not restore. Once an apply returns successfully the change is on disk, and mcpls keeps no record of what was there before.

mcpls refuses an edit outright when:

- there are no `workspace.roots`, or a path resolves outside them;
- it would change or destroy a file the filesystem marks read-only (one refusal names all such files);
- a file it edits holds no readable text;
- two entries address one document;
- it creates a file in a directory that does not exist;
- it edits a file under a directory the same edit moves.
