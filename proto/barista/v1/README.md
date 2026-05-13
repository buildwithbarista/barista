# Barista IPC protocol — v1

This directory hosts the wire-protocol definitions for the
`barista`-to-`barback` worker IPC and the optional roastery
transport. Concrete `.proto` files land alongside the
implementation crate (`barista-ipc`) and the barback daemon in a
subsequent release.

Schema versioning follows `proto/barista/v<N>/` — incompatible
changes bump `N`. The current scaffold is v1.
