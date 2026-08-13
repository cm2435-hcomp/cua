# cua WinRestore — GNOME Shell helper (X11)

This small GNOME Shell extension lets `cua-driver` restore one exact minimized
X11 window without activating it. Mutter retains the authoritative minimized
state, and an ordinary X11 client cannot reliably clear that state without a
foreground transition.

It exposes `org.cua.WinRestore` on the user session bus:

- `GetVersion() -> uint` reports the helper contract version.
- `Restore(pid, xid, title) -> bool` requires one exact Shell-owned window,
  deminimizes it, and reports success only when Shell focus is unchanged.

Before calling it, `cua-driver` resolves the immutable D-Bus owner and verifies
that it is the current user's root-installed `gnome-shell` executable. It also
revalidates the exact `(pid, XID, title)` target through X11. Missing, stale,
ambiguous, or untrusted state is refused before dispatch. A failure after
Shell accepts the restore remains a possible-effect error.

## Install

```sh
~/.cua-driver/packages/current/x11-helper/install.sh
# From a source checkout, run ./install.sh in this directory.
# Then log out and back in once.
gnome-extensions info winrestore@cua  # State: ACTIVE
```

The helper is required only for minimized-window recovery on GNOME X11.
Ordinary visible-window observation and background actions continue to work
without it; minimized observation refuses safely when it is unavailable.
