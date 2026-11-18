# cardinal

cardinal arbitrates local compute resources and handles housekeeping for parallel coding agents running on one machine. It is a single binary that serves as both a CLI and a per-job runner, for macOS (arm64), Linux (x64), and Windows (x64).

Status: early development.

## Install

Each [release](https://github.com/shuymn/cardinal/releases) has one archive per platform and a `SHA256SUMS` file:

| Platform | Archive |
| --- | --- |
| macOS (arm64) | `cardinal-aarch64-apple-darwin.tar.gz` |
| Linux (x64, glibc) | `cardinal-x86_64-unknown-linux-gnu.tar.gz` |
| Windows (x64) | `cardinal-x86_64-pc-windows-msvc.zip` |

Each archive holds a directory of the same name with the `cardinal` binary (`cardinal.exe` on Windows), `LICENSE`, and `README.md`. On macOS or Linux, download the archive and the checksums, verify, and put the binary on your `PATH`:

```sh
target=aarch64-apple-darwin  # or x86_64-unknown-linux-gnu
base=https://github.com/shuymn/cardinal/releases/latest/download
curl -fL -O "$base/cardinal-$target.tar.gz" -O "$base/SHA256SUMS"
shasum -a 256 --check --ignore-missing SHA256SUMS
tar -xzf "cardinal-$target.tar.gz"
install -m 755 "cardinal-$target/cardinal" ~/.local/bin/cardinal
cardinal --version
```

On Windows, compare `Get-FileHash cardinal-x86_64-pc-windows-msvc.zip` with its line in `SHA256SUMS`, then extract `cardinal.exe` to a directory on your `PATH`.

Install to a path that stays the same across upgrades: the Claude Code hook's `[hook] runner` setting names the binary by path, and a path that disappears makes the hook fall back to running commands unarbitrated (`docs/architecture.md` section 6.2).
