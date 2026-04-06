# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- Phase 3a — Reaper backend user identity integration (#6)([f281086](https://github.com/miguelgila/wren/commit/f2810861e599620bce96ce6c1831a5b22618feb0))
- Phase 5.1 multi-user identity support (#5)([a4afd18](https://github.com/miguelgila/wren/commit/a4afd181e7bd63d6a3facf4232f4d55c700e05cc))
- Preserve completed pods for 24h so bubo logs works after job completion([8e268ac](https://github.com/miguelgila/wren/commit/8e268ac6d9063e4d60e037ddf847f95a60a345f9))
- Add Helm chart, GitHub Actions CI/CD, and Makefile([35bcb29](https://github.com/miguelgila/wren/commit/35bcb297fa5b11781ce7843e2c1de361ee90d5a2))
- Add admission webhook validation for MPIJob and BuboQueue CRDs([aa71cdd](https://github.com/miguelgila/wren/commit/aa71cdd9b50e60a3f939bd591d7a80e79612ac7c))
- Add Kubernetes Lease-based leader election for HA deployments([81fc534](https://github.com/miguelgila/wren/commit/81fc534fde0d0e770a4fb196ef15a14233604bde))
- Add job dependency resolver and job array expansion([031c98e](https://github.com/miguelgila/wren/commit/031c98e628b1345ed3dc7e0becc417c96d98ca04))
- Add multi-factor fair-share priority scheduling([11ccd6a](https://github.com/miguelgila/wren/commit/11ccd6aa80d4ded1ecdd4c5a090fe26fde0fcc88))
- Add Slurm-style backfill scheduler for resource gap filling([7c727d1](https://github.com/miguelgila/wren/commit/7c727d1159cd9852b2346fcccf4cee4478fb4713))
- Comprehensive integration test suite with multi-node kind cluster([0ddec59](https://github.com/miguelgila/wren/commit/0ddec595b2f995b9893ade7a8faf66378ec20323))
- Add bare-metal MPI bootstrap helpers for Reaper backend([7efd4c7](https://github.com/miguelgila/wren/commit/7efd4c7fcb3733024f34209558810482f9d10568))
- Implement Reaper bare-metal execution backend([fd7b998](https://github.com/miguelgila/wren/commit/fd7b998b9b3bdf90ca5eeed2c1096262215f54b9))
- Add resource reservation, enhanced metrics, and wire into reconciler([360f1eb](https://github.com/miguelgila/wren/commit/360f1ebe228c88d140df661695983b9f7b961e7e))
- Add multi-queue management with BuboQueue policy enforcement([505c13b](https://github.com/miguelgila/wren/commit/505c13b32d35f1427aa4b9052d3b7f90ea0b6433))
- Add topology-aware node scoring for gang scheduler([6369545](https://github.com/miguelgila/wren/commit/6369545d01eff234850edd2533312962ba5bfd92))
- Add integration test infrastructure([c718884](https://github.com/miguelgila/wren/commit/c71888432a7a48f28b4a86d4b0590af6a895e1d7))
- Add CRD generation, K8s manifests, and Dockerfile([51a9550](https://github.com/miguelgila/wren/commit/51a955012c8f3195de0054270470dc48a2d505cd))
- Implement bubo-cli with submit, queue, cancel, status, and logs commands([dc168c8](https://github.com/miguelgila/wren/commit/dc168c8eeedb8ff81f8fd4ff2057e58660a5ff3b))
- Implement bubo-controller with reconciler, container backend, and metrics([a4316aa](https://github.com/miguelgila/wren/commit/a4316aa9025e0e3dfbc7551c4bf908058e61332d))
- Implement bubo-scheduler with gang scheduling, priority queue, and resource tracking([d79b8ff](https://github.com/miguelgila/wren/commit/d79b8ffd50a2035b2af0ae5baf3bbe03dbf7ff10))
- Implement bubo-core crate with CRDs, types, and backend trait([d067dbc](https://github.com/miguelgila/wren/commit/d067dbc56c674a8f24ffbe9de4ec199413a2dc20))
- Initial project setup with CI/CD and tooling([7b5d916](https://github.com/miguelgila/wren/commit/7b5d91625281dd0fdb7d2efcc0009abe29248abb))

### Bug Fixes

- Tolerate AlreadyExists on resource creation and simplify MPI launcher logic([9561488](https://github.com/miguelgila/wren/commit/95614883a43812794871ccdcc0050757e327ccff))
- Skip MPI launcher for simple jobs, fix leader election and pod watching([4e9fd30](https://github.com/miguelgila/wren/commit/4e9fd303e10ffeb822012b7add747fc23940130c))

### Refactoring

- Rename project from Bubo to Wren([1542e48](https://github.com/miguelgila/wren/commit/1542e483b62ec969ef381b9595bd4eb8dd6e070d))
- Rename MPIJob CRD to BuboJob across entire codebase([87403b4](https://github.com/miguelgila/wren/commit/87403b4d297dbdbad652225ac6475d7da2f801bd))
- Add Reaper-style CLI flags to integration test script([026771d](https://github.com/miguelgila/wren/commit/026771d1e405212e87ae1c2f9c8d9cb630fc8782))
- Rename project from scythe to bubo([2b87761](https://github.com/miguelgila/wren/commit/2b87761dce7f3cf6e8a65c894c7831877023c959))

### Documentation

- Documentation review, quickstart improvements, CLI context support (#4)([c33e187](https://github.com/miguelgila/wren/commit/c33e1874d5a6ec5383996843887799cd47227c6c))
- Rewrite README, fix CLAUDE.md, consolidate CI (#3)([3d61c62](https://github.com/miguelgila/wren/commit/3d61c624ff268f15857fb412f5755a18567124e8))
- Update milestones with accurate phase status and add multi-user Phase 5([2afca30](https://github.com/miguelgila/wren/commit/2afca30b13f69c4f9cf77191be74e913cddc39ca))
- Add quickstart script and self-contained example scenarios([1932b07](https://github.com/miguelgila/wren/commit/1932b0792536cad0d16b803806f9d3ef64776e01))
- Add Reaper and GPU training example manifests([f998087](https://github.com/miguelgila/wren/commit/f99808743a3f2ddd2d770a4a34a10459801b8492))

### Testing

- Add integration tests for pod preservation and log retrieval([c5da1e2](https://github.com/miguelgila/wren/commit/c5da1e2b2e5f363cceffd2604083c59e59db56eb))

### CI/CD

- Remove duplicate runs on merge and retry logic in integration tests (#34)([1a59432](https://github.com/miguelgila/wren/commit/1a59432deaf2d440a4ef635701ea9b7b580137d7))

### Miscellaneous

- Update Reaper subchart to OCI registry v0.2.27 (#35)([38f3628](https://github.com/miguelgila/wren/commit/38f36281517283a92d7fbdb962bae1e5d8a59fbb))
- Suppress dead_code warnings for scaffolded modules([cebe40a](https://github.com/miguelgila/wren/commit/cebe40a2a79f37c0461f3154dae508240e2d7348))
- Remove draft root files, add integration test plan([d2093f2](https://github.com/miguelgila/wren/commit/d2093f2f2b24924dc6f863b085c32ad064f3607c))

