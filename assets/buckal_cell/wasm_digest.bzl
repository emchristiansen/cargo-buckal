# The buckal cell's `wasm_sha256` rule, which BUCK files cargo-buckal emits load as
# `load("@buckal//:wasm_digest.bzl", "wasm_sha256")` for a wasm module that a
# `[package.metadata.buck2.test-env]` `{ wasm_sha256 = "<package>" }` declaration
# names. Copy it into the repository's buckal cell beside `wrapper.bzl`, with
# `tool/wasm_sha256.py` and the `wasm_sha256` target of `tool/BUCK.wasm_sha256`.

def _wasm_sha256_impl(ctx: AnalysisContext) -> list[Provider]:
    out = ctx.actions.declare_output(ctx.label.name)
    ctx.actions.run(
        cmd_args(ctx.attrs._tool[RunInfo], ctx.attrs.src, out.as_output()),
        category = "wasm_sha256",
    )
    return [DefaultInfo(default_output = out)]

# A file holding the lowercase hex SHA-256 of `src` (a module's `[cdylib]`), 64
# characters with no newline: the witness a test compares the module's bytes with.
wasm_sha256 = rule(
    impl = _wasm_sha256_impl,
    attrs = {
        "src": attrs.source(),
        "_tool": attrs.default_only(attrs.exec_dep(providers = [RunInfo], default = "buckal//tool:wasm_sha256")),
    },
)
