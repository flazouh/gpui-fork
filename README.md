# gpui-fork

The four GPUI crates that [Atelier](https://github.com/flazouh/atelier) builds on, with the few changes that Atelier needs and Zed does not take.

- `gpui-pre`, `gpui-pre-apple`, `gpui-pre-wgpu` and `gpui-pre-macos` are the sources of the crates.io release 0.3.6, which is a snapshot of [Zed](https://github.com/zed-industries/zed). They are Apache-2.0. Each crate keeps its `LICENSE-APACHE`.
- The tag `upstream-0.3.6` is the untouched release. The branch `atelier-0.3.6` is that release plus our patches, one commit each. `git diff upstream-0.3.6..atelier-0.3.6` is the whole difference.
- `PATCHES.md` lists each patch, why it exists, and the test that holds it.

## Use it

In the root `Cargo.toml` of a project that depends on GPUI through `gpui-kit`:

```toml
[patch.crates-io]
gpui-pre = { git = "https://github.com/flazouh/gpui-fork", rev = "<commit>" }
gpui-pre-apple = { git = "https://github.com/flazouh/gpui-fork", rev = "<commit>" }
gpui-pre-wgpu = { git = "https://github.com/flazouh/gpui-fork", rev = "<commit>" }
gpui-pre-macos = { git = "https://github.com/flazouh/gpui-fork", rev = "<commit>" }
```

To try a change before it is pushed, point `--config 'patch.crates-io.gpui-pre.path=...'` at a local checkout.

## Rules for a patch

1. New renderer code goes in new files. An existing file changes only at the batch loop, the pipeline list and the end of a shader file.
2. No change to the layout of an existing GPU struct. New tags, new structs, new batch kinds.
3. Keep the upstream code style in these crates.
4. Each patch has a line in `PATCHES.md` and a test.
5. Move to a newer upstream release only when Atelier needs it: import the release as a commit on a new branch and rebase the patches onto it.

This is not an official Zed project.
