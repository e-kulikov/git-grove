# Changelog

## [0.6.1](https://github.com/e-kulikov/git-grove/compare/v0.6.0...v0.6.1) (2026-10-07)


### Bug Fixes

* **fsx:** widen nlink to u64 so this builds on aarch64 ([8e4a2a6](https://github.com/e-kulikov/git-grove/commit/8e4a2a6933be4ea2c1c6823825e48103a7795859))
* **fsx:** widen nlink to u64 so this builds on aarch64 ([2621f26](https://github.com/e-kulikov/git-grove/commit/2621f26164eb5c2262d9f81712629b769e352a8f))
* **tests:** stop hardcoding the crate version in release/smoke tests ([#12](https://github.com/e-kulikov/git-grove/issues/12)) ([314e071](https://github.com/e-kulikov/git-grove/commit/314e0712bf8f5d197d22df2d1e0f3ef22286a49d))

## [0.6.0](https://github.com/e-kulikov/git-grove/compare/v0.5.0...v0.6.0) (2026-10-06)


### Features

* **cli:** add embedded skill output ([b8948c0](https://github.com/e-kulikov/git-grove/commit/b8948c0270e91d7e9fdcc5abee52c005bca824b6))
* **hooks:** add grove metadata guard ([52a77ea](https://github.com/e-kulikov/git-grove/commit/52a77ea1034dd41f5f3ddda8d750670fc5cd4693))
* **hooks:** deny unrecognized programs that mention a tracked wrapper name ([211da28](https://github.com/e-kulikov/git-grove/commit/211da285a333fd84ba8dc359938dbd03f8185038))
* **setup:** add safe hook configuration merge ([9a21d24](https://github.com/e-kulikov/git-grove/commit/9a21d245f91edc763f9dc628c9907f948b78c3a2))
* **setup:** configure local agent guards ([07fc29d](https://github.com/e-kulikov/git-grove/commit/07fc29d364c9337894cf9518878cacd83bf41958))
* **setup:** target one worktree, provision new ones, fix the Codex config ([c49e80f](https://github.com/e-kulikov/git-grove/commit/c49e80f3dcc5a9bee6743cc6e69611cde4a0bab9))


### Bug Fixes

* **hooks:** accept env's empty-name assignment argument too ([b0bba8c](https://github.com/e-kulikov/git-grove/commit/b0bba8c2bfbf11bebcf907a475992a11b0e03844))
* **hooks:** accept env's own looser NAME=VALUE shape, not the shell's stricter one ([950825a](https://github.com/e-kulikov/git-grove/commit/950825a18d5fe366dd5f70fefbdde68d97eabb9b))
* **hooks:** apply the universal fallback in quoted content too, refined to require an adjacent flag ([6ef2556](https://github.com/e-kulikov/git-grove/commit/6ef2556b7d79fc9fca6d1af5bb04d6d8790c5a04))
* **hooks:** classify clustered env/timeout options instead of exact-matching ([2ee5b00](https://github.com/e-kulikov/git-grove/commit/2ee5b00d29023f0650e4a04110c8ca9c25633aac))
* **hooks:** close a Bash fd-redirection bypass; correct docs on matchers ([33e45de](https://github.com/e-kulikov/git-grove/commit/33e45de6fc4041e45c26d601eca10e57702758e6))
* **hooks:** close brace/tilde expansion, env-as-wrapper, and separate-value gaps ([8572e1d](https://github.com/e-kulikov/git-grove/commit/8572e1d3dc3c19ccb3ba33b12439c6c8589889bb))
* **hooks:** close glob, wrapper, and clustered-flag bypasses; fix -C false positives ([d453249](https://github.com/e-kulikov/git-grove/commit/d4532491b897525e5c8bbf6eb896531432997a89))
* **hooks:** close remaining -C gaps, document the interpreter-language limit explicitly ([ef6f175](https://github.com/e-kulikov/git-grove/commit/ef6f1752a6eb9ebe56df9438586cb374956092ba))
* **hooks:** deny Bash commands whose path resolution a cd/subshell/substitution makes unsound ([15face7](https://github.com/e-kulikov/git-grove/commit/15face7305ee46ae414dceefc08d324fa4830720))
* **hooks:** deny dispatch wrappers, reserved words, and unrecognized assignment/redirect grammar in the command-word scan ([78ecbe5](https://github.com/e-kulikov/git-grove/commit/78ecbe51bfe69f13263af97ce1eec7a4341d33ec))
* **hooks:** deny env -S/--split-string outright, unquoted or not ([903f547](https://github.com/e-kulikov/git-grove/commit/903f547abff127438de242e53af5530cce054e2d))
* **hooks:** deny rather than skip a quoted segment with an unrecognized prefix ([9039f29](https://github.com/e-kulikov/git-grove/commit/9039f2995f5ad3aafae7bbe3f399f4e727309fde))
* **hooks:** drop a redirect's separate-word target too, not just a glued one ([29c8431](https://github.com/e-kulikov/git-grove/commit/29c8431889e622887a203226bc5673fd6e006e75))
* **hooks:** extract the value half of a glued Bash option ([83a10f5](https://github.com/e-kulikov/git-grove/commit/83a10f568f744823be0ce538bb0c6db8526a4f60))
* **hooks:** fail closed on an unrecognized redirection shape instead of guessing it's safe ([f5ba854](https://github.com/e-kulikov/git-grove/commit/f5ba85411b0e02f521ef1b9a9ddbdcb825f12274))
* **hooks:** find a Bash command word past glued separators, assignments, and prefix redirections ([56213ba](https://github.com/e-kulikov/git-grove/commit/56213ba7d66bca6ad9263bea2c6e3795562daa35))
* **hooks:** find candidate paths inside a quoted multi-word argument ([e2e3ca2](https://github.com/e-kulikov/git-grove/commit/e2e3ca20bc8b7f52f5d8ac14ad40666542d3a82b))
* **hooks:** join backslash-newline line continuations, deny more reserved/dispatch words ([e92437e](https://github.com/e-kulikov/git-grove/commit/e92437e438c6ae3b2972d7d1a86611b41378316f))
* **hooks:** only deny brace expansion when it actually contains a comma or range ([71006f5](https://github.com/e-kulikov/git-grove/commit/71006f537c1bb5ca16e53bb3392ec456259c01a5))
* **hooks:** only deny tilde expansion at an actual word boundary ([46ddc88](https://github.com/e-kulikov/git-grove/commit/46ddc88fac2f6f290c3ff66d2cc55c6e8ecc3fab))
* **hooks:** only treat a colon as a tilde boundary inside an assignment word ([de8159b](https://github.com/e-kulikov/git-grove/commit/de8159ba3b7beb41c89432d94e17a013d443afa4))
* **hooks:** precisely classify env's own -C cluster instead of the broad heuristic ([41b44a4](https://github.com/e-kulikov/git-grove/commit/41b44a4de1f28352cd22d14bf5ae3477c0e47d12))
* **hooks:** recognize env's -a/--argv0 and optional-argument signal options ([ee19ac6](https://github.com/e-kulikov/git-grove/commit/ee19ac6a9a794cface2c72935627de2e35df9137))
* **hooks:** recognize the full Bash redirection operator table ([16b9428](https://github.com/e-kulikov/git-grove/commit/16b9428901e3f904a4894edda9c7d58db846a7c0))
* **hooks:** recurse into quoted shell code, deny alias/shopt and long-form directory flags ([5d4b711](https://github.com/e-kulikov/git-grove/commit/5d4b711627e49e5adde602e36d3193fcfb1eda7a))
* **hooks:** shell-quote the hook executable, close a glued-redirection bypass, and stop mis-targeting setup ([9775558](https://github.com/e-kulikov/git-grove/commit/9775558abc4d5f51faecd244782a10ff3c8e6833))
* **hooks:** skip redirections standing in for a flag's own separate value ([8edc6d6](https://github.com/e-kulikov/git-grove/commit/8edc6d6de02dc8203487ba22a392ad7baff46688))
* **hooks:** stop treating git --exec-path as always taking a separate value ([8e074fe](https://github.com/e-kulikov/git-grove/commit/8e074fe9b7dd6234c3fea3759066b8e17ba474c1))
* **hooks:** track quote state per byte, not per whole token ([448fbf9](https://github.com/e-kulikov/git-grove/commit/448fbf9cf056f19a096ce99999ecbff8b80a071c))
* **hooks:** treat a line continuation as invisible for tilde-boundary tracking ([edfccfe](https://github.com/e-kulikov/git-grove/commit/edfccfed21e79b8a9264282509d03110c20a97c6))
* **hooks:** treat a mid-command redirection as transparent; recognize env's lone dash ([adb8538](https://github.com/e-kulikov/git-grove/commit/adb8538a37789889b36951844cfc066d32e8c0e4))
* **hooks:** treat timeout as a wrapper with a mandatory duration positional ([dd46d15](https://github.com/e-kulikov/git-grove/commit/dd46d15b8d80cc06a42c4a820a32beb6dc06cf3c))
* **hooks:** widen recursion evidence narrowly, scope -C to known directory-changing programs ([44e2242](https://github.com/e-kulikov/git-grove/commit/44e2242ba7c4b3eb5b4b6d7ec95ed26556eba034))
* **transaction:** accept a schema-1 adoption journal so an interrupted 0.5 adopt can still be resumed ([beb0d10](https://github.com/e-kulikov/git-grove/commit/beb0d106aba160bee713b69e7c50dfa0aab57d98))
* **transaction:** accept the abort-direction erased guide transition too ([3f32d17](https://github.com/e-kulikov/git-grove/commit/3f32d17a1de7dd38ec6e7f5cf3c1153094b7f98c))
* **transaction:** recognize a torn schema-1 journal across the erased guide transition ([1b2021e](https://github.com/e-kulikov/git-grove/commit/1b2021e65d9ef9cfb3e67e8b7fffd163f6032606))
* **transaction:** require Progress::Forward in the erased-guide-transition check ([6bba430](https://github.com/e-kulikov/git-grove/commit/6bba4301402f5ec60aa33633aef839f30165bb9e))
* **transaction:** restore schema-1 semantic validation and reject the impossible native-to-legacy direction ([90f3796](https://github.com/e-kulikov/git-grove/commit/90f37967a4e33c543d3e040e7b55361faaa9cb6d))
* **transaction:** tighten the erased-guide-transition exception to the exact legal shape ([dd76f0a](https://github.com/e-kulikov/git-grove/commit/dd76f0a548e7fcd2a8fa3e9b4fbc1e3371d97215))
* **transaction:** validate schema-1 generation pairs directly, before trusting the upgraded comparison ([653d93e](https://github.com/e-kulikov/git-grove/commit/653d93efdceb285766fc3ae5fc6a08ce906f6630))
