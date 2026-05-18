## Summary

<!-- What does this change do, and why? -->

## Checklist

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --features fdk-aac --lib --no-deps` is clean
- [ ] `cargo test --features fdk-aac --lib` passes locally
- [ ] If touching codec parameters or DSP behaviour, `docs/codecs.md`
      and/or `docs/codec-implementation.md` are updated to match
- [ ] If adding a new codec / platform, sources are cited in the PR
      description and in the docs

## Notes for reviewers

<!-- Anything you'd like a closer look at, known limitations, etc. -->
