# Sources of `terminal`

The crate comes from the `terminal` crate of Arro (`crates/services/terminal`), by the same
author and under the same GPL-3.0-or-later license. The vendored ConPTY (`conpty.dll` and
`OpenConsole.exe` from NuGet, with its hash pins and fetch script) is removed: sessions on
Windows use the pseudoconsole of the system. The process job, the process tree walk and the
snapshot type are trimmed copies of Arro's `platform` and `base` crates.
