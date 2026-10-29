CSS formatting uses the PostCSS 8.5.29 layout contract from PR #40 (design head 4c8f080017282fd1283e8bf773fc5b0408689719). Install the locked dependencies with npm ci in web/.

From the repository root:

    npm --prefix web run format:css
    npm --prefix web run format:css:check
    node --test web/tools/format-css.test.cjs

The default command and helper invocation without a mode flag only check; they never write. The check alias has identical behavior.

Check mode compares every byte with the formatted result. On mismatch it exits nonzero, names the first differing line, and leaves the file untouched. Writing is a separate command, npm --prefix web run format:css:write (or the helper with --write). It formats style.css and reports the first changed line. Both modes enforce the parser version, ordered normalized AST equality, and a one-declaration negative control before any write.

To check another file without writing:

    node web/tools/format-css.cjs --check /path/to/file.css

The helper resolves dependencies and its default style.css relative to itself, so invocation does not depend on the working directory. It retains design's raw-layout rules: one declaration block per line, two-space indentation for nested rules, original rule order, and a final newline. No CSS files are changed by adding these tools.

Exit contract: 0 means the check is byte-identical (or an explicit write succeeded); 1 means a byte mismatch, printed on one stderr line as MISMATCH <absolute-path>:<first-differing-line>. Usage, parse, safety and I/O errors exit 2 and report CSS formatter: <reason>. Neither check failure nor usage failure writes.

Write mode runs the AST comparison and declaration negative control first. Only after all checks pass does it write a temporary file beside the target, flush and close it, and atomically rename it over the target. Before rename the original stays intact; failed writes remove the temporary file.
