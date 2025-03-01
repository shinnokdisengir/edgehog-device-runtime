# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Update OS requirements to specify ttyd minimum version
- Update Astarte Device SDK to 0.8.3 release
  [#367](https://github.com/edgehog-device-manager/edgehog-device-runtime/pull/367)
- Add support for the
  [`CellularConnectionProperties`](https://github.com/edgehog-device-manager/edgehog-astarte-interfaces/blob/ed3b0a413a3d5586267d88d10f85c310584cb80b/io.edgehog.devicemanager.CellularConnectionProperties.json)
  via the D-Bus service `CellularModems`
  [#402](https://github.com/edgehog-device-manager/edgehog-device-runtime/pull/402)

## [0.8.1] - 2024-06-10

### Changed

- Substitute alpha version with 0.1.0 of edgehog-device-forwarder-proto dependency

## [0.7.2] - 2024-05-28

### Fixed

- Update sdk dependency to fix a purge property bug
  [#341](https://github.com/astarte-platform/astarte-device-sdk-rust/issues/341)

## [0.8.0] - 2024-03-25

### Added

- Add support for `io.edgehog.devicemanager.ForwarderSessionRequest` interface
- Add support for `io.edgehog.devicemanager.ForwarderSessionState` interface
- Add remote terminal support

### Changed

- Update the MSRV to rust 1.72.0

## [0.7.1] - 2023-07-03

### Added

- Add Astarte Message Hub library support.

## [0.7.0] - 2023-06-05

### Added

- Add support for `io.edgehog.devicemanager.OTAEvent` interface.
- Add support for update/cancel operation in `io.edgehog.devicemanager.OTARequest` interface.

### Removed

- Remove support for `io.edgehog.devicemanager.OTAResponse` interface.

## [0.6.0] - 2023-02-10

### Changed

- Update Astarte Device SDK to 0.5.1 release.

## [0.5.0] - 2022-10-10

### Added

- Initial Edgehog Device Runtime release.
