# grumpy-repl

Interactive REPL shell for [GrumpyDB](https://crates.io/crates/grumpydb).

`grumpy-repl` is a dual-mode command-line shell:

- **Connected mode** (default): connects to a running `grumpydb-server` over TCP (TLS optional) and authenticates with username/password or JWT.
- **Embedded mode** (`--embedded`): opens a local on-disk GrumpyDB directly, no server needed.

It supports the full GrumpyDB command surface: `LOGIN`, `USE`, `CREATE COLLECTION`, `INSERT`, `GET`, `UPDATE`, `DELETE`, `SCAN`, `COUNT`, `CREATE INDEX`, `FIND`, JSON literals, filter expressions, and admin commands.

## Install

```sh
cargo install grumpy-repl
```

## Usage

Connected mode (against a `grumpydb-server`):

```sh
grumpy-repl --host 127.0.0.1 --port 6543 --user admin@default --password <pwd>
```

Embedded mode (no server, local files):

```sh
grumpy-repl --embedded --data-dir ./mydata
```

Run `grumpy-repl --help` for the full list of flags.

History is persisted to `~/.grumpy_repl_history`. The default embedded data directory is `.grumpy_repl_data/`.

## License

MIT — see the [LICENSE](https://github.com/pierreg256/grumpydb/blob/master/LICENSE) file in the parent repository.
