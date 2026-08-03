# Changelog

## [0.3.2](https://github.com/HomeOps/wmux/compare/v0.3.1...v0.3.2) (2026-08-03)


### Bug Fixes

* embed version metadata and stop building a packer-shaped binary ([#7](https://github.com/HomeOps/wmux/issues/7)) ([96fa66e](https://github.com/HomeOps/wmux/commit/96fa66e699f0ecfc72b303e6d043673c44b43633))

## [0.3.1](https://github.com/HomeOps/wmux/compare/v0.3.0...v0.3.1) (2026-08-02)


### Bug Fixes

* restore win32-input-mode decoding and default run depth to 2 ([fd11f64](https://github.com/HomeOps/wmux/commit/fd11f6498e1bf09a40b755041c4350fd629f9396))

## [0.3.0](https://github.com/HomeOps/wmux/compare/v0.2.0...v0.3.0) (2026-08-02)


### Features

* add headless run, send, and capture for scripted sessions ([434a901](https://github.com/HomeOps/wmux/commit/434a9014269867860024e6a770d1cde1a6206e4d))
* add wmux detach to free a client without the prefix key ([3faf552](https://github.com/HomeOps/wmux/commit/3faf552acc32a20312498b6dbbec908babd25c91))
* let wmux detach default to the session it is running inside ([f5a98d8](https://github.com/HomeOps/wmux/commit/f5a98d888ab5281d60ddbd6ad8abfee85ab55210))


### Bug Fixes

* read console input via ReadFile so the detach prefix is intercepted ([06f4e7b](https://github.com/HomeOps/wmux/commit/06f4e7ba5e30002560c0ed6970b9a3df823f7555))

## [0.2.0](https://github.com/HomeOps/wmux/compare/v0.1.0...v0.2.0) (2026-08-02)


### Features

* terminal session persistence for Windows ([757a816](https://github.com/HomeOps/wmux/commit/757a816a36084fc8d2b3950bf77e5624d046dd76))
