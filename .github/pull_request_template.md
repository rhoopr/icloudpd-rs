## Summary

<!-- What changed and why. Link related issues with "Fixes #123" or "Closes #123". -->

## Contract and risk

<!-- State the acceptance criteria and name any affected safety-contract ID. Note risks, tradeoffs, follow-up work, and independent/adversarial review results. Write "None" when not applicable. -->

## Test plan

<!-- List exact commands, focused scenario slices, and edge cases checked. -->

## Regression proof

<!-- For a bug fix or safety-contract change, name the focused test and how you confirmed it fails with the defect present. Write "Not applicable" when it is not a regression. -->

## State transition proof

<!-- Changes to durable configuration, filesystem paths, media publication, metadata, SQLite state, retry work, or provider checkpoints require a state-transition proof through the production call graph. Fill in each item, or write "Not applicable" with a concrete reason. -->

- Initial durable state:
- Controlled mutation:
- Production cycle:
- Durable outcome:
- Steady-state cycle:
- Deliberate defect mutation:

## Checklist

- [ ] `just gate` passes, or the unchecked item is explained in the test plan
- [ ] Behavior changes include focused tests
- [ ] CLI, docs, workflow, Docker, systemd, or Homebrew surface changes were checked against their matching files
