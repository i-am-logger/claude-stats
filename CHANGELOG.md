# Changelog

## [1.6.0](https://github.com/i-am-logger/claude-stats/compare/v1.5.5...v1.6.0) (2026-07-10)


### Features

* show named teammates with runtime and honest agent liveness ([#20](https://github.com/i-am-logger/claude-stats/issues/20)) ([4d7bfde](https://github.com/i-am-logger/claude-stats/commit/4d7bfde22b7c76a1a88662eab70c422f19316f4d))

## [1.5.5](https://github.com/i-am-logger/claude-stats/compare/v1.5.4...v1.5.5) (2026-07-09)


### Bug Fixes

* restore active-context detection, Claude 5 context windows, workflow subagents ([#18](https://github.com/i-am-logger/claude-stats/issues/18)) ([bf81e56](https://github.com/i-am-logger/claude-stats/commit/bf81e56709ecd99f551153381515a53323884a08))

## [1.5.4](https://github.com/i-am-logger/claude-stats/compare/v1.5.3...v1.5.4) (2026-03-21)


### Bug Fixes

* update quinn-proto and rustls-webpki for security advisories ([197f623](https://github.com/i-am-logger/claude-stats/commit/197f623ef2a6d30dec86ef1bf8b4603a5fcedee6))

## [1.5.3](https://github.com/i-am-logger/claude-stats/compare/v1.5.2...v1.5.3) (2026-03-21)


### Bug Fixes

* model-aware context window and persistent subagent display ([734426c](https://github.com/i-am-logger/claude-stats/commit/734426c5982ad219a39c69485075f1ba5ab85303))

## [1.5.2](https://github.com/i-am-logger/claude-stats/compare/v1.5.1...v1.5.2) (2026-03-08)


### Bug Fixes

* improve rate limiting with HTTP-date parsing and longer intervals ([4bd6c2d](https://github.com/i-am-logger/claude-stats/commit/4bd6c2d880b9799e2784e2a1dfc54ee4c830ba8e))

## [1.5.1](https://github.com/i-am-logger/claude-stats/compare/v1.5.0...v1.5.1) (2026-03-06)


### Bug Fixes

* show "No active contexts" when no sessions are found ([7983aaf](https://github.com/i-am-logger/claude-stats/commit/7983aaf988e107913f8e836e13655423b8c58470))

## [1.5.0](https://github.com/i-am-logger/claude-stats/compare/v1.4.5...v1.5.0) (2026-03-05)


### Features

* handle HTTP 429 rate limiting with countdown timer ([dadeb42](https://github.com/i-am-logger/claude-stats/commit/dadeb4245ecbab6b1dfae9c6878909853f9aedc1))


### Bug Fixes

* increase poll interval to 6 minutes, add rate limiting tests ([c339c11](https://github.com/i-am-logger/claude-stats/commit/c339c1105fa23b3d696a9e1145e7391469127202))
* increase usage and status poll intervals from 30s to 60s ([58141c0](https://github.com/i-am-logger/claude-stats/commit/58141c0188c607e7b4a248e62994cf975bb48dba))
* show "rate limited" in red on title lines, poll every 2 minutes ([fd102c4](https://github.com/i-am-logger/claude-stats/commit/fd102c4369d1054b5d8278aabbc42018f9dc0a59))

## [1.4.5](https://github.com/i-am-logger/claude-stats/compare/v1.4.4...v1.4.5) (2026-03-04)


### Bug Fixes

* show multiple Claude instances from same directory as separate sessions ([201b365](https://github.com/i-am-logger/claude-stats/commit/201b36599c141398d8bbf31a419c5749ebe52379))

## [1.4.4](https://github.com/i-am-logger/claude-stats/compare/v1.4.3...v1.4.4) (2026-02-28)


### Bug Fixes

* detect both foreground and background subagents ([e1787cc](https://github.com/i-am-logger/claude-stats/commit/e1787cc26b616d64e2807c04c0895fa913337e8d))

## [1.4.3](https://github.com/i-am-logger/claude-stats/compare/v1.4.2...v1.4.3) (2026-02-28)


### Bug Fixes

* co-locate viewmodels, extract session cache, kill missed mutants ([97affec](https://github.com/i-am-logger/claude-stats/commit/97affecd551ec5860df4027e0db114ad4166f437))

## [1.4.2](https://github.com/i-am-logger/claude-stats/compare/v1.4.1...v1.4.2) (2026-02-27)


### Bug Fixes

* improve session parsing accuracy and subagent task detection ([964eac3](https://github.com/i-am-logger/claude-stats/commit/964eac34f32d4b087fc08935a9104a51754ea572))

## [1.4.1](https://github.com/i-am-logger/claude-stats/compare/v1.4.0...v1.4.1) (2026-02-27)


### Bug Fixes

* check for self-version updates every 10 minutes instead of hourly ([d3ac844](https://github.com/i-am-logger/claude-stats/commit/d3ac844497726804e41f14b327d62e4ca169f221))

## [1.4.0](https://github.com/i-am-logger/claude-stats/compare/v1.3.0...v1.4.0) (2026-02-27)


### Features

* show update notification when newer claude-stats release is available ([3502bde](https://github.com/i-am-logger/claude-stats/commit/3502bde47062dc7a4c30903c8be8b24b181e3323))

## [1.3.0](https://github.com/i-am-logger/claude-stats/compare/v1.2.0...v1.3.0) (2026-02-27)


### Features

* add criterion benchmarks, proptest, cargo-deny, MSRV CI ([c099376](https://github.com/i-am-logger/claude-stats/commit/c099376ceeeac185eb00caf4e9f44f0098a357bb))


### Bug Fixes

* bump MSRV to 1.88 to match dependency requirements ([58dcd8a](https://github.com/i-am-logger/claude-stats/commit/58dcd8a4b0a1073450373256890497bee33bb09f))

## [1.2.0](https://github.com/i-am-logger/claude-stats/compare/v1.1.0...v1.2.0) (2026-02-27)


### Features

* resource tracking, session caching, logging, and robustness ([ba8182b](https://github.com/i-am-logger/claude-stats/commit/ba8182b833254cfc3d946626b75b2506cc0a70bd))

## [1.1.0](https://github.com/i-am-logger/claude-stats/compare/v1.0.0...v1.1.0) (2026-02-26)


### Features

* show account email and Claude Code version in header ([257d6e1](https://github.com/i-am-logger/claude-stats/commit/257d6e15e9c518313fe56d37e07ac65e3de1fe44))

## [1.0.0](https://github.com/i-am-logger/claude-stats/compare/v0.2.0...v1.0.0) (2026-02-26)


### ⚠ BREAKING CHANGES

* v1.0.0 stable release

### Features

* strict linting, safe indexing, and CI hardening ([2429b6b](https://github.com/i-am-logger/claude-stats/commit/2429b6b316ba282bde05e189802affb8a0931eb8))
* v1.0.0 stable release ([81a6a85](https://github.com/i-am-logger/claude-stats/commit/81a6a850642cd80be7b461d5734479503ba428a8))


### Bug Fixes

* clippy lint violations, updated header and screenshot ([e63724e](https://github.com/i-am-logger/claude-stats/commit/e63724ee8001b7d7744168b5a514549a6d1fd36e))

## [0.2.0](https://github.com/i-am-logger/claude-stats/compare/v0.1.0...v0.2.0) (2026-02-20)


### Features

* segmented usage bar and screenshot docs ([825f014](https://github.com/i-am-logger/claude-stats/commit/825f014324ed61323dc2ce9704a4db6aa563284d))

## 0.1.0 (2026-02-19)


### Features

* claude-stats TUI dashboard for Claude Code usage limits ([e8ec7ae](https://github.com/i-am-logger/claude-stats/commit/e8ec7ae32bbfd266c853fe6a73cb713aca6b2729))
