# Security

## Reporting a problem

If you find a security problem in Mixrail, please don't open a public issue. Report it
privately instead: on GitHub, go to the **Security** tab of this repository and choose
**Report a vulnerability**. You'll get an answer as soon as possible.

## Supported versions

Only the latest release gets security fixes. Updating to it is always the fix.

## How Mixrail stays up to date

- The Rust libraries built into Mixrail are checked weekly against the RustSec advisory
  database, and Dependabot proposes updates as soon as a fix exists.
- System libraries (PipeWire, ALSA, the C library) are not built in: your distribution
  updates those with its normal updates.
