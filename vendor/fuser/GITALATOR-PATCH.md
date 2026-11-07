# Gitalator's fuser patch

Vendored from the crates.io fuser 0.14.0 release (MIT; see LICENSE.md).
Original crate checksum: 2e697f6f62c20b6fad1ba0f84ae909f25971cf16e735273524e3977c94604cf8.

Local changes:
- Trim trailing whitespace in upstream deny.toml for repository diff checks.
- Add Filesystem::interrupt(req, original_unique) returning Result<(), c_int>.
  Default ENOSYS preserves upstream behavior. Ok sends no interrupt response;
  the filesystem must arrange its own original-request EINTR reply. Unknown or
  completed IDs return EAGAIN so the kernel can resolve the interrupt race.
- Dispatch FUSE_INTERRUPT to that callback and permit kernel-generated interrupts
  through the same UID access check as other kernel-generated operations.

Cancellation policy and reply ownership live in Gitalator, not this dependency.
Keep this patch small when updating the vendored release. Cargo uses it through
[patch.crates-io] in the root manifest. Registry packaging metadata is omitted.
