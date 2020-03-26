# Changelog

## Unreleased

* Implement `Display` for `Error`
* Add `find_device_identifier` to try multiple ways to find a valid device identifier
* Add `read_mfd_id` to read compact Mfr/Device ID using opcode 0x90
* Add support for devices that use 0x90 to obtain mfr/device ID

## 0.2.0 - 2020-03-25

* Add `BlockDevice` and `Read` traits, and allow writing to devices ([#5])
* Improve and fix handling of the JEDEC ID ([#16])

[#5]: https://github.com/jonas-schievink/spi-memory/pull/5
[#16]: https://github.com/jonas-schievink/spi-memory/pull/16

## 0.1.0 - 2019-05-24

Initial release.
