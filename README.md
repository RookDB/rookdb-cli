# RookDB CLI

An interactive SQL REPL for [RookDB](https://rookdb.github.io) — type SQL statements directly in your terminal.

---

## Installation

### macOS — Homebrew

```sh
brew install RookDB/tap/rookdb-cli
```

### Linux — Shell Installer

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/RookDB/rookdb-cli/releases/latest/download/rookdb-installer.sh \
  | sh
```

### Windows — PowerShell

```powershell
irm https://github.com/RookDB/rookdb-cli/releases/latest/download/rookdb-installer.ps1 | iex
```

Or download the `.msi` installer directly from the [Releases page](https://github.com/RookDB/rookdb-cli/releases).

### Build from source

Requires [Rust](https://www.rust-lang.org/tools/install).

```sh
cargo install rookdb-cli
```

---

## Usage

```sh
rookdb
```

```
--------------------------------------
Welcome to RookDB
--------------------------------------

> CREATE DATABASE mydb;
> USE mydb;
> CREATE TABLE users (id INT NOT NULL, name VARCHAR(100));
> INSERT INTO users VALUES (1, 'Alice');
> SELECT * FROM users;
> exit
```

For the full user guide see the [documentation](https://rookdb.github.io/docs/CLI).

---

## Releasing a new version

1. Bump the version in `Cargo.toml`
2. Commit and tag:

```sh
git commit -am "release: v0.2.0"
git tag v0.2.0
git push && git push --tags
```

The GitHub Actions release workflow automatically builds cross-platform binaries,
generates all installers, publishes a GitHub Release, and updates the Homebrew formula.

---

## License

Apache-2.0 — see [LICENSE](LICENSE).